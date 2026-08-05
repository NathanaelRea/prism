use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{Connection, FromRow, SqliteConnection};

#[derive(Clone, Debug, FromRow)]
pub(crate) struct AgentRow {
    pub state: String,
    pub updated_unix_ms: i64,
}

#[derive(Clone, Debug, FromRow)]
pub(crate) struct PullRequestRow {
    pub number: i64,
    pub title: String,
    pub url: String,
    pub state: String,
    pub merge_state_status: Option<String>,
    pub check_status: Option<String>,
    pub refreshed_unix_ms: i64,
    pub merged: i64,
    pub draft: i64,
    pub observation_error: Option<String>,
}

#[derive(Clone, Debug, FromRow)]
pub(crate) struct WorkflowRow {
    pub kind: String,
    pub run_id: String,
    pub worktree_path: String,
    pub lifecycle: String,
    pub pause_requested: i64,
    pub updated_unix_ms: i64,
    pub dispatch_state: Option<String>,
    pub daemon_instance_id: Option<String>,
    pub worker_id: Option<String>,
    pub lease_expires_unix_ms: Option<i64>,
    pub heartbeat_unix_ms: Option<i64>,
    pub interruption_generation: i64,
    pub dispatch_updated_unix_ms: Option<i64>,
    pub current_step: Option<String>,
    pub current_step_state: Option<String>,
    pub completed: i64,
    pub total: i64,
}

#[derive(Clone, Debug, FromRow)]
pub(crate) struct LinkedPlanOwnerRow {
    pub plan_run_id: String,
    pub auto_run_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ControlKind {
    Auto,
    Plan,
}

impl ControlKind {
    fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Plan => "plan",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ControlAction {
    Pause,
    Resume,
    Stop,
}

pub(crate) struct ControlInput<'a> {
    pub kind: ControlKind,
    pub run_id: &'a str,
    pub lifecycle: &'a str,
    pub pause_requested: bool,
    pub updated_unix_ms: i64,
    pub dispatch_state: Option<&'a str>,
    pub interruption_generation: i64,
    pub action: ControlAction,
    pub now: i64,
}

pub(crate) struct ControlOutput {
    pub state: String,
    pub processes: Vec<(i64, Option<i64>)>,
    pub sessions: Vec<crate::harness::SessionRef>,
}

#[derive(FromRow)]
struct ControlSnapshotRow {
    status: String,
    pause_requested: i64,
    updated_unix_ms: i64,
    dispatch_state: Option<String>,
    interruption_generation: i64,
}

#[derive(FromRow)]
struct CancellationProcessRow {
    process_id: i64,
    process_identity: Option<i64>,
}

#[derive(FromRow)]
struct CancellationSessionRow {
    adapter_id: Option<String>,
    endpoint: Option<String>,
    id: String,
}

#[derive(FromRow)]
struct HiddenRow {
    branch: String,
}

pub(crate) struct WorkspaceReader {
    path: PathBuf,
}

impl WorkspaceReader {
    pub(crate) fn open(path: &Path) -> Result<Self, String> {
        if !path.exists() {
            return Err(format!("database does not exist: {}", path.display()));
        }
        Ok(Self { path: path.into() })
    }

    pub(crate) fn hidden(&self) -> Result<Vec<String>, String> {
        self.run(async |connection| {
            let rows = sqlx::query_file_as!(HiddenRow, "sql/workspace/load_hidden.sql")
                .fetch_all(connection)
                .await?;
            Ok(rows.into_iter().map(|row| row.branch).collect())
        })
    }

    pub(crate) fn agent(&self, branch: &str) -> Result<Option<AgentRow>, String> {
        self.run(async |connection| {
            sqlx::query_file_as!(AgentRow, "sql/workspace/load_agent.sql", branch)
                .fetch_optional(connection)
                .await
        })
    }

    pub(crate) fn pull_request(&self, branch: &str) -> Result<Option<PullRequestRow>, String> {
        self.run(async |connection| {
            sqlx::query_file_as!(
                PullRequestRow,
                "sql/workspace/load_pull_request.sql",
                branch
            )
            .fetch_optional(connection)
            .await
        })
    }

    pub(crate) fn workflows(
        &self,
        repo_root: &str,
        include_terminal: bool,
    ) -> Result<Vec<WorkflowRow>, String> {
        self.run(async |connection| {
            if include_terminal {
                sqlx::query_file_as!(
                    WorkflowRow,
                    "sql/workspace/load_workflows.sql",
                    repo_root,
                    repo_root
                )
                .fetch_all(connection)
                .await
            } else {
                sqlx::query_file_as!(
                    WorkflowRow,
                    "sql/workspace/load_active_workflows.sql",
                    repo_root,
                    repo_root
                )
                .fetch_all(connection)
                .await
            }
        })
    }

