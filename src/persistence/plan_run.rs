use std::path::{Path, PathBuf};

use sqlx::{Connection, SqliteConnection};

use super::database::{block_on, writable_options};
use super::error::DatabaseError;
use crate::plan_run::{
    PersistedPlanRun, PlanLaunch, PlanOutputKind, PlanOutputLine, PlanRun, PlanRunMode,
    PlanRunStatus, PlanStepRun, PlanStepStatus, PlanTodo,
};

#[derive(Debug)]
struct RunRow {
    id: String,
    harness_id: String,
    adapter_id: String,
    repo_root: String,
    scope_path: String,
    plan_path: String,
    plan_display: String,
    step_name: String,
    start_step: i64,
    total_steps: i64,
    mode: String,
    status: String,
    pause_requested: i64,
    selected_step: i64,
    created_unix_ms: i64,
    updated_unix_ms: i64,
    archived_unix_ms: Option<i64>,
}

#[derive(Debug)]
struct StepRow {
    run_id: String,
    step: i64,
    prompt: String,
    status: String,
    execution_state: Option<String>,
    session_endpoint: Option<String>,
    session_id: Option<String>,
    agent_variant: Option<String>,
    execution_process_id: Option<i64>,
    started_unix_ms: Option<i64>,
    finished_unix_ms: Option<i64>,
    exit_code: Option<i64>,
    latest_message: Option<String>,
    active_tool: Option<String>,
    todos_json: String,
    summary: Option<String>,
    error: Option<String>,
    session_adapter_id: Option<String>,
    execution_process_start_time_ticks: Option<i64>,
}

#[derive(Debug)]
struct OutputRow {
    run_id: String,
    step: i64,
    line_number: i64,
    time_unix_ms: i64,
    kind: String,
    text: String,
    block_id: Option<String>,
}

fn invalid(field: &'static str, value: impl ToString) -> DatabaseError {
    DatabaseError::InvalidValue {
        field,
        value: value.to_string(),
    }
}

fn from_i64<T: TryFrom<i64>>(field: &'static str, value: i64) -> Result<T, DatabaseError> {
    value.try_into().map_err(|_| invalid(field, value))
}

fn to_i64<T: TryInto<i64> + Copy>(field: &'static str, value: T) -> Result<i64, DatabaseError> {
    value.try_into().map_err(|_| invalid(field, "out of range"))
}

impl TryFrom<RunRow> for PlanRun {
    type Error = DatabaseError;

    fn try_from(row: RunRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row.id,
            harness_id: row.harness_id,
            adapter_id: row.adapter_id,
            repo_root: row.repo_root,
            scope_path: PathBuf::from(row.scope_path),
            plan_path: PathBuf::from(row.plan_path),
            plan_display: row.plan_display,
            step_name: row.step_name,
            start_step: from_i64("plan_run.start_step", row.start_step)?,
            total_steps: from_i64("plan_run.total_steps", row.total_steps)?,
            mode: PlanRunMode::parse(&row.mode).map_err(|_| invalid("plan_run.mode", row.mode))?,
            status: PlanRunStatus::parse(&row.status)
                .map_err(|_| invalid("plan_run.status", row.status))?,
            pause_requested: match row.pause_requested {
                0 => false,
                1 => true,
                value => return Err(invalid("plan_run.pause_requested", value)),
            },
            selected_step: from_i64("plan_run.selected_step", row.selected_step)?,
            created_unix_ms: from_i64("plan_run.created_unix_ms", row.created_unix_ms)?,
            updated_unix_ms: from_i64("plan_run.updated_unix_ms", row.updated_unix_ms)?,
            archived_unix_ms: row
                .archived_unix_ms
                .map(|value| from_i64("plan_run.archived_unix_ms", value))
                .transpose()?,
        })
    }
}

impl TryFrom<StepRow> for PlanStepRun {
    type Error = DatabaseError;

