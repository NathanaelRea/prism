use super::*;

pub fn migrate_schema(conn: &rusqlite::Connection) -> Result<(), String> {
    conn.execute_batch(
        "
        create table if not exists plan_run (
          id text primary key,
          harness_id text not null default 'opencode',
          adapter_id text not null default 'opencode',
          repo_root text not null,
          scope_path text not null,
          plan_path text not null,
          plan_display text not null,
          step_name text not null,
          start_step integer not null,
          total_steps integer not null,
          mode text not null,
          status text not null,
          pause_requested integer not null default 0,
          selected_step integer not null,
          created_unix_ms integer not null,
          updated_unix_ms integer not null,
          archived_unix_ms integer
        );

        create table if not exists plan_step_run (
          run_id text not null references plan_run(id) on delete cascade,
          step integer not null,
          prompt text not null,
          status text not null,
          opencode_state text,
          opencode_server_url text,
          opencode_session_id text,
          execution_state text,
          execution_process_id integer,
          execution_process_start_time_ticks integer,
          session_endpoint text,
          session_id text,
          session_adapter_id text,
          agent_variant text,
          process_id integer,
          started_unix_ms integer,
          finished_unix_ms integer,
          exit_code integer,
          latest_message text,
          active_tool text,
          todos_json text not null default '[]',
          summary text,
          error text,
          primary key (run_id, step)
        );

        create table if not exists plan_output_line (
          run_id text not null,
          step integer not null,
          line_number integer not null,
          time_unix_ms integer not null,
          kind text not null,
          text text not null,
          block_id text,
          primary key (run_id, step, line_number),
          foreign key (run_id, step) references plan_step_run(run_id, step) on delete cascade
        );

        create index if not exists plan_run_repo_idx
          on plan_run(repo_root, updated_unix_ms);
        create index if not exists plan_run_scope_idx
          on plan_run(scope_path, updated_unix_ms);
        create index if not exists plan_run_status_idx
          on plan_run(status, updated_unix_ms);
        create index if not exists plan_output_line_step_idx
          on plan_output_line(run_id, step, line_number);
        ",
    )
    .map_err(|error| format!("create plan run schema: {error}"))?;
    add_column_if_missing(
        conn,
        "plan_run",
        "harness_id",
        "alter table plan_run add column harness_id text not null default 'opencode'",
    )?;
    add_column_if_missing(
        conn,
        "plan_run",
        "adapter_id",
        "alter table plan_run add column adapter_id text not null default 'opencode'",
    )?;
    add_column_if_missing(
        conn,
        "plan_run",
        "archived_unix_ms",
        "alter table plan_run add column archived_unix_ms integer",
    )?;
    add_column_if_missing(
        conn,
        "plan_run",
        "pause_requested",
        "alter table plan_run add column pause_requested integer not null default 0",
    )?;
    add_column_if_missing(
        conn,
        "plan_step_run",
        "opencode_state",
        "alter table plan_step_run add column opencode_state text",
    )?;
    add_column_if_missing(
        conn,
        "plan_step_run",
        "agent_variant",
        "alter table plan_step_run add column agent_variant text",
    )?;
    for (column, definition, legacy) in [
        ("execution_state", "text", "opencode_state"),
        ("execution_process_id", "integer", "process_id"),
        ("execution_process_start_time_ticks", "integer", "null"),
        ("session_endpoint", "text", "opencode_server_url"),
        ("session_id", "text", "opencode_session_id"),
        (
            "session_adapter_id",
            "text",
            "case when opencode_session_id is not null then 'opencode' end",
        ),
    ] {
        if !table_has_column(conn, "plan_step_run", column)? {
            conn.execute(
                &format!("alter table plan_step_run add column {column} {definition}"),
                [],
            )
            .map_err(|error| format!("migrate plan_step_run.{column}: {error}"))?;
            conn.execute(&format!("update plan_step_run set {column} = {legacy}"), [])
                .map_err(|error| format!("backfill plan_step_run.{column}: {error}"))?;
        }
    }
    Ok(())
}

pub fn save_plan_run(
    conn: &rusqlite::Connection,
    persisted: &PersistedPlanRun,
) -> Result<(), String> {
    let tx = conn
        .unchecked_transaction()
        .map_err(|error| format!("begin plan run transaction: {error}"))?;
    save_run_with_conn(&tx, &persisted.run)?;
    for step in &persisted.steps {
        save_step_with_conn(&tx, step)?;
    }
    tx.commit()
        .map_err(|error| format!("commit plan run transaction: {error}"))?;
    Ok(())
}