    pub(crate) fn linked_plan_owners(&self) -> Result<Vec<LinkedPlanOwnerRow>, String> {
        self.run(async |connection| {
            sqlx::query_file_as!(
                LinkedPlanOwnerRow,
                "sql/workspace/load_linked_plan_owners.sql"
            )
            .fetch_all(connection)
            .await
        })
    }

    fn run<T, F>(&self, operation: F) -> Result<T, String>
    where
        F: for<'a> AsyncFnOnce(&'a mut SqliteConnection) -> Result<T, sqlx::Error>,
    {
        let options = readonly_options(&self.path)?;
        crate::async_runtime::block_on(async {
            let mut connection = SqliteConnection::connect_with(&options)
                .await
                .map_err(|error| format!("open workspace database: {error}"))?;
            // This fixed PRAGMA is connection policy and is not representable by a SQLx macro.
            // SQLX_RUNTIME_SQL: SQLite connection policy PRAGMAs are runtime-only statements.
            sqlx::query("pragma query_only = on")
                .execute(&mut connection)
                .await
                .map_err(|error| format!("configure workspace database: {error}"))?;
            operation(&mut connection)
                .await
                .map_err(|error| format!("read workspace projection: {error}"))
        })
        .map_err(|error| format!("access workspace application runtime: {error}"))?
    }
}

pub(crate) fn apply_control(
    path: &Path,
    input: &ControlInput<'_>,
) -> Result<ControlOutput, String> {
    crate::persistence::database::initialize(path).map_err(|error| error.to_string())?;
    let options =
        super::database::writable_options(path, false).map_err(|error| error.to_string())?;
    crate::async_runtime::block_on(async {
        let mut connection = SqliteConnection::connect_with(&options)
            .await
            .map_err(|error| format!("open workspace control database: {error}"))?;
        super::database::begin_immediate_query()
            .execute(&mut connection)
            .await
            .map_err(|error| format!("begin workspace control: {error}"))?;
        let result = apply_control_inner(&mut connection, input).await;
        match result {
            Ok(output) => {
                super::database::commit_query()
                    .execute(&mut connection)
                    .await
                    .map_err(|error| format!("commit workspace control: {error}"))?;
                Ok(output)
            }
            Err(error) => {
                let _ = super::database::rollback_query()
                    .execute(&mut connection)
                    .await;
                Err(error)
            }
        }
    })
    .map_err(|error| format!("access workspace control runtime: {error}"))?
}

