use std::path::{Path, PathBuf};

use sqlx::{Connection, SqliteConnection};

use crate::auto_flow::{
    AutoEvent, AutoImplementationSource, AutoOutputKind, AutoOutputLine, AutoRun, AutoRunMode,
    AutoRunStatus, AutoStepKey, AutoStepRun, AutoStepStatus, PersistedAutoRun, stabilization_model,
};

#[derive(Debug)]
struct RunRow {
    id: String,
    harness_id: String,
    adapter_id: String,
    repo_root: String,
    worktree_path: String,
    worktree_incarnation: Option<String>,
    branch: String,
    mode: String,
    implementation_source: String,
    plan_path: Option<String>,
    plan_run_mode: String,
    variant: String,
    agent_profile: Option<String>,
    prompt_summary: String,
    initial_prompt: String,
    status: String,
    pause_requested: i64,
    selected_step_run_id: Option<i64>,
    pr_number: Option<i64>,
    pr_url: Option<String>,
    current_head_sha: Option<String>,
    review_baseline_json: Option<String>,
    stabilization_status: Option<String>,
    stabilization_blocker: Option<String>,
    stabilization_next_work: Option<String>,
    pending_push_json: Option<String>,
    created_unix_ms: i64,
    updated_unix_ms: i64,
    archived_unix_ms: Option<i64>,
}

#[derive(Debug)]
struct StepRow {
    id: i64,
    run_id: String,
    sequence: i64,
    step_key: String,
    reason: Option<String>,
    status: String,
    attempt: i64,
    started_unix_ms: Option<i64>,
    finished_unix_ms: Option<i64>,
    execution_state: Option<String>,
    session_endpoint: Option<String>,
    session_id: Option<String>,
    execution_process_id: Option<i64>,
    plan_run_id: Option<String>,
    commit_sha: Option<String>,
    head_sha: Option<String>,
    work_guard_json: Option<String>,
    blocker: Option<String>,
    summary: Option<String>,
    error: Option<String>,
    session_adapter_id: Option<String>,
    execution_process_start_time_ticks: Option<i64>,
}

#[derive(Debug)]
struct OutputRow {
    step_run_id: i64,
    line_number: i64,
    time_unix_ms: i64,
    kind: String,
    text: String,
    block_id: Option<String>,
}

struct PreparedRun<'a> {
    run: &'a AutoRun,
    worktree_path: String,
    plan_path: Option<String>,
    pending_push_json: Option<String>,
    pr_number: Option<i64>,
    created: i64,
    updated: i64,
    archived: Option<i64>,
}

struct PreparedStep<'a> {
    step: &'a AutoStepRun,
    sequence: i64,
    attempt: i64,
    started: Option<i64>,
    finished: Option<i64>,
    process_id: Option<i64>,
    process_identity: Option<i64>,
    work_guard_json: Option<String>,
}

fn invalid(field: &'static str, value: impl ToString) -> String {
    format!("invalid persisted value for {field}: {}", value.to_string())
}

fn from_i64<T: TryFrom<i64>>(field: &'static str, value: i64) -> Result<T, String> {
    value.try_into().map_err(|_| invalid(field, value))
}

fn to_i64<T: TryInto<i64> + Copy>(field: &'static str, value: T) -> Result<i64, String> {
    value.try_into().map_err(|_| invalid(field, "out of range"))
}

fn parse_optional<T>(
    field: &'static str,
    value: Option<String>,
    parse: impl FnOnce(&str) -> Result<T, String>,
) -> Result<Option<T>, String> {
    value
        .map(|value| parse(&value).map_err(|_| invalid(field, value)))
        .transpose()
}

impl TryFrom<RunRow> for AutoRun {
    type Error = String;