pub fn submit_plan_run(
    conn: &rusqlite::Connection,
    persisted: &PersistedPlanRun,
) -> Result<(), String> {
    let tx = conn
        .unchecked_transaction()
        .map_err(|error| format!("begin managed plan submission: {error}"))?;
    save_run_with_conn(&tx, &persisted.run)?;
    for step in &persisted.steps {
        save_step_with_conn(&tx, step)?;
    }
    crate::execution::enqueue(
        &tx,
        &crate::execution::WorkflowIdentity::new(
            crate::execution::WorkflowKind::Plan,
            &persisted.run.id,
        ),
    )?;
    tx.commit()
        .map_err(|error| format!("commit managed plan submission: {error}"))
}

pub fn load_plan_run(
    conn: &rusqlite::Connection,
    run_id: &str,
) -> Result<Option<PersistedPlanRun>, String> {
    let run = load_run_with_conn(conn, run_id)?;
    let Some(run) = run else {
        return Ok(None);
    };
    let steps = load_steps_with_conn(conn, run_id)?;
    Ok(Some(PersistedPlanRun { run, steps }))
}

pub fn load_recent_plan_runs_for_repo(
    conn: &rusqlite::Connection,
    repo_root: &Path,
    limit: usize,
) -> Result<Vec<PersistedPlanRun>, String> {
    let mut statement = conn
        .prepare(
            "select id
             from plan_run
             where repo_root = ?1
               and archived_unix_ms is null
             order by
               case status
                  when 'running' then 0
                  when 'queued' then 1
                  when 'paused' then 2
                  when 'failed' then 3
                  else 4
                end,
               updated_unix_ms desc
             limit ?2",
        )
        .map_err(|error| format!("prepare recent plan run load: {error}"))?;
    let ids = statement
        .query_map(
            params![repo_root.display().to_string(), usize_to_i64(limit)],
            |row| row.get::<_, String>(0),
        )
        .map_err(|error| format!("load recent plan run ids: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read recent plan run ids: {error}"))?;
    ids.into_iter()
        .filter_map(|id| load_plan_run(conn, &id).transpose())
        .collect()
}

pub fn load_resumable_plan_run(
    conn: &rusqlite::Connection,
    launch: &PlanLaunch,
) -> Result<Option<PersistedPlanRun>, String> {
    let run_id = conn
        .query_row(
            "select id
             from plan_run
             where repo_root = ?1
               and scope_path = ?2
               and plan_path = ?3
               and step_name = ?4
               and start_step = ?5
                and total_steps = ?6
                and mode = ?7
                 and harness_id = ?8
                 and adapter_id = ?9
               and archived_unix_ms is null
               and status in ('queued', 'running', 'paused')
             order by updated_unix_ms desc
             limit 1",
            params![
                launch.repo_root.as_str(),
                launch.scope_path.display().to_string(),
                launch.plan_path.display().to_string(),
                launch.step_name.as_str(),
                usize_to_i64(launch.start_step),
                usize_to_i64(launch.total_steps),
                launch.mode.as_str(),
                launch.harness_id.as_str(),
                launch.adapter_id.as_str(),
            ],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| format!("load resumable plan run id: {error}"))?;
    run_id
        .map(|run_id| load_plan_run(conn, &run_id))
        .transpose()
        .map(Option::flatten)
}

pub fn save_plan_step(conn: &rusqlite::Connection, step: &PlanStepRun) -> Result<(), String> {
    save_step_with_conn(conn, step)
}

pub(super) fn save_run_with_conn(conn: &rusqlite::Connection, run: &PlanRun) -> Result<(), String> {
    conn.execute(
        "insert into plan_run (
           id, harness_id, repo_root, scope_path, plan_path, plan_display, step_name, start_step,
           total_steps, mode, status, pause_requested, selected_step, created_unix_ms,
           updated_unix_ms, archived_unix_ms, adapter_id
         ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
         on conflict(id) do update set
            repo_root = excluded.repo_root,
            harness_id = excluded.harness_id,
            adapter_id = excluded.adapter_id,
           scope_path = excluded.scope_path,
           plan_path = excluded.plan_path,
           plan_display = excluded.plan_display,
           step_name = excluded.step_name,
           start_step = excluded.start_step,
           total_steps = excluded.total_steps,
           mode = excluded.mode,
           status = excluded.status,
           pause_requested = excluded.pause_requested,
           selected_step = excluded.selected_step,
           updated_unix_ms = excluded.updated_unix_ms,
           archived_unix_ms = excluded.archived_unix_ms
         where plan_run.status != 'aborted' or excluded.status = 'queued'",
        params![
            run.id.as_str(),
            run.harness_id.as_str(),
            run.repo_root.as_str(),
            run.scope_path.display().to_string(),
            run.plan_path.display().to_string(),
            run.plan_display.as_str(),
            run.step_name.as_str(),
            usize_to_i64(run.start_step),
            usize_to_i64(run.total_steps),
            run.mode.as_str(),
            run.status.as_str(),
            bool_to_i64(run.pause_requested),
            usize_to_i64(run.selected_step),
            u64_to_i64(run.created_unix_ms),
            u64_to_i64(run.updated_unix_ms),
            run.archived_unix_ms.map(u64_to_i64),
            run.adapter_id.as_str(),
        ],
    )
    .map_err(|error| format!("write plan run: {error}"))?;
    Ok(())
}