async fn apply_control_inner(
    connection: &mut SqliteConnection,
    input: &ControlInput<'_>,
) -> Result<ControlOutput, String> {
    let current = load_control_snapshot(connection, input.kind, input.run_id).await?;
    let expected_dispatch = input.dispatch_state.map(str::to_string);
    if current.as_ref().is_none_or(|row| {
        row.status != input.lifecycle
            || (row.pause_requested != 0) != input.pause_requested
            || row.updated_unix_ms != input.updated_unix_ms
            || row.dispatch_state != expected_dispatch
            || row.interruption_generation != input.interruption_generation
    }) {
        return Err("workflow changed while applying control; inspect it again".to_string());
    }

    let mut output = ControlOutput {
        state: String::new(),
        processes: Vec::new(),
        sessions: Vec::new(),
    };
    match input.action {
        ControlAction::Pause => {
            let changed = match input.kind {
                ControlKind::Auto => {
                    let changed = sqlx::query_file!(
                        "sql/workspace/pause_auto_run.sql",
                        input.run_id,
                        input.now,
                        input.run_id
                    )
                    .execute(&mut *connection)
                    .await
                    .map_err(query_error)?
                    .rows_affected();
                    sqlx::query_file!(
                        "sql/workspace/pause_linked_plan_runs.sql",
                        input.now,
                        input.run_id
                    )
                    .execute(&mut *connection)
                    .await
                    .map_err(query_error)?;
                    changed
                }
                ControlKind::Plan => sqlx::query_file!(
                    "sql/workspace/pause_plan_run.sql",
                    input.run_id,
                    input.now,
                    input.run_id
                )
                .execute(&mut *connection)
                .await
                .map_err(query_error)?
                .rows_affected(),
            };
            if changed != 1 {
                return Err("workflow cannot be paused from its current state".to_string());
            }
            let paused = load_control_snapshot(connection, input.kind, input.run_id)
                .await?
                .is_some_and(|row| row.status == "paused");
            output.state = if paused { "paused" } else { "pause_requested" }.to_string();
            if paused {
                set_dispatch(connection, input, "paused").await?;
            }
        }
        ControlAction::Resume => {
            if !input.pause_requested
                && input.lifecycle != "paused"
                && input.dispatch_state != Some("paused")
            {
                return Err("workflow is not paused".to_string());
            }
            if !input.pause_requested && input.lifecycle != "paused" {
                match input.kind {
                    ControlKind::Auto => {
                        sqlx::query_file!(
                            "sql/workspace/adapt_auto_dispatch_pause.sql",
                            input.run_id
                        )
                        .execute(&mut *connection)
                        .await
                        .map_err(query_error)?;
                    }
                    ControlKind::Plan => {
                        sqlx::query_file!(
                            "sql/workspace/adapt_plan_dispatch_pause.sql",
                            input.run_id
                        )
                        .execute(&mut *connection)
                        .await
                        .map_err(query_error)?;
                    }
                }
            }
            if input.dispatch_state == Some("claimed") {
                let changed = match input.kind {
                    ControlKind::Auto => {
                        let changed = sqlx::query_file!(
                            "sql/workspace/resume_claimed_auto_run.sql",
                            input.now,
                            input.run_id
                        )
                        .execute(&mut *connection)
                        .await
                        .map_err(query_error)?
                        .rows_affected();
                        sqlx::query_file!(
                            "sql/workspace/resume_linked_plan_runs.sql",
                            input.now,
                            input.run_id
                        )
                        .execute(&mut *connection)
                        .await
                        .map_err(query_error)?;
                        changed
                    }
                    ControlKind::Plan => sqlx::query_file!(
                        "sql/workspace/resume_claimed_plan_run.sql",
                        input.now,
                        input.run_id
                    )
                    .execute(&mut *connection)
                    .await
                    .map_err(query_error)?
                    .rows_affected(),
                };
                if changed != 1 {
                    return Err("workflow changed while applying resume".to_string());
                }
                output.state = "running".to_string();
            } else {
                let changed = match input.kind {
                    ControlKind::Auto => {
                        let changed = sqlx::query_file!(
                            "sql/workspace/resume_auto_run.sql",
                            input.run_id,
                            input.now,
                            input.run_id
                        )
                        .execute(&mut *connection)
                        .await
                        .map_err(query_error)?
                        .rows_affected();
                        sqlx::query_file!(
                            "sql/workspace/resume_linked_plan_runs.sql",
                            input.now,
                            input.run_id
                        )
                        .execute(&mut *connection)
                        .await
                        .map_err(query_error)?;
                        changed
                    }
                    ControlKind::Plan => sqlx::query_file!(
                        "sql/workspace/resume_plan_run.sql",
                        input.run_id,
                        input.now,
                        input.run_id
                    )
                    .execute(&mut *connection)
                    .await
                    .map_err(query_error)?
                    .rows_affected(),
                };
                if changed != 1 {
                    return Err("workflow changed while applying resume".to_string());
                }
                let running = load_control_snapshot(connection, input.kind, input.run_id)
                    .await?
                    .is_some_and(|row| row.status == "running");
                if running {
                    output.state = "running".to_string();
                } else {
                    enqueue_dispatch(connection, input).await?;
                    output.state = "queued".to_string();
                }
            }
        }
        ControlAction::Stop => {
            let (processes, sessions) = load_cancellation(connection, input).await?;
            output.processes = processes;
            output.sessions = sessions;
            let changed = match input.kind {
                ControlKind::Auto => {
                    sqlx::query_file!(
                        "sql/workspace/abort_auto_steps.sql",
                        input.now,
                        input.run_id
                    )
                    .execute(&mut *connection)
                    .await
                    .map_err(query_error)?;
                    sqlx::query_file!(
                        "sql/workspace/abort_linked_plan_steps.sql",
                        input.now,
                        input.run_id
                    )
                    .execute(&mut *connection)
                    .await
                    .map_err(query_error)?;
                    sqlx::query_file!(
                        "sql/workspace/abort_linked_plan_runs.sql",
                        input.now,
                        input.run_id
                    )
                    .execute(&mut *connection)
                    .await
                    .map_err(query_error)?;
                    sqlx::query_file!("sql/workspace/abort_auto_run.sql", input.now, input.run_id)
                        .execute(&mut *connection)
                        .await
                        .map_err(query_error)?
                        .rows_affected()
                }
                ControlKind::Plan => {
                    sqlx::query_file!(
                        "sql/workspace/abort_plan_steps.sql",
                        input.now,
                        input.run_id
                    )
                    .execute(&mut *connection)
                    .await
                    .map_err(query_error)?;
                    sqlx::query_file!("sql/workspace/abort_plan_run.sql", input.now, input.run_id)
                        .execute(&mut *connection)
                        .await
                        .map_err(query_error)?
                        .rows_affected()
                }
            };
            if changed != 1 {
                return Err("workflow cannot be stopped from its current state".to_string());
            }
            set_dispatch(connection, input, "terminal").await?;
            output.state = "aborted".to_string();
        }
    }
    Ok(output)
}

