use std::fmt;
use std::path::{Path, PathBuf};

use sqlx::{Connection, SqliteConnection};

use super::database::{block_on, writable_options};
use super::error::DatabaseError;
use crate::execution::{DispatchState, WorkflowIdentity, WorkflowKind};

#[derive(Debug)]
pub(crate) enum WorkflowError {
    Database(DatabaseError),
    InvalidKind(String),
    InvalidDispatchState(String),
    CannotQueue,
    StaleClaim,
    StaleRecovery { kind: WorkflowKind, run_id: String },
    Process(String),
}

impl fmt::Display for WorkflowError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => error.fmt(formatter),
            Self::InvalidKind(kind) => write!(formatter, "unknown workflow kind: {kind}"),
            Self::InvalidDispatchState(state) => {
                write!(formatter, "unknown dispatch state: {state}")
            }
            Self::CannotQueue => formatter.write_str("workflow could not be queued"),
            Self::StaleClaim => formatter.write_str("execution claim is stale"),
            Self::StaleRecovery { kind, run_id } => write!(
                formatter,
                "stale recovery generation for {} run {run_id}",
                kind.label()
            ),
            Self::Process(error) => formatter.write_str(error),
        }
    }
}

impl From<DatabaseError> for WorkflowError {
    fn from(error: DatabaseError) -> Self {
        match error {
            DatabaseError::StaleClaim => Self::StaleClaim,
            error => Self::Database(error),
        }
    }
}

#[derive(Debug)]
struct ProcessRow {
    process_id: i64,
    start_time_ticks: Option<i64>,
}

pub(crate) struct WorkerEvent<'a> {
    pub time: i64,
    pub action: &'a str,
    pub repo: &'a str,
    pub message: &'a str,
    pub data_json: Option<&'a str>,
}

pub(crate) struct WorkflowStore {
    path: PathBuf,
}

impl WorkflowStore {
    pub(crate) fn open(path: &Path) -> Result<Self, WorkflowError> {
        super::database::initialize(path)?;
        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    fn connection_options(&self) -> Result<sqlx::sqlite::SqliteConnectOptions, WorkflowError> {
        writable_options(&self.path, false).map_err(Into::into)
    }

    pub(crate) fn enqueue(
        &self,
        workflow: &WorkflowIdentity,
        now: i64,
    ) -> Result<(), WorkflowError> {
        let options = self.connection_options()?;
        let kind = workflow.kind.label();
        let changed = block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            immediate(&mut connection, async |connection| {
                let changed =
                    sqlx::query_file!("sql/workflow/enqueue.sql", kind, workflow.run_id, now, now)
                        .execute(&mut *connection)
                        .await?
                        .rows_affected();
                if changed == 0 {
                    let requested = sqlx::query_file!(
                        "sql/workflow/request_requeue.sql",
                        now,
                        kind,
                        workflow.run_id
                    )
                    .execute(connection)
                    .await?
                    .rows_affected();
                    Ok(requested)
                } else {
                    Ok(1)
                }
            })
            .await
        })?;
        if changed == 1 {
            Ok(())
        } else {
            Err(WorkflowError::CannotQueue)
        }
    }

    pub(crate) fn dispatch_state(
        &self,
        workflow: &WorkflowIdentity,
    ) -> Result<Option<DispatchState>, WorkflowError> {
        let options = self.connection_options()?;
        let kind = workflow.kind.label();
        let state = block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            sqlx::query_file_scalar!(
                "sql/workflow/load_dispatch_state.sql",
                kind,
                workflow.run_id
            )
            .fetch_optional(&mut connection)
            .await
        })?;
        state
            .map(|state| {
                DispatchState::parse(&state).map_err(|_| WorkflowError::InvalidDispatchState(state))
            })
            .transpose()
    }

    pub(crate) fn mark_abandoned(&self, daemon: &str, now: i64) -> Result<usize, WorkflowError> {
        let options = self.connection_options()?;
        let changed = block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            immediate(&mut connection, async |connection| {
                Ok(
                    sqlx::query_file!("sql/workflow/mark_abandoned.sql", now, daemon, now)
                        .execute(connection)
                        .await?
                        .rows_affected(),
                )
            })
            .await
        })?;
        usize::try_from(changed).map_err(|_| {
            WorkflowError::InvalidDispatchState("affected row count out of range".into())
        })
    }

    pub(crate) fn apply_recovery_decision(
        &self,
        decisions: &[(WorkflowIdentity, i64, bool)],
        now: i64,
    ) -> Result<(), WorkflowError> {
        let processes = self.recovery_processes(decisions)?;
        for (workflow, process) in processes {
            terminate_process(&workflow, process)?;
        }
        let options = self.connection_options()?;
        let stale = block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            immediate(&mut connection, async |connection| {
                for (workflow, generation, _) in decisions {
                    if !recovery_is_current(connection, workflow, *generation).await? {
                        return Ok(Some((
                            workflow.kind.label().to_string(),
                            workflow.run_id.clone(),
                        )));
                    }
                }
                for (workflow, generation, selected) in decisions {
                    let state = if *selected { "queued" } else { "paused" };
                    let kind = workflow.kind.label();
                    sqlx::query_file!(
                        "sql/workflow/apply_recovery.sql",
                        state,
                        now,
                        now,
                        kind,
                        workflow.run_id,
                        generation
                    )
                    .execute(&mut *connection)
                    .await?;
                    match workflow.kind {
                        WorkflowKind::Auto => {
                            if *selected {
                                sqlx::query_file!(
                                    "sql/workflow/reset_auto_steps.sql",
                                    workflow.run_id
                                )
                                .execute(&mut *connection)
                                .await?;
                                sqlx::query_file!(
                                    "sql/workflow/reset_auto_linked_plan_steps.sql",
                                    workflow.run_id
                                )
                                .execute(&mut *connection)
                                .await?;
                                sqlx::query_file!(
                                    "sql/workflow/queue_auto_linked_plan_runs.sql",
                                    now,
                                    workflow.run_id
                                )
                                .execute(&mut *connection)
                                .await?;
                            }
                            sqlx::query_file!(
                                "sql/workflow/update_auto_recovered.sql",
                                state,
                                now,
                                workflow.run_id
                            )
                            .execute(&mut *connection)
                            .await?;
                        }
                        WorkflowKind::Plan => {
                            if *selected {
                                sqlx::query_file!(
                                    "sql/workflow/reset_plan_steps.sql",
                                    workflow.run_id
                                )
                                .execute(&mut *connection)
                                .await?;
                            }
                            sqlx::query_file!(
                                "sql/workflow/update_plan_recovered.sql",
                                state,
                                now,
                                workflow.run_id
                            )
                            .execute(&mut *connection)
                            .await?;
                        }
                    }
                }
                Ok(None)
            })
            .await
        })?;
        match stale {
            Some((kind, run_id)) => Err(WorkflowError::StaleRecovery {
                kind: WorkflowKind::parse(&kind).map_err(|_| WorkflowError::InvalidKind(kind))?,
                run_id,
            }),
            None => Ok(()),
        }
    }

    fn recovery_processes(
        &self,
        decisions: &[(WorkflowIdentity, i64, bool)],
    ) -> Result<Vec<(WorkflowIdentity, ProcessRow)>, WorkflowError> {
        let options = self.connection_options()?;
        let result = block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            immediate(&mut connection, async |connection| {
                let mut result = Vec::new();
                for (workflow, generation, _) in decisions {
                    if !recovery_is_current(connection, workflow, *generation).await? {
                        return Ok(Err((
                            workflow.kind.label().to_string(),
                            workflow.run_id.clone(),
                        )));
                    }
                    let rows = match workflow.kind {
                        WorkflowKind::Auto => {
                            sqlx::query_file_as!(
                                ProcessRow,
                                "sql/workflow/list_auto_processes.sql",
                                workflow.run_id,
                                workflow.run_id
                            )
                            .fetch_all(&mut *connection)
                            .await?
                        }
                        WorkflowKind::Plan => {
                            sqlx::query_file_as!(
                                ProcessRow,
                                "sql/workflow/list_plan_processes.sql",
                                workflow.run_id
                            )
                            .fetch_all(&mut *connection)
                            .await?
                        }
                    };
                    result.extend(rows.into_iter().map(|row| (workflow.clone(), row)));
                }
                Ok(Ok(result))
            })
            .await
        })?;
        result.map_err(|(kind, run_id)| WorkflowError::StaleRecovery {
            kind: WorkflowKind::parse(&kind).unwrap_or(WorkflowKind::Plan),
            run_id,
        })
    }

    pub(crate) fn insert_worker_event(&self, event: WorkerEvent<'_>) -> Result<(), WorkflowError> {
        let options = self.connection_options()?;
        block_on(async {
            let mut connection = SqliteConnection::connect_with(&options).await?;
            sqlx::query_file!(
                "sql/workflow/insert_worker_event.sql",
                event.time,
                event.action,
                event.repo,
                event.message,
                event.data_json
            )
            .execute(&mut connection)
            .await?;
            Ok(())
        })?;
        Ok(())
    }
}