pub(super) fn save_step_with_conn(
    conn: &rusqlite::Connection,
    step: &PlanStepRun,
) -> Result<(), String> {
    let todos_json = serde_json::to_string(
        &step
            .todos
            .iter()
            .map(|todo| {
                let mut map = BTreeMap::new();
                map.insert("title", todo.title.as_str());
                map.insert("status", todo.status.as_str());
                map
            })
            .collect::<Vec<_>>(),
    )
    .map_err(|error| format!("serialize plan todos: {error}"))?;
    conn.execute(
        "insert into plan_step_run (
           run_id, step, prompt, status, execution_state, session_endpoint, session_id,
           agent_variant, execution_process_id, started_unix_ms, finished_unix_ms, exit_code, latest_message,
            active_tool, todos_json, summary, error, session_adapter_id, execution_process_start_time_ticks
          ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)
          on conflict(run_id, step) do update set
            prompt = excluded.prompt,
            status = excluded.status,
             execution_state = excluded.execution_state,
             session_endpoint = excluded.session_endpoint,
             session_id = excluded.session_id,
             agent_variant = excluded.agent_variant,
             execution_process_id = excluded.execution_process_id,
             started_unix_ms = excluded.started_unix_ms,
             finished_unix_ms = excluded.finished_unix_ms,
             exit_code = excluded.exit_code,
             latest_message = excluded.latest_message,
             active_tool = excluded.active_tool,
             todos_json = excluded.todos_json,
             summary = excluded.summary,
              error = excluded.error,
               session_adapter_id = excluded.session_adapter_id,
               execution_process_start_time_ticks = excluded.execution_process_start_time_ticks
           where plan_step_run.status != 'aborted' or excluded.status = 'queued'",
        params![
            step.run_id.as_str(),
            usize_to_i64(step.step),
            step.prompt.as_str(),
            step.status.as_str(),
            step.execution.state.as_deref(),
            step.session.endpoint.as_deref(),
            step.session.id.as_deref(),
            step.agent_variant.as_deref(),
            step.execution.process_id.map(i64::from),
            step.started_unix_ms.map(u64_to_i64),
            step.finished_unix_ms.map(u64_to_i64),
            step.exit_code,
            step.latest_message.as_deref(),
            step.active_tool.as_deref(),
            todos_json,
            step.summary.as_deref(),
            step.error.as_deref(),
            step.session.adapter_id.as_deref(),
            step.execution.process_start_time_ticks.map(u64_to_i64),
        ],
    )
    .map_err(|error| format!("write plan step run: {error}"))?;
    Ok(())
}

pub(super) fn load_run_with_conn(
    conn: &rusqlite::Connection,
    run_id: &str,
) -> Result<Option<PlanRun>, String> {
    conn.query_row(
        "select id, harness_id, repo_root, scope_path, plan_path, plan_display, step_name,
                start_step, total_steps, mode, status, pause_requested, selected_step,
                 created_unix_ms, updated_unix_ms, archived_unix_ms, adapter_id
         from plan_run
         where id = ?1",
        params![run_id],
        |row| {
            let mode: String = row.get(9)?;
            let status: String = row.get(10)?;
            Ok(PlanRun {
                id: row.get(0)?,
                harness_id: row.get(1)?,
                adapter_id: row.get(16)?,
                repo_root: row.get(2)?,
                scope_path: PathBuf::from(row.get::<_, String>(3)?),
                plan_path: PathBuf::from(row.get::<_, String>(4)?),
                plan_display: row.get(5)?,
                step_name: row.get(6)?,
                start_step: i64_to_usize(row.get(7)?, 7),
                total_steps: i64_to_usize(row.get(8)?, 8),
                mode: PlanRunMode::parse(&mode).map_err(from_string_error)?,
                status: PlanRunStatus::parse(&status).map_err(from_string_error)?,
                pause_requested: row.get::<_, i64>(11)? != 0,
                selected_step: i64_to_usize(row.get(12)?, 12),
                created_unix_ms: i64_to_u64(row.get(13)?, 13),
                updated_unix_ms: i64_to_u64(row.get(14)?, 14),
                archived_unix_ms: row
                    .get::<_, Option<i64>>(15)?
                    .map(|value| value.max(0) as u64),
            })
        },
    )
    .optional()
    .map_err(|error| format!("load plan run: {error}"))
}