async fn load_control_snapshot(
    connection: &mut SqliteConnection,
    kind: ControlKind,
    run_id: &str,
) -> Result<Option<ControlSnapshotRow>, String> {
    match kind {
        ControlKind::Auto => sqlx::query_file_as!(
            ControlSnapshotRow,
            "sql/workspace/validate_auto_control.sql",
            run_id
        )
        .fetch_optional(connection)
        .await
        .map_err(query_error),
        ControlKind::Plan => sqlx::query_file_as!(
            ControlSnapshotRow,
            "sql/workspace/validate_plan_control.sql",
            run_id
        )
        .fetch_optional(connection)
        .await
        .map_err(query_error),
    }
}

async fn set_dispatch(
    connection: &mut SqliteConnection,
    input: &ControlInput<'_>,
    state: &str,
) -> Result<(), String> {
    let kind = input.kind.label();
    sqlx::query_file!(
        "sql/workspace/set_dispatch.sql",
        state,
        input.now,
        kind,
        input.run_id
    )
    .execute(connection)
    .await
    .map_err(query_error)?;
    Ok(())
}

async fn enqueue_dispatch(
    connection: &mut SqliteConnection,
    input: &ControlInput<'_>,
) -> Result<(), String> {
    let kind = input.kind.label();
    sqlx::query_file!(
        "sql/workspace/enqueue_dispatch.sql",
        kind,
        input.run_id,
        input.now,
        input.now
    )
    .execute(connection)
    .await
    .map_err(query_error)?;
    Ok(())
}

async fn load_cancellation(
    connection: &mut SqliteConnection,
    input: &ControlInput<'_>,
) -> Result<(Vec<(i64, Option<i64>)>, Vec<crate::harness::SessionRef>), String> {
    let (processes, sessions) = match input.kind {
        ControlKind::Auto => {
            let processes = sqlx::query_file_as!(
                CancellationProcessRow,
                "sql/workspace/load_auto_cancellation_processes.sql",
                input.run_id,
                input.run_id
            )
            .fetch_all(&mut *connection)
            .await
            .map_err(query_error)?;
            let sessions = sqlx::query_file_as!(
                CancellationSessionRow,
                "sql/workspace/load_auto_cancellation_sessions.sql",
                input.run_id,
                input.run_id
            )
            .fetch_all(&mut *connection)
            .await
            .map_err(query_error)?;
            (processes, sessions)
        }
        ControlKind::Plan => {
            let processes = sqlx::query_file_as!(
                CancellationProcessRow,
                "sql/workspace/load_plan_cancellation_processes.sql",
                input.run_id
            )
            .fetch_all(&mut *connection)
            .await
            .map_err(query_error)?;
            let sessions = sqlx::query_file_as!(
                CancellationSessionRow,
                "sql/workspace/load_plan_cancellation_sessions.sql",
                input.run_id
            )
            .fetch_all(&mut *connection)
            .await
            .map_err(query_error)?;
            (processes, sessions)
        }
    };
    Ok((
        processes
            .into_iter()
            .map(|row| (row.process_id, row.process_identity))
            .collect(),
        sessions
            .into_iter()
            .map(|row| crate::harness::SessionRef {
                adapter_id: row.adapter_id,
                endpoint: row.endpoint,
                id: Some(row.id),
            })
            .collect(),
    ))
}

fn query_error(error: sqlx::Error) -> String {
    format!("workspace control: {error}")
}

fn readonly_options(path: &Path) -> Result<SqliteConnectOptions, String> {
    SqliteConnectOptions::from_str(&path.to_string_lossy())
        .map_err(|error| format!("open workspace database {}: {error}", path.display()))
        .map(|options| {
            options
                .read_only(true)
                .create_if_missing(false)
                .foreign_keys(true)
                .busy_timeout(Duration::from_millis(50))
        })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_PATH: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn workspace_reader_projects_named_rows_from_a_file_database() {
        let path = std::env::temp_dir().join(format!(
            "prism-workspace-interface-{}-{}.db",
            std::process::id(),
            NEXT_PATH.fetch_add(1, Ordering::Relaxed)
        ));
        let store = crate::persistence::session::SessionStore::open(&path).unwrap();
        store
            .archive(&crate::persistence::session::ArchiveInput {
                branch: "feature/workspace-reader",
                repo_root: "/tmp/repo",
                worktree_path: "/tmp/repo-worktree",
                archived_unix_ms: 1,
                classification: "work",
            })
            .unwrap();
        store
            .save_agent_state("feature/workspace-reader", "running", 2)
            .unwrap();

        let reader = WorkspaceReader::open(&path).unwrap();
        assert_eq!(reader.hidden().unwrap(), ["feature/workspace-reader"]);
        let agent = reader.agent("feature/workspace-reader").unwrap().unwrap();
        assert_eq!(agent.state, "running");
        assert_eq!(agent.updated_unix_ms, 2);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    }
}