    fn try_from(row: RunRow) -> Result<Self, Self::Error> {
        let pending_push = row
            .pending_push_json
            .map(|value| {
                serde_json::from_str(&value)
                    .map_err(|_| invalid("auto_run.pending_push_json", value))
            })
            .transpose()?;
        Ok(Self {
            id: row.id,
            harness_id: row.harness_id,
            adapter_id: row.adapter_id,
            repo_root: row.repo_root,
            worktree_path: PathBuf::from(row.worktree_path),
            worktree_incarnation: row.worktree_incarnation,
            branch: row.branch,
            mode: AutoRunMode::parse(&row.mode).map_err(|_| invalid("auto_run.mode", row.mode))?,
            implementation_source: AutoImplementationSource::parse(&row.implementation_source)
                .map_err(|_| {
                    invalid("auto_run.implementation_source", row.implementation_source)
                })?,
            plan_path: row.plan_path.map(PathBuf::from),
            plan_run_mode: match row.plan_run_mode.as_str() {
                "sequential" => crate::plan_run::PlanRunMode::Sequential,
                "parallel" => crate::plan_run::PlanRunMode::Parallel,
                _ => return Err(invalid("auto_run.plan_run_mode", row.plan_run_mode)),
            },
            variant: row.variant,
            agent_profile: row.agent_profile,
            prompt_summary: row.prompt_summary,
            initial_prompt: row.initial_prompt,
            status: AutoRunStatus::parse(&row.status)
                .map_err(|_| invalid("auto_run.status", row.status))?,
            pause_requested: match row.pause_requested {
                0 => false,
                1 => true,
                value => return Err(invalid("auto_run.pause_requested", value)),
            },
            selected_step_run_id: row.selected_step_run_id,
            pr_number: row
                .pr_number
                .map(|value| from_i64("auto_run.pr_number", value))
                .transpose()?,
            pr_url: row.pr_url,
            current_head_sha: row.current_head_sha,
            review_baseline_json: row.review_baseline_json,
            stabilization_status: parse_optional(
                "auto_run.stabilization_status",
                row.stabilization_status,
                stabilization_model::StabilizationStatus::parse,
            )?,
            stabilization_blocker: parse_optional(
                "auto_run.stabilization_blocker",
                row.stabilization_blocker,
                stabilization_model::StabilizationBlocker::parse,
            )?,
            stabilization_next_work: parse_optional(
                "auto_run.stabilization_next_work",
                row.stabilization_next_work,
                stabilization_model::StabilizationWorkKind::parse,
            )?,
            pending_push,
            created_unix_ms: from_i64("auto_run.created_unix_ms", row.created_unix_ms)?,
            updated_unix_ms: from_i64("auto_run.updated_unix_ms", row.updated_unix_ms)?,
            archived_unix_ms: row
                .archived_unix_ms
                .map(|value| from_i64("auto_run.archived_unix_ms", value))
                .transpose()?,
        })
    }
}

impl TryFrom<StepRow> for AutoStepRun {
    type Error = String;

    fn try_from(row: StepRow) -> Result<Self, Self::Error> {
        let work_guard = row
            .work_guard_json
            .map(|value| {
                serde_json::from_str(&value)
                    .map_err(|_| invalid("auto_step_run.work_guard_json", value))
            })
            .transpose()?;
        Ok(Self {
            id: Some(row.id),
            run_id: row.run_id,
            sequence: from_i64("auto_step_run.sequence", row.sequence)?,
            step_key: AutoStepKey::parse(&row.step_key),
            reason: row.reason,
            status: AutoStepStatus::parse(&row.status)
                .map_err(|_| invalid("auto_step_run.status", row.status))?,
            attempt: from_i64("auto_step_run.attempt", row.attempt)?,
            started_unix_ms: row
                .started_unix_ms
                .map(|value| from_i64("auto_step_run.started_unix_ms", value))
                .transpose()?,
            finished_unix_ms: row
                .finished_unix_ms
                .map(|value| from_i64("auto_step_run.finished_unix_ms", value))
                .transpose()?,
            execution: crate::harness::ExecutionRef {
                state: row.execution_state,
                process_id: row
                    .execution_process_id
                    .map(|value| from_i64("auto_step_run.execution_process_id", value))
                    .transpose()?,
                process_identity: row
                    .execution_process_start_time_ticks
                    .map(|value| {
                        from_i64("auto_step_run.execution_process_start_time_ticks", value)
                    })
                    .transpose()?,
            },
            session: crate::harness::SessionRef {
                adapter_id: row.session_adapter_id,
                endpoint: row.session_endpoint,
                id: row.session_id,
            },
            plan_run_id: row.plan_run_id,
            commit_sha: row.commit_sha,
            head_sha: row.head_sha,
            work_guard,
            blocker: parse_optional(
                "auto_step_run.blocker",
                row.blocker,
                stabilization_model::StabilizationBlocker::parse,
            )?,
            summary: row.summary,
            error: row.error,
        })
    }
}

