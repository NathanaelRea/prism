use std::fmt;
use std::path::{Path, PathBuf};

use sqlx::{Connection, SqliteConnection};

use super::database::{block_on, writable_options};
use super::error::DatabaseError;
use crate::execution::{DispatchState, WorkflowIdentity};

#[derive(Debug)]
pub(crate) enum WorkflowError {
    Database(DatabaseError),
    InvalidDispatchState(String),
    CannotQueue,
    StaleClaim,
}

impl fmt::Display for WorkflowError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => error.fmt(formatter),
            Self::InvalidDispatchState(state) => {
                write!(formatter, "unknown dispatch state: {state}")
            }
            Self::CannotQueue => formatter.write_str("workflow could not be queued"),
            Self::StaleClaim => formatter.write_str("execution claim is stale"),
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