    fn try_from(row: StepRow) -> Result<Self, Self::Error> {
        let todos = parse_todos(&row.todos_json)?;
        Ok(Self {
            run_id: row.run_id,
            step: from_i64("plan_step_run.step", row.step)?,
            prompt: row.prompt,
            status: PlanStepStatus::parse(&row.status)
                .map_err(|_| invalid("plan_step_run.status", row.status))?,
            execution: crate::harness::ExecutionRef {
                state: row.execution_state,
                process_id: row
                    .execution_process_id
                    .map(|value| from_i64("plan_step_run.execution_process_id", value))
                    .transpose()?,
                process_identity: row
                    .execution_process_start_time_ticks
                    .map(|value| {
                        from_i64("plan_step_run.execution_process_start_time_ticks", value)
                    })
                    .transpose()?,
            },
            session: crate::harness::SessionRef {
                adapter_id: row.session_adapter_id,
                endpoint: row.session_endpoint,
                id: row.session_id,
            },
            agent_variant: row.agent_variant,
            started_unix_ms: row
                .started_unix_ms
                .map(|value| from_i64("plan_step_run.started_unix_ms", value))
                .transpose()?,
            finished_unix_ms: row
                .finished_unix_ms
                .map(|value| from_i64("plan_step_run.finished_unix_ms", value))
                .transpose()?,
            exit_code: row
                .exit_code
                .map(|value| from_i64("plan_step_run.exit_code", value))
                .transpose()?,
            latest_message: row.latest_message,
            active_tool: row.active_tool,
            todos,
            summary: row.summary,
            error: row.error,
        })
    }
}

impl TryFrom<OutputRow> for PlanOutputLine {
    type Error = DatabaseError;

    fn try_from(row: OutputRow) -> Result<Self, Self::Error> {
        Ok(Self {
            run_id: row.run_id,
            step: from_i64("plan_output_line.step", row.step)?,
            line_number: from_i64("plan_output_line.line_number", row.line_number)?,
            time_unix_ms: from_i64("plan_output_line.time_unix_ms", row.time_unix_ms)?,
            kind: PlanOutputKind::parse(&row.kind)
                .map_err(|_| invalid("plan_output_line.kind", row.kind))?,
            text: row.text,
            block_id: row.block_id,
        })
    }
}

async fn connect(path: &Path) -> Result<SqliteConnection, sqlx::Error> {
    let options =
        writable_options(path, false).map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
    SqliteConnection::connect_with(&options).await
}

fn write<T>(
    path: &Path,
    operation: impl AsyncFnOnce(&mut SqliteConnection) -> Result<T, sqlx::Error>,
) -> Result<T, DatabaseError> {
    block_on(async {
        let mut connection = connect(path).await?;
        operation(&mut connection).await
    })
}

async fn upsert_run(connection: &mut SqliteConnection, run: &PlanRun) -> Result<(), sqlx::Error> {
    let start_step = i64::try_from(run.start_step)
        .map_err(|_| sqlx::Error::Protocol("plan start step out of range".into()))?;
    let total_steps = i64::try_from(run.total_steps)
        .map_err(|_| sqlx::Error::Protocol("plan total steps out of range".into()))?;
    let selected_step = i64::try_from(run.selected_step)
        .map_err(|_| sqlx::Error::Protocol("selected plan step out of range".into()))?;
    let created = i64::try_from(run.created_unix_ms)
        .map_err(|_| sqlx::Error::Protocol("plan creation time out of range".into()))?;
    let updated = i64::try_from(run.updated_unix_ms)
        .map_err(|_| sqlx::Error::Protocol("plan update time out of range".into()))?;
    let archived = run
        .archived_unix_ms
        .map(i64::try_from)
        .transpose()
        .map_err(|_| sqlx::Error::Protocol("plan archive time out of range".into()))?;
    let scope_path = run.scope_path.to_string_lossy().into_owned();
    let plan_path = run.plan_path.to_string_lossy().into_owned();
    let mode = run.mode.as_str();
    let status = run.status.as_str();
    let pause_requested = i64::from(run.pause_requested);
    let id = run.id.clone();
    let harness_id = run.harness_id.clone();
    let repo_root = run.repo_root.clone();
    let plan_display = run.plan_display.clone();
    let step_name = run.step_name.clone();
    let adapter_id = run.adapter_id.clone();
    sqlx::query_file!(
        "sql/plan_run/upsert_run.sql",
        id,
        harness_id,
        repo_root,
        scope_path,
        plan_path,
        plan_display,
        step_name,
        start_step,
        total_steps,
        mode,
        status,
        pause_requested,
        selected_step,
        created,
        updated,
        archived,
        adapter_id
    )
    .execute(connection)
    .await?;
    Ok(())
}