impl TryFrom<OutputRow> for AutoOutputLine {
    type Error = String;

    fn try_from(row: OutputRow) -> Result<Self, Self::Error> {
        Ok(Self {
            step_run_id: row.step_run_id,
            line_number: from_i64("auto_output_line.line_number", row.line_number)?,
            time_unix_ms: from_i64("auto_output_line.time_unix_ms", row.time_unix_ms)?,
            kind: AutoOutputKind::parse(&row.kind)
                .map_err(|_| invalid("auto_output_line.kind", row.kind))?,
            text: row.text,
            block_id: row.block_id,
        })
    }
}

impl<'a> PreparedRun<'a> {
    fn new(run: &'a AutoRun) -> Result<Self, String> {
        Ok(Self {
            run,
            worktree_path: run.worktree_path.to_string_lossy().into_owned(),
            plan_path: run
                .plan_path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            pending_push_json: run
                .pending_push
                .as_ref()
                .map(serde_json::to_string)
                .transpose()
                .map_err(|error| invalid("auto_run.pending_push_json", error))?,
            pr_number: run
                .pr_number
                .map(|value| to_i64("auto_run.pr_number", value))
                .transpose()?,
            created: to_i64("auto_run.created_unix_ms", run.created_unix_ms)?,
            updated: to_i64("auto_run.updated_unix_ms", run.updated_unix_ms)?,
            archived: run
                .archived_unix_ms
                .map(|value| to_i64("auto_run.archived_unix_ms", value))
                .transpose()?,
        })
    }
}

impl<'a> PreparedStep<'a> {
    fn new(step: &'a AutoStepRun) -> Result<Self, String> {
        Ok(Self {
            step,
            sequence: to_i64("auto_step_run.sequence", step.sequence)?,
            attempt: to_i64("auto_step_run.attempt", step.attempt)?,
            started: step
                .started_unix_ms
                .map(|value| to_i64("auto_step_run.started_unix_ms", value))
                .transpose()?,
            finished: step
                .finished_unix_ms
                .map(|value| to_i64("auto_step_run.finished_unix_ms", value))
                .transpose()?,
            process_id: step.execution.process_id.map(i64::from),
            process_identity: step
                .execution
                .process_identity
                .map(|value| to_i64("auto_step_run.execution_process_start_time_ticks", value))
                .transpose()?,
            work_guard_json: step
                .work_guard
                .as_ref()
                .map(serde_json::to_string)
                .transpose()
                .map_err(|error| invalid("auto_step_run.work_guard_json", error))?,
        })
    }
}