async fn recovery_is_current(
    connection: &mut SqliteConnection,
    workflow: &WorkflowIdentity,
    generation: i64,
) -> Result<bool, sqlx::Error> {
    let kind = workflow.kind.label();
    sqlx::query_file_scalar!(
        "sql/workflow/validate_recovery.sql",
        kind,
        workflow.run_id,
        generation
    )
    .fetch_one(connection)
    .await
}

async fn immediate<T>(
    connection: &mut SqliteConnection,
    operation: impl AsyncFnOnce(&mut SqliteConnection) -> Result<T, sqlx::Error>,
) -> Result<T, sqlx::Error> {
    super::database::begin_immediate_query()
        .execute(&mut *connection)
        .await?;
    let result = operation(&mut *connection).await;
    match result {
        Ok(value) => {
            super::database::commit_query().execute(connection).await?;
            Ok(value)
        }
        Err(error) => {
            let _ = super::database::rollback_query().execute(connection).await;
            Err(error)
        }
    }
}

fn terminate_process(
    workflow: &WorkflowIdentity,
    process: ProcessRow,
) -> Result<(), WorkflowError> {
    let Ok(process_id) = u32::try_from(process.process_id) else {
        return Ok(());
    };
    let identity = process
        .start_time_ticks
        .and_then(|value| u64::try_from(value).ok());
    let recorded = crate::process::RecordedProcess::from_stored(process_id, identity);
    let outcome =
        crate::process::terminate_recorded_process(recorded, std::time::Duration::from_secs(1))
            .map_err(|error| {
                WorkflowError::Process(format!(
                    "terminate interrupted {} run {} process {process_id}: {error}",
                    workflow.kind.label(),
                    workflow.run_id
                ))
            })?;
    if outcome == crate::process::TerminationOutcome::Unverifiable {
        return Err(WorkflowError::Process(format!(
            "interrupted {} run {} is blocked by live process {process_id} without a reusable process identity",
            workflow.kind.label(),
            workflow.run_id
        )));
    }
    Ok(())
}