async fn upsert_step(
    connection: &mut SqliteConnection,
    step: &PlanStepRun,
) -> Result<(), sqlx::Error> {
    let number = i64::try_from(step.step)
        .map_err(|_| sqlx::Error::Protocol("plan step out of range".into()))?;
    let process_id = step.execution.process_id.map(i64::from);
    let started = step
        .started_unix_ms
        .map(i64::try_from)
        .transpose()
        .map_err(|_| sqlx::Error::Protocol("plan start time out of range".into()))?;
    let finished = step
        .finished_unix_ms
        .map(i64::try_from)
        .transpose()
        .map_err(|_| sqlx::Error::Protocol("plan finish time out of range".into()))?;
    let identity = step
        .execution
        .process_identity
        .map(i64::try_from)
        .transpose()
        .map_err(|_| sqlx::Error::Protocol("plan process identity out of range".into()))?;
    let todos = serde_json::to_string(
        &step
            .todos
            .iter()
            .map(|todo| serde_json::json!({"title": todo.title, "status": todo.status}))
            .collect::<Vec<_>>(),
    )
    .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
    let status = step.status.as_str();
    let run_id = step.run_id.clone();
    let prompt = step.prompt.clone();
    let execution_state = step.execution.state.clone();
    let session_endpoint = step.session.endpoint.clone();
    let session_id = step.session.id.clone();
    let agent_variant = step.agent_variant.clone();
    let latest_message = step.latest_message.clone();
    let active_tool = step.active_tool.clone();
    let summary = step.summary.clone();
    let error = step.error.clone();
    let session_adapter_id = step.session.adapter_id.clone();
    sqlx::query_file!(
        "sql/plan_run/upsert_step.sql",
        run_id,
        number,
        prompt,
        status,
        execution_state,
        session_endpoint,
        session_id,
        agent_variant,
        process_id,
        started,
        finished,
        step.exit_code,
        latest_message,
        active_tool,
        todos,
        summary,
        error,
        session_adapter_id,
        identity
    )
    .execute(connection)
    .await?;
    Ok(())
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

pub(crate) fn save(
    path: &Path,
    persisted: &PersistedPlanRun,
    enqueue: bool,
) -> Result<(), DatabaseError> {
    super::database::initialize(path)?;
    write(path, async |connection| {
        immediate(connection, async |connection| {
            upsert_run(connection, &persisted.run).await?;
            for step in &persisted.steps {
                upsert_step(connection, step).await?;
            }
            if enqueue {
                let now = i64::try_from(persisted.run.updated_unix_ms)
                    .map_err(|_| sqlx::Error::Protocol("plan enqueue time out of range".into()))?;
                let queued =
                    sqlx::query_file!("sql/plan_run/enqueue.sql", persisted.run.id, now, now)
                        .execute(&mut *connection)
                        .await?
                        .rows_affected();
                if queued == 0 {
                    let requested = sqlx::query_file!(
                        "sql/plan_run/request_requeue.sql",
                        now,
                        persisted.run.id
                    )
                    .execute(connection)
                    .await?
                    .rows_affected();
                    if requested == 0 {
                        return Err(sqlx::Error::Protocol(
                            "plan workflow could not be queued".into(),
                        ));
                    }
                }
            }
            Ok(())
        })
        .await
    })
}

pub(crate) fn save_run(path: &Path, run: &PlanRun) -> Result<(), DatabaseError> {
    super::database::initialize(path)?;
    write(path, async |connection| upsert_run(connection, run).await)
}

pub(crate) fn save_step(path: &Path, step: &PlanStepRun) -> Result<(), DatabaseError> {
    super::database::initialize(path)?;
    write(path, async |connection| upsert_step(connection, step).await)
}

pub(crate) fn load(path: &Path, run_id: &str) -> Result<Option<PersistedPlanRun>, DatabaseError> {
    super::database::initialize(path)?;
    let options = writable_options(path, false)?;
    let (run, steps) = block_on(async {
        let mut connection = SqliteConnection::connect_with(&options).await?;
        let run = sqlx::query_file_as!(RunRow, "sql/plan_run/load_run.sql", run_id)
            .fetch_optional(&mut connection)
            .await?;
        let steps = if run.is_some() {
            sqlx::query_file_as!(StepRow, "sql/plan_run/load_steps.sql", run_id)
                .fetch_all(&mut connection)
                .await?
        } else {
            Vec::new()
        };
        Ok((run, steps))
    })?;
    run.map(|row| {
        Ok(PersistedPlanRun {
            run: row.try_into()?,
            steps: steps
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_, _>>()?,
        })
    })
    .transpose()
}

pub(crate) fn recent(
    path: &Path,
    repo_root: &Path,
    limit: usize,
) -> Result<Vec<PersistedPlanRun>, DatabaseError> {
    super::database::initialize(path)?;
    let repo_root = repo_root.to_string_lossy();
    let limit = to_i64("plan_run.limit", limit)?;
    let options = writable_options(path, false)?;
    let ids = block_on(async {
        let mut connection = SqliteConnection::connect_with(&options).await?;
        sqlx::query_file_scalar!("sql/plan_run/load_recent_ids.sql", repo_root, limit)
            .fetch_all(&mut connection)
            .await
    })?;
    ids.into_iter()
        .map(|id| load(path, &id).and_then(|run| run.ok_or_else(|| invalid("plan_run.id", id))))
        .collect()
}

pub(crate) fn resumable(
    path: &Path,
    launch: &PlanLaunch,
) -> Result<Option<PersistedPlanRun>, DatabaseError> {
    super::database::initialize(path)?;
    let start = to_i64("plan_run.start_step", launch.start_step)?;
    let total = to_i64("plan_run.total_steps", launch.total_steps)?;
    let options = writable_options(path, false)?;
    let scope_path = launch.scope_path.to_string_lossy().into_owned();
    let plan_path = launch.plan_path.to_string_lossy().into_owned();
    let mode = launch.mode.as_str();
    let repo_root = launch.repo_root.clone();
    let step_name = launch.step_name.clone();
    let harness_id = launch.harness_id.clone();
    let adapter_id = launch.adapter_id.clone();
    let id = block_on(async {
        let mut connection = SqliteConnection::connect_with(&options).await?;
        sqlx::query_file_scalar!(
            "sql/plan_run/load_resumable_id.sql",
            repo_root,
            scope_path,
            plan_path,
            step_name,
            start,
            total,
            mode,
            harness_id,
            adapter_id
        )
        .fetch_optional(&mut connection)
        .await
    })?;
    id.map(|id| load(path, &id))
        .transpose()
        .map(Option::flatten)
}

pub(crate) fn load_output(
    path: &Path,
    run_id: &str,
    step: usize,
) -> Result<Vec<PlanOutputLine>, DatabaseError> {
    super::database::initialize(path)?;
    let step = to_i64("plan_output_line.step", step)?;
    let options = writable_options(path, false)?;
    let rows = block_on(async {
        let mut connection = SqliteConnection::connect_with(&options).await?;
        sqlx::query_file_as!(OutputRow, "sql/plan_run/load_output.sql", run_id, step)
            .fetch_all(&mut connection)
            .await
    })?;
    rows.into_iter().map(TryInto::try_into).collect()
}

pub(crate) fn append_output(
    path: &Path,
    line: &PlanOutputLine,
    max_lines: usize,
) -> Result<(), DatabaseError> {
    super::database::initialize(path)?;
    let step = to_i64("plan_output_line.step", line.step)?;
    let number = to_i64("plan_output_line.line_number", line.line_number)?;
    let time = to_i64("plan_output_line.time_unix_ms", line.time_unix_ms)?;
    let retained = to_i64("plan_output_line.retained", max_lines.saturating_sub(1))?;
    let run_id = line.run_id.clone();
    let text = line.text.clone();
    let block_id = line.block_id.clone();
    write(path, async |connection| {
        immediate(connection, async |connection| {
            let kind = line.kind.as_str();
            sqlx::query_file!(
                "sql/plan_run/upsert_output.sql",
                run_id,
                step,
                number,
                time,
                kind,
                text,
                block_id
            )
            .execute(&mut *connection)
            .await?;
            if max_lines == 0 {
                return Ok(());
            }
            let deleted = if retained == 0 {
                sqlx::query_file!("sql/plan_run/trim_all_output.sql", run_id, step)
                    .execute(&mut *connection)
                    .await?
                    .rows_affected()
            } else {
                sqlx::query_file!(
                    "sql/plan_run/trim_output.sql",
                    run_id,
                    step,
                    run_id,
                    step,
                    retained
                )
                .execute(&mut *connection)
                .await?
                .rows_affected()
            };
            if deleted > 0 {
                let first =
                    sqlx::query_file_scalar!("sql/plan_run/first_output_line.sql", run_id, step)
                        .fetch_one(&mut *connection)
                        .await?;
                if first >= 0 {
                    let marker = first.saturating_sub(1);
                    let text = format!("[... omitted {deleted} older output lines ...]");
                    sqlx::query_file!(
                        "sql/plan_run/upsert_output.sql",
                        run_id,
                        step,
                        marker,
                        time,
                        "system",
                        text,
                        Option::<String>::None
                    )
                    .execute(connection)
                    .await?;
                }
            }
            Ok(())
        })
        .await
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn append_allocated_output(
    path: &Path,
    run_id: &str,
    step: usize,
    time_unix_ms: u64,
    kind: PlanOutputKind,
    text: &str,
    block_id: Option<&str>,
    max_lines: usize,
) -> Result<(), DatabaseError> {
    super::database::initialize(path)?;
    let step = to_i64("plan_output_line.step", step)?;
    let time = to_i64("plan_output_line.time_unix_ms", time_unix_ms)?;
    let retained = to_i64("plan_output_line.retained", max_lines.saturating_sub(1))?;
    let kind = kind.as_str();
    write(path, async |connection| {
        immediate(connection, async |connection| {
            sqlx::query_file!(
                "sql/plan_run/insert_allocated_output.sql",
                run_id,
                step,
                time,
                kind,
                text,
                block_id,
                run_id,
                step
            )
            .execute(&mut *connection)
            .await?;
            if max_lines == 0 {
                return Ok(());
            }
            let deleted = if retained == 0 {
                sqlx::query_file!("sql/plan_run/trim_all_output.sql", run_id, step)
                    .execute(&mut *connection)
                    .await?
                    .rows_affected()
            } else {
                sqlx::query_file!(
                    "sql/plan_run/trim_output.sql",
                    run_id,
                    step,
                    run_id,
                    step,
                    retained
                )
                .execute(&mut *connection)
                .await?
                .rows_affected()
            };
            if deleted > 0 {
                let first =
                    sqlx::query_file_scalar!("sql/plan_run/first_output_line.sql", run_id, step)
                        .fetch_one(&mut *connection)
                        .await?;
                if first >= 0 {
                    let marker = first.saturating_sub(1);
                    let marker_text = format!("[... omitted {deleted} older output lines ...]");
                    let marker_block: Option<String> = None;
                    sqlx::query_file!(
                        "sql/plan_run/upsert_output.sql",
                        run_id,
                        step,
                        marker,
                        time,
                        "system",
                        marker_text,
                        marker_block
                    )
                    .execute(connection)
                    .await?;
                }
            }
            Ok(())
        })
        .await
    })
}

pub(crate) fn output_exists(
    path: &Path,
    run_id: &str,
    step: usize,
    kind: PlanOutputKind,
    text: &str,
) -> Result<bool, DatabaseError> {
    super::database::initialize(path)?;
    let step = to_i64("plan_output_line.step", step)?;
    let options = writable_options(path, false)?;
    let kind = kind.as_str();
    block_on(async {
        let mut connection = SqliteConnection::connect_with(&options).await?;
        sqlx::query_file_scalar!("sql/plan_run/output_exists.sql", run_id, step, kind, text)
            .fetch_one(&mut connection)
            .await
    })
}

pub(crate) fn cleanup(path: &Path, cutoff: u64) -> Result<usize, DatabaseError> {
    super::database::initialize(path)?;
    let cutoff = to_i64("plan_run.archived_unix_ms", cutoff)?;
    let options = writable_options(path, false)?;
    let changed = block_on(async {
        let mut connection = SqliteConnection::connect_with(&options).await?;
        Ok(
            sqlx::query_file!("sql/plan_run/cleanup_archived.sql", cutoff)
                .execute(&mut connection)
                .await?
                .rows_affected(),
        )
    })?;
    usize::try_from(changed).map_err(|_| invalid("plan_run.cleanup_count", changed))
}

pub(crate) fn finish_step(path: &Path, step: &PlanStepRun) -> Result<bool, DatabaseError> {
    super::database::initialize(path)?;
    let number = to_i64("plan_step_run.step", step.step)?;
    let finished = step
        .finished_unix_ms
        .map(|value| to_i64("plan_step_run.finished_unix_ms", value))
        .transpose()?;
    let status = step.status.as_str();
    let active_tool = step.active_tool.clone();
    let error = step.error.clone();
    let run_id = step.run_id.clone();
    let changed = write(path, async |connection| {
        Ok(sqlx::query_file!(
            "sql/plan_run/finish_step.sql",
            status,
            finished,
            step.exit_code,
            active_tool,
            error,
            run_id,
            number
        )
        .execute(connection)
        .await?
        .rows_affected())
    })?;
    Ok(changed != 0)
}

pub(crate) fn load_step_status(
    path: &Path,
    run_id: &str,
    step: usize,
) -> Result<PlanStepStatus, DatabaseError> {
    super::database::initialize(path)?;
    let step = to_i64("plan_step_run.step", step)?;
    let options = writable_options(path, false)?;
    let status = block_on(async {
        let mut connection = SqliteConnection::connect_with(&options).await?;
        sqlx::query_file_scalar!("sql/plan_run/load_step_status.sql", run_id, step)
            .fetch_one(&mut connection)
            .await
    })?;
    PlanStepStatus::parse(&status).map_err(|_| invalid("plan_step_run.status", status))
}

pub(crate) fn claim_process(path: &Path, step: &PlanStepRun) -> Result<bool, DatabaseError> {
    super::database::initialize(path)?;
    let number = to_i64("plan_step_run.step", step.step)?;
    let process_id = step.execution.process_id.map(i64::from);
    let identity = step
        .execution
        .process_identity
        .map(|value| to_i64("plan_step_run.execution_process_start_time_ticks", value))
        .transpose()?;
    let changed = write(path, async |connection| {
        Ok(sqlx::query_file!(
            "sql/plan_run/claim_spawned_process.sql",
            process_id,
            identity,
            step.run_id,
            number
        )
        .execute(connection)
        .await?
        .rows_affected())
    })?;
    Ok(changed != 0)
}

fn parse_todos(text: &str) -> Result<Vec<PlanTodo>, DatabaseError> {
    let values: Vec<serde_json::Value> =
        serde_json::from_str(text).map_err(|_| invalid("plan_step_run.todos_json", text))?;
    values
        .into_iter()
        .map(|value| {
            let title = value
                .get("title")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| invalid("plan_step_run.todos_json.title", value.to_string()))?;
            let status = value
                .get("status")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| invalid("plan_step_run.todos_json.status", value.to_string()))?;
            Ok(PlanTodo::new(title, status))
        })
        .collect()
}