async fn immediate<T>(
    connection: &mut SqliteConnection,
    operation: impl AsyncFnOnce(&mut SqliteConnection) -> Result<T, sqlx::Error>,
) -> Result<T, sqlx::Error> {
    super::database::begin_immediate_query()
        .execute(&mut *connection)
        .await?;
    match operation(&mut *connection).await {
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

async fn upsert_run(
    connection: &mut SqliteConnection,
    prepared: &PreparedRun<'_>,
    selected_step_run_id: Option<i64>,
) -> Result<(), sqlx::Error> {
    let run = prepared.run;
    let plan_mode = match run.plan_run_mode {
        crate::plan_run::PlanRunMode::Sequential => "sequential",
        crate::plan_run::PlanRunMode::Parallel => "parallel",
    };
    let mode = run.mode.as_str();
    let implementation_source = run.implementation_source.as_str();
    let status = run.status.as_str();
    let stabilization_status = run.stabilization_status.map(|value| value.as_str());
    let stabilization_blocker = run
        .stabilization_blocker
        .as_ref()
        .map(|value| value.as_str());
    let stabilization_next_work = run
        .stabilization_next_work
        .as_ref()
        .map(|value| value.as_str());
    sqlx::query_file!(
        "sql/auto_flow/upsert_run.sql",
        run.id,
        run.harness_id,
        run.repo_root,
        prepared.worktree_path,
        run.worktree_incarnation,
        run.branch,
        mode,
        implementation_source,
        prepared.plan_path,
        plan_mode,
        run.variant,
        run.agent_profile,
        run.prompt_summary,
        run.initial_prompt,
        status,
        run.pause_requested,
        selected_step_run_id,
        prepared.pr_number,
        run.pr_url,
        run.current_head_sha,
        run.review_baseline_json,
        stabilization_status,
        stabilization_blocker,
        stabilization_next_work,
        prepared.pending_push_json,
        prepared.created,
        prepared.updated,
        prepared.archived,
        run.adapter_id
    )
    .execute(connection)
    .await?;
    Ok(())
}

async fn upsert_step(
    connection: &mut SqliteConnection,
    prepared: &PreparedStep<'_>,
) -> Result<i64, sqlx::Error> {
    let step = prepared.step;
    let blocker = step.blocker.as_ref().map(|value| value.as_str());
    let step_key = step.step_key.as_str();
    let status = step.status.as_str();
    if let Some(id) = step.id {
        sqlx::query_file!(
            "sql/auto_flow/update_step.sql",
            step.run_id,
            prepared.sequence,
            step_key,
            step.reason,
            status,
            prepared.attempt,
            prepared.started,
            prepared.finished,
            step.execution.state,
            step.session.endpoint,
            step.session.id,
            prepared.process_id,
            step.plan_run_id,
            step.commit_sha,
            step.head_sha,
            prepared.work_guard_json,
            blocker,
            step.summary,
            step.error,
            step.session.adapter_id,
            prepared.process_identity,
            id,
            status
        )
        .execute(connection)
        .await?;
        Ok(id)
    } else {
        sqlx::query_file_scalar!(
            "sql/auto_flow/insert_step.sql",
            step.run_id,
            prepared.sequence,
            step_key,
            step.reason,
            status,
            prepared.attempt,
            prepared.started,
            prepared.finished,
            step.execution.state,
            step.session.endpoint,
            step.session.id,
            prepared.process_id,
            step.plan_run_id,
            step.commit_sha,
            step.head_sha,
            prepared.work_guard_json,
            blocker,
            step.summary,
            step.error,
            step.session.adapter_id,
            prepared.process_identity
        )
        .fetch_one(connection)
        .await
    }
}

fn options(path: &Path) -> Result<sqlx::sqlite::SqliteConnectOptions, String> {
    super::database::writable_options(path, false)
        .map_err(|error| format!("open Auto Flow database {}: {error}", path.display()))
}

fn block<T>(
    future: impl std::future::Future<Output = Result<T, sqlx::Error>>,
) -> Result<T, String> {
    crate::async_runtime::block_on(future)
        .map_err(|error| format!("access Auto Flow application runtime: {error}"))?
        .map_err(|error| format!("Auto Flow database operation: {error}"))
}

fn write<T>(
    path: &Path,
    operation: impl AsyncFnOnce(&mut SqliteConnection) -> Result<T, sqlx::Error>,
) -> Result<T, String> {
    let options = options(path)?;
    block(async {
        let mut connection = SqliteConnection::connect_with(&options).await?;
        let result = operation(&mut connection).await;
        let close = connection.close().await;
        match result {
            Err(error) => Err(error),
            Ok(value) => {
                close?;
                Ok(value)
            }
        }
    })
}

pub(crate) fn save(
    path: &Path,
    persisted: &mut PersistedAutoRun,
    enqueue: bool,
    selected_step_index: Option<usize>,
) -> Result<(), String> {
    let mut run_without_selection = persisted.run.clone();
    run_without_selection.selected_step_run_id = None;
    let first_run = PreparedRun::new(&run_without_selection)?;
    let steps = persisted
        .steps
        .iter()
        .map(PreparedStep::new)
        .collect::<Result<Vec<_>, _>>()?;
    let ids = write(path, async |connection| {
        immediate(connection, async |connection| {
            upsert_run(connection, &first_run, None).await?;
            let mut ids = Vec::with_capacity(steps.len());
            for step in &steps {
                ids.push(upsert_step(connection, step).await?);
            }
            let selected_step_run_id = selected_step_index
                .and_then(|index| ids.get(index).copied())
                .or(persisted.run.selected_step_run_id);
            upsert_run(connection, &first_run, selected_step_run_id).await?;
            if enqueue {
                let changed = sqlx::query_file!(
                    "sql/auto_flow/enqueue.sql",
                    first_run.run.id,
                    first_run.updated,
                    first_run.updated
                )
                .execute(&mut *connection)
                .await?
                .rows_affected();
                if changed == 0 {
                    let requested = sqlx::query_file!(
                        "sql/auto_flow/request_workflow_requeue.sql",
                        first_run.updated,
                        first_run.run.id
                    )
                    .execute(&mut *connection)
                    .await?
                    .rows_affected();
                    if requested == 0 {
                        return Err(sqlx::Error::Protocol(
                            "workflow could not be queued".to_string(),
                        ));
                    }
                }
            }
            Ok(ids)
        })
        .await
    })?;
    for (step, id) in persisted.steps.iter_mut().zip(ids) {
        step.id = Some(id);
    }
    if let Some(index) = selected_step_index {
        persisted.run.selected_step_run_id = persisted.steps.get(index).and_then(|step| step.id);
    }
    Ok(())
}

pub(crate) fn save_run(path: &Path, run: &AutoRun) -> Result<(), String> {
    let prepared = PreparedRun::new(run)?;
    write(path, async |connection| {
        immediate(connection, async |connection| {
            upsert_run(connection, &prepared, run.selected_step_run_id).await
        })
        .await
    })
}

pub(crate) fn save_step(path: &Path, step: &mut AutoStepRun) -> Result<i64, String> {
    let prepared = PreparedStep::new(step)?;
    let id = write(path, async |connection| {
        immediate(connection, async |connection| {
            upsert_step(connection, &prepared).await
        })
        .await
    })?;
    step.id = Some(id);
    Ok(id)
}

async fn load_on(
    connection: &mut SqliteConnection,
    run_id: &str,
) -> Result<Option<(RunRow, Vec<StepRow>)>, sqlx::Error> {
    let run = sqlx::query_file_as!(RunRow, "sql/auto_flow/load_run.sql", run_id)
        .fetch_optional(&mut *connection)
        .await?;
    let Some(run) = run else {
        return Ok(None);
    };
    let steps = sqlx::query_file_as!(StepRow, "sql/auto_flow/load_steps.sql", run_id)
        .fetch_all(connection)
        .await?;
    Ok(Some((run, steps)))
}

pub(crate) fn load(path: &Path, run_id: &str) -> Result<Option<PersistedAutoRun>, String> {
    let options = options(path)?;
    let loaded = block(async {
        let mut connection = SqliteConnection::connect_with(&options).await?;
        load_on(&mut connection, run_id).await
    })?;
    loaded
        .map(|(run, steps)| {
            Ok(PersistedAutoRun {
                run: run.try_into()?,
                steps: steps
                    .into_iter()
                    .map(TryInto::try_into)
                    .collect::<Result<_, _>>()?,
            })
        })
        .transpose()
}

fn load_many(path: &Path, ids: Vec<String>) -> Result<Vec<PersistedAutoRun>, String> {
    ids.into_iter()
        .map(|id| load(path, &id)?.ok_or_else(|| invalid("auto_run.id", id)))
        .collect()
}

pub(crate) fn recent_active(
    path: &Path,
    repo_root: &Path,
    limit: usize,
) -> Result<Vec<PersistedAutoRun>, String> {
    let repo_root = repo_root.to_string_lossy();
    let limit = to_i64("auto_run.limit", limit)?;
    let options = options(path)?;
    let ids = block(async {
        let mut connection = SqliteConnection::connect_with(&options).await?;
        sqlx::query_file_scalar!(
            "sql/auto_flow/load_recent_active_run_ids.sql",
            repo_root,
            limit
        )
        .fetch_all(&mut connection)
        .await
    })?;
    load_many(path, ids)
}

pub(crate) fn terminal_repairs(
    path: &Path,
    repo_root: &Path,
) -> Result<Vec<PersistedAutoRun>, String> {
    let repo_root = repo_root.to_string_lossy();
    let options = options(path)?;
    let ids = block(async {
        let mut connection = SqliteConnection::connect_with(&options).await?;
        sqlx::query_file_scalar!("sql/auto_flow/load_terminal_repair_run_ids.sql", repo_root)
            .fetch_all(&mut connection)
            .await
    })?;
    load_many(path, ids)
}

pub(crate) fn save_identity(
    path: &Path,
    run_id: &str,
    identity_json: Option<&str>,
) -> Result<bool, String> {
    let changed = write(path, async |connection| {
        immediate(connection, async |connection| {
            Ok(sqlx::query_file!(
                "sql/auto_flow/save_change_request_identity.sql",
                identity_json,
                run_id
            )
            .execute(connection)
            .await?
            .rows_affected())
        })
        .await
    })?;
    Ok(changed == 1)
}

pub(crate) fn load_identity(path: &Path, run_id: &str) -> Result<Option<String>, String> {
    let options = options(path)?;
    block(async {
        let mut connection = SqliteConnection::connect_with(&options).await?;
        Ok(
            sqlx::query_file_scalar!("sql/auto_flow/load_change_request_identity.sql", run_id)
                .fetch_optional(&mut connection)
                .await?
                .flatten(),
        )
    })
}

pub(crate) fn load_output(path: &Path, step_run_id: i64) -> Result<Vec<AutoOutputLine>, String> {
    let options = options(path)?;
    let rows = block(async {
        let mut connection = SqliteConnection::connect_with(&options).await?;
        sqlx::query_file_as!(
            OutputRow,
            "sql/auto_flow/load_output_lines.sql",
            step_run_id
        )
        .fetch_all(&mut connection)
        .await
    })?;
    rows.into_iter().map(TryInto::try_into).collect()
}

pub(crate) fn append_output(
    path: &Path,
    line: &AutoOutputLine,
    max_lines: usize,
) -> Result<(), String> {
    let number = to_i64("auto_output_line.line_number", line.line_number)?;
    let time = to_i64("auto_output_line.time_unix_ms", line.time_unix_ms)?;
    let retained = to_i64("auto_output_line.retained", max_lines.saturating_sub(1))?;
    let kind = line.kind.as_str();
    write(path, async |connection| {
        immediate(connection, async |connection| {
            sqlx::query_file!(
                "sql/auto_flow/upsert_output_line.sql",
                line.step_run_id,
                number,
                time,
                kind,
                line.text,
                line.block_id
            )
            .execute(&mut *connection)
            .await?;
            if max_lines == 0 {
                return Ok(());
            }
            let deleted = if retained == 0 {
                sqlx::query_file!("sql/auto_flow/delete_output_lines.sql", line.step_run_id)
                    .execute(&mut *connection)
                    .await?
                    .rows_affected()
            } else {
                sqlx::query_file!(
                    "sql/auto_flow/trim_output_lines.sql",
                    line.step_run_id,
                    line.step_run_id,
                    retained
                )
                .execute(&mut *connection)
                .await?
                .rows_affected()
            };
            if deleted > 0 {
                let first = sqlx::query_file_scalar!(
                    "sql/auto_flow/first_output_line.sql",
                    line.step_run_id
                )
                .fetch_one(&mut *connection)
                .await?;
                if let Some(first) = first {
                    let marker = first.saturating_sub(1);
                    let text = format!("[... omitted {deleted} older output lines ...]");
                    sqlx::query_file!(
                        "sql/auto_flow/upsert_output_line.sql",
                        line.step_run_id,
                        marker,
                        time,
                        "system",
                        text,
                        None::<String>
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

pub(crate) fn next_output(path: &Path, step_run_id: i64) -> Result<u64, String> {
    let options = options(path)?;
    let value = block(async {
        let mut connection = SqliteConnection::connect_with(&options).await?;
        sqlx::query_file_scalar!("sql/auto_flow/next_output_line.sql", step_run_id)
            .fetch_one(&mut connection)
            .await
    })?;
    from_i64("auto_output_line.line_number", value)
}

pub(crate) fn append_event(path: &Path, event: &AutoEvent) -> Result<i64, String> {
    let time = to_i64("auto_event.time_unix_ms", event.time_unix_ms)?;
    write(path, async |connection| {
        immediate(connection, async |connection| {
            sqlx::query_file_scalar!(
                "sql/auto_flow/insert_event.sql",
                event.run_id,
                event.step_run_id,
                time,
                event.kind,
                event.data_json
            )
            .fetch_one(connection)
            .await
        })
        .await
    })
}

pub(crate) fn save_run_and_event(
    path: &Path,
    run: &AutoRun,
    event: &AutoEvent,
) -> Result<i64, String> {
    let prepared = PreparedRun::new(run)?;
    let time = to_i64("auto_event.time_unix_ms", event.time_unix_ms)?;
    write(path, async |connection| {
        immediate(connection, async |connection| {
            upsert_run(connection, &prepared, run.selected_step_run_id).await?;
            sqlx::query_file_scalar!(
                "sql/auto_flow/insert_event.sql",
                event.run_id,
                event.step_run_id,
                time,
                event.kind,
                event.data_json
            )
            .fetch_one(connection)
            .await
        })
        .await
    })
}

pub(crate) fn claim_process(
    path: &Path,
    step_id: i64,
    process_id: u32,
    process_identity: Option<u64>,
) -> Result<bool, String> {
    let process_identity = process_identity
        .map(|value| to_i64("auto_step_run.execution_process_start_time_ticks", value))
        .transpose()?;
    let process_id = i64::from(process_id);
    let changed = write(path, async |connection| {
        immediate(connection, async |connection| {
            Ok(sqlx::query_file!(
                "sql/auto_flow/claim_step_process.sql",
                process_id,
                process_identity,
                step_id
            )
            .execute(connection)
            .await?
            .rows_affected())
        })
        .await
    })?;
    Ok(changed == 1)
}

pub(crate) fn finish_step(path: &Path, step: &AutoStepRun) -> Result<bool, String> {
    let id = step
        .id
        .ok_or_else(|| invalid("auto_step_run.id", "missing"))?;
    let finished = step
        .finished_unix_ms
        .map(|value| to_i64("auto_step_run.finished_unix_ms", value))
        .transpose()?;
    let status = step.status.as_str();
    let changed = write(path, async |connection| {
        immediate(connection, async |connection| {
            Ok(sqlx::query_file!(
                "sql/auto_flow/finish_step_guarded.sql",
                status,
                finished,
                step.error,
                id
            )
            .execute(connection)
            .await?
            .rows_affected())
        })
        .await
    })?;
    Ok(changed == 1)
}

pub(crate) fn load_step_status(path: &Path, step_id: i64) -> Result<AutoStepStatus, String> {
    let options = options(path)?;
    let status = block(async {
        let mut connection = SqliteConnection::connect_with(&options).await?;
        sqlx::query_file_scalar!("sql/auto_flow/load_step_status.sql", step_id)
            .fetch_one(&mut connection)
            .await
    })?;
    AutoStepStatus::parse(&status).map_err(|_| invalid("auto_step_run.status", status))
}

#[cfg(test)]
pub(crate) fn test_clear_worktree_incarnation(path: &Path, run_id: &str) -> Result<(), String> {
    test_execute(path, async |connection| {
        sqlx::query_file!("sql/auto_flow/test_clear_worktree_incarnation.sql", run_id)
            .execute(connection)
            .await?;
        Ok(())
    })
}

#[cfg(test)]
pub(crate) fn test_corrupt_change_request_identity(
    path: &Path,
    run_id: &str,
) -> Result<(), String> {
    test_execute(path, async |connection| {
        sqlx::query_file!(
            "sql/auto_flow/test_corrupt_change_request_identity.sql",
            run_id
        )
        .execute(connection)
        .await?;
        Ok(())
    })
}

#[cfg(test)]
pub(crate) fn test_count_events(path: &Path, run_id: &str, kind: &str) -> Result<i64, String> {
    test_execute(path, async |connection| {
        sqlx::query_file_scalar!("sql/auto_flow/test_count_events.sql", run_id, kind)
            .fetch_one(connection)
            .await
    })
}

#[cfg(test)]
pub(crate) fn test_install_selected_step_failure(
    path: &Path,
    changed_only: bool,
) -> Result<(), String> {
    test_execute(path, async |connection| {
        if changed_only {
            sqlx::query_file!("sql/auto_flow/test_fail_changed_selected_step_update.sql")
                .execute(connection)
                .await?;
        } else {
            sqlx::query_file!("sql/auto_flow/test_fail_selected_step_update.sql")
                .execute(connection)
                .await?;
        }
        Ok(())
    })
}

#[cfg(all(test, unix))]
pub(crate) fn test_install_policy_refresh_failure(path: &Path) -> Result<(), String> {
    test_execute(path, async |connection| {
        sqlx::query_file!("sql/auto_flow/test_fail_policy_refresh.sql")
            .execute(connection)
            .await?;
        Ok(())
    })
}

#[cfg(test)]
pub(crate) fn test_insert_task_metadata(
    path: &Path,
    branch: &str,
    worktree: &Path,
) -> Result<(), String> {
    let worktree = worktree.to_string_lossy().into_owned();
    test_execute(path, async |connection| {
        sqlx::query_file!(
            "sql/auto_flow/test_insert_task_metadata.sql",
            branch,
            worktree
        )
        .execute(connection)
        .await?;
        Ok(())
    })
}

#[cfg(test)]
pub(crate) fn test_insert_pending_deletion(
    path: &Path,
    branch: &str,
    worktree: &Path,
    incarnation: &str,
) -> Result<(), String> {
    let worktree = worktree.to_string_lossy().into_owned();
    test_execute(path, async |connection| {
        sqlx::query_file!(
            "sql/auto_flow/test_insert_pending_deletion.sql",
            branch,
            worktree,
            incarnation
        )
        .execute(connection)
        .await?;
        Ok(())
    })
}

#[cfg(test)]
pub(crate) fn test_count_task_metadata(path: &Path, branch: &str) -> Result<i64, String> {
    test_execute(path, async |connection| {
        sqlx::query_file_scalar!("sql/auto_flow/test_count_task_metadata.sql", branch)
            .fetch_one(connection)
            .await
    })
}

#[cfg(test)]
pub(crate) fn test_count_pending_deletion(path: &Path, branch: &str) -> Result<i64, String> {
    test_execute(path, async |connection| {
        sqlx::query_file_scalar!("sql/auto_flow/test_count_pending_deletion.sql", branch)
            .fetch_one(connection)
            .await
    })
}

#[cfg(test)]
fn test_execute<T>(
    path: &Path,
    operation: impl AsyncFnOnce(&mut SqliteConnection) -> Result<T, sqlx::Error>,
) -> Result<T, String> {
    let options = options(path)?;
    block(async {
        let mut connection = SqliteConnection::connect_with(&options).await?;
        operation(&mut connection).await
    })
}