pub(super) fn load_steps_with_conn(
    conn: &rusqlite::Connection,
    run_id: &str,
) -> Result<Vec<PlanStepRun>, String> {
    let mut statement = conn
        .prepare(
            "select run_id, step, prompt, status, execution_state, session_endpoint, session_id,
                agent_variant, execution_process_id, started_unix_ms, finished_unix_ms, exit_code,
                    latest_message, active_tool, todos_json, summary, error, session_adapter_id,
                    execution_process_start_time_ticks
             from plan_step_run
             where run_id = ?1
             order by step",
        )
        .map_err(|error| format!("prepare plan step load: {error}"))?;
    let rows = statement
        .query_map(params![run_id], |row| {
            let status: String = row.get(3)?;
            let execution_state: Option<String> = row.get(4)?;
            let todos_json: String = row.get(14)?;
            Ok(PlanStepRun {
                run_id: row.get(0)?,
                step: i64_to_usize(row.get(1)?, 1),
                prompt: row.get(2)?,
                status: PlanStepStatus::parse(&status).map_err(from_string_error)?,
                execution: crate::harness::ExecutionRef {
                    state: execution_state,
                    process_id: row
                        .get::<_, Option<i64>>(8)?
                        .map(|value| value.max(0) as u32),
                    process_start_time_ticks: row
                        .get::<_, Option<i64>>(18)?
                        .map(|value| value.max(0) as u64),
                },
                session: crate::harness::SessionRef {
                    adapter_id: row.get(17)?,
                    endpoint: row.get(5)?,
                    id: row.get(6)?,
                },
                agent_variant: row.get(7)?,
                started_unix_ms: row
                    .get::<_, Option<i64>>(9)?
                    .map(|value| value.max(0) as u64),
                finished_unix_ms: row
                    .get::<_, Option<i64>>(10)?
                    .map(|value| value.max(0) as u64),
                exit_code: row.get(11)?,
                latest_message: row.get(12)?,
                active_tool: row.get(13)?,
                todos: parse_todos_json(&todos_json).map_err(from_string_error)?,
                summary: row.get(15)?,
                error: row.get(16)?,
            })
        })
        .map_err(|error| format!("load plan steps: {error}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read plan steps: {error}"))
}

pub(super) fn parse_todos_json(text: &str) -> Result<Vec<PlanTodo>, String> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let values: Vec<PlanTodo> = serde_json::from_str::<Vec<Value>>(text)
        .map_err(|error| format!("parse plan todos: {error}"))?
        .into_iter()
        .filter_map(|value| {
            let title = value.get("title")?.as_str()?.to_string();
            let status = value.get("status")?.as_str()?.to_string();
            Some(PlanTodo { title, status })
        })
        .collect();
    Ok(values)
}

pub(super) fn from_string_error(error: String) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(error.into())
}

pub(super) fn add_column_if_missing(
    conn: &rusqlite::Connection,
    table: &str,
    column: &str,
    sql: &str,
) -> Result<(), String> {
    let mut statement = conn
        .prepare(&format!("pragma table_info({table})"))
        .map_err(|error| format!("inspect {table} schema: {error}"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| format!("read {table} schema: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read {table} schema column: {error}"))?;
    if !columns.iter().any(|name| name == column) {
        conn.execute(sql, [])
            .map_err(|error| format!("migrate {table}.{column}: {error}"))?;
    }
    Ok(())
}

fn table_has_column(
    conn: &rusqlite::Connection,
    table: &str,
    column: &str,
) -> Result<bool, String> {
    let mut statement = conn
        .prepare(&format!("pragma table_info({table})"))
        .map_err(|error| format!("inspect {table} schema: {error}"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| format!("read {table} schema: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read {table} schema column: {error}"))?;
    Ok(columns.iter().any(|name| name == column))
}

pub(super) fn i64_to_usize(value: i64, index: usize) -> usize {
    usize::try_from(value)
        .unwrap_or_else(|_| panic!("SQLite column {index} contained invalid usize: {value}"))
}

pub(super) fn i64_to_u64(value: i64, index: usize) -> u64 {
    u64::try_from(value)
        .unwrap_or_else(|_| panic!("SQLite column {index} contained invalid u64: {value}"))
}

pub(super) fn usize_to_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

pub(super) fn u64_to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

pub(super) fn bool_to_i64(value: bool) -> i64 {
    if value { 1 } else { 0 }
}
