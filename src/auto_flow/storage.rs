use super::*;

pub fn migrate_schema(conn: &rusqlite::Connection) -> Result<(), String> {
    conn.execute_batch(
        "
        create table if not exists auto_run (
          id text primary key,
          harness_id text not null default 'opencode',
          adapter_id text not null default 'opencode',
          repo_root text not null,
          worktree_path text not null,
          worktree_incarnation text,
          branch text not null,
          mode text not null,
          implementation_source text not null default 'prompt',
          plan_path text,
          plan_run_mode text not null default 'sequential',
          variant text not null,
          agent_profile text,
          prompt_summary text not null,
          initial_prompt text not null,
          status text not null,
          pause_requested integer not null default 0,
          selected_step_run_id integer,
          pr_number integer,
          pr_url text,
          current_head_sha text,
          review_baseline_json text,
          stabilization_status text,
          stabilization_blocker text,
          stabilization_next_work text,
          pending_push_json text,
          created_unix_ms integer not null,
          updated_unix_ms integer not null,
          archived_unix_ms integer,
          foreign key (selected_step_run_id) references auto_step_run(id) on delete set null
        );

        create table if not exists auto_step_run (
          id integer primary key autoincrement,
          run_id text not null references auto_run(id) on delete cascade,
          sequence integer not null,
          step_key text not null,
          reason text,
          status text not null,
          attempt integer not null,
          started_unix_ms integer,
          finished_unix_ms integer,
          opencode_server_url text,
          opencode_session_id text,
          process_id integer,
          execution_state text,
          execution_process_id integer,
          execution_process_start_time_ticks integer,
          session_endpoint text,
          session_id text,
          session_adapter_id text,
          plan_run_id text,
          commit_sha text,
          head_sha text,
          work_guard_json text,
          blocker text,
          summary text,
          error text,
          unique(run_id, sequence)
        );

        create table if not exists auto_output_line (
          step_run_id integer not null references auto_step_run(id) on delete cascade,
          line_number integer not null,
          time_unix_ms integer not null,
          kind text not null,
          text text not null,
          block_id text,
          primary key (step_run_id, line_number)
        );

        create table if not exists auto_event (
          id integer primary key autoincrement,
          run_id text not null references auto_run(id) on delete cascade,
          step_run_id integer references auto_step_run(id) on delete set null,
          time_unix_ms integer not null,
          kind text not null,
          data_json text not null
        );

        create index if not exists auto_run_repo_idx
          on auto_run(repo_root, updated_unix_ms);
        create index if not exists auto_run_worktree_idx
          on auto_run(worktree_path, updated_unix_ms);
        create index if not exists auto_run_status_idx
          on auto_run(status, updated_unix_ms);
        create index if not exists auto_step_run_run_idx
          on auto_step_run(run_id, sequence);
        create index if not exists auto_step_run_key_idx
          on auto_step_run(run_id, step_key, attempt);
        create index if not exists auto_output_line_step_idx
          on auto_output_line(step_run_id, line_number);
        create index if not exists auto_event_run_idx
          on auto_event(run_id, time_unix_ms);

        create table if not exists auto_schema_version (
          id integer primary key check (id = 1),
          version integer not null
        );
        ",
    )
    .map_err(|error| format!("create auto flow schema: {error}"))?;
    if !table_has_column(conn, "auto_run", "pr_url")? {
        conn.execute("alter table auto_run add column pr_url text", [])
            .map_err(|error| format!("migrate auto_run pr_url column: {error}"))?;
    }
    if !table_has_column(conn, "auto_run", "harness_id")? {
        conn.execute(
            "alter table auto_run add column harness_id text not null default 'opencode'",
            [],
        )
        .map_err(|error| format!("migrate auto_run harness_id column: {error}"))?;
    }
    if !table_has_column(conn, "auto_run", "adapter_id")? {
        conn.execute(
            "alter table auto_run add column adapter_id text not null default 'opencode'",
            [],
        )
        .map_err(|error| format!("migrate auto_run adapter_id column: {error}"))?;
    }
    if !table_has_column(conn, "auto_run", "worktree_incarnation")? {
        conn.execute(
            "alter table auto_run add column worktree_incarnation text",
            [],
        )
        .map_err(|error| format!("migrate auto_run worktree_incarnation column: {error}"))?;
    }
    if !table_has_column(conn, "auto_run", "implementation_source")? {
        conn.execute(
            "alter table auto_run add column implementation_source text not null default 'prompt'",
            [],
        )
        .map_err(|error| format!("migrate auto_run implementation_source column: {error}"))?;
        conn.execute(
            "update auto_run
             set implementation_source = case mode
               when 'plan_first' then 'draft_plan'
               else 'prompt'
             end",
            [],
        )
        .map_err(|error| format!("backfill auto_run implementation_source: {error}"))?;
    }
    if !table_has_column(conn, "auto_run", "plan_path")? {
        conn.execute("alter table auto_run add column plan_path text", [])
            .map_err(|error| format!("migrate auto_run plan_path column: {error}"))?;
    }
    if !table_has_column(conn, "auto_run", "plan_run_mode")? {
        conn.execute(
            "alter table auto_run add column plan_run_mode text not null default 'sequential'",
            [],
        )
        .map_err(|error| format!("migrate auto_run plan_run_mode column: {error}"))?;
    }
    if !table_has_column(conn, "auto_step_run", "plan_run_id")? {
        conn.execute("alter table auto_step_run add column plan_run_id text", [])
            .map_err(|error| format!("migrate auto_step_run plan_run_id column: {error}"))?;
    }
    for (column, definition, legacy) in [
        ("execution_process_id", "integer", Some("process_id")),
        ("execution_process_start_time_ticks", "integer", None),
        ("session_endpoint", "text", Some("opencode_server_url")),
        ("session_id", "text", Some("opencode_session_id")),
        (
            "session_adapter_id",
            "text",
            Some("case when opencode_session_id is not null then 'opencode' end"),
        ),
        ("execution_state", "text", None),
    ] {
        if !table_has_column(conn, "auto_step_run", column)? {
            conn.execute(
                &format!("alter table auto_step_run add column {column} {definition}"),
                [],
            )
            .map_err(|error| format!("migrate auto_step_run {column}: {error}"))?;
            if let Some(legacy) = legacy {
                conn.execute(&format!("update auto_step_run set {column} = {legacy}"), [])
                    .map_err(|error| format!("backfill auto_step_run {column}: {error}"))?;
            }
        }
    }
    for (table, column) in [
        ("auto_run", "stabilization_status"),
        ("auto_run", "stabilization_blocker"),
        ("auto_run", "stabilization_next_work"),
        ("auto_run", "pending_push_json"),
        ("auto_step_run", "work_guard_json"),
        ("auto_step_run", "blocker"),
    ] {
        if !table_has_column(conn, table, column)? {
            conn.execute(&format!("alter table {table} add column {column} text"), [])
                .map_err(|error| format!("migrate {table} {column} column: {error}"))?;
        }
    }
    reset_incompatible_active_runs(conn)?;
    conn.execute(
        "insert into auto_schema_version (id, version) values (1, 6)
         on conflict(id) do update set version = excluded.version",
        [],
    )
    .map_err(|error| format!("write auto schema version: {error}"))?;
    Ok(())
}

pub fn save_auto_run(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
) -> Result<(), String> {
    let tx = conn
        .unchecked_transaction()
        .map_err(|error| format!("begin auto run transaction: {error}"))?;
    save_persisted_auto_run_with_conn(&tx, persisted)?;
    tx.commit()
        .map_err(|error| format!("commit auto run transaction: {error}"))?;
    Ok(())
}

pub fn submit_auto_run(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
) -> Result<(), String> {
    let tx = conn
        .unchecked_transaction()
        .map_err(|error| format!("begin managed Auto Flow submission: {error}"))?;
    save_persisted_auto_run_with_conn(&tx, persisted)?;
    crate::execution::enqueue(
        &tx,
        &crate::execution::WorkflowIdentity::new(
            crate::execution::WorkflowKind::Auto,
            &persisted.run.id,
        ),
    )?;
    tx.commit()
        .map_err(|error| format!("commit managed Auto Flow submission: {error}"))
}

pub(super) fn save_persisted_auto_run_with_conn(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
) -> Result<(), String> {
    let mut run_without_selection = persisted.run.clone();
    run_without_selection.selected_step_run_id = None;
    save_run_with_conn(conn, &run_without_selection)?;
    for step in &mut persisted.steps {
        save_step_with_conn(conn, step)?;
    }
    save_run_with_conn(conn, &persisted.run)
}

pub fn load_auto_run(
    conn: &rusqlite::Connection,
    run_id: &str,
) -> Result<Option<PersistedAutoRun>, String> {
    let run = load_run_with_conn(conn, run_id)?;
    let Some(mut run) = run else {
        return Ok(None);
    };
    if run.status == AutoRunStatus::Done
        && (run.pending_push.is_some()
            || run
                .stabilization_status
                .is_some_and(stabilization_model::StabilizationStatus::keeps_run_active))
    {
        run.status = AutoRunStatus::Paused;
        if run.pending_push.is_some() {
            run.stabilization_status = Some(stabilization_model::StabilizationStatus::Blocked);
            run.stabilization_blocker =
                Some(stabilization_model::StabilizationBlocker::PendingPush);
            run.stabilization_next_work =
                Some(stabilization_model::StabilizationWorkKind::PushPendingRepair);
        }
        run.updated_unix_ms = unix_ms();
        save_run_with_conn(conn, &run)?;
    }
    let steps = load_steps_with_conn(conn, run_id)?;
    Ok(Some(PersistedAutoRun { run, steps }))
}

pub fn load_recent_active_runs_for_repo(
    conn: &rusqlite::Connection,
    repo_root: &Path,
    limit: usize,
) -> Result<Vec<PersistedAutoRun>, String> {
    let mut statement = conn
        .prepare(
            "select id
             from auto_run
             where repo_root = ?1
               and archived_unix_ms is null
               and (status in ('queued', 'running', 'paused', 'failed')
                    or pending_push_json is not null
                    or stabilization_status in ('observing', 'blocked', 'waiting', 'ready'))
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
        .map_err(|error| format!("prepare recent auto run load: {error}"))?;
    let ids = statement
        .query_map(
            params![repo_root.display().to_string(), usize_to_i64(limit)],
            |row| row.get::<_, String>(0),
        )
        .map_err(|error| format!("load recent auto run ids: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read recent auto run ids: {error}"))?;
    ids.into_iter()
        .filter_map(|id| load_auto_run(conn, &id).transpose())
        .collect()
}

pub(super) fn save_run_with_conn(conn: &rusqlite::Connection, run: &AutoRun) -> Result<(), String> {
    conn.execute(
        "insert into auto_run (
           id, harness_id, repo_root, worktree_path, worktree_incarnation, branch, mode, implementation_source, plan_path,
           plan_run_mode, variant, agent_profile, prompt_summary, initial_prompt, status, pause_requested,
           selected_step_run_id, pr_number, pr_url, current_head_sha, review_baseline_json,
           stabilization_status, stabilization_blocker, stabilization_next_work, pending_push_json,
             created_unix_ms, updated_unix_ms, archived_unix_ms, adapter_id
           ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29)
         on conflict(id) do update set
            repo_root = excluded.repo_root,
            harness_id = excluded.harness_id,
            adapter_id = excluded.adapter_id,
           worktree_path = excluded.worktree_path,
           worktree_incarnation = excluded.worktree_incarnation,
           branch = excluded.branch,
           mode = excluded.mode,
           implementation_source = excluded.implementation_source,
           plan_path = excluded.plan_path,
           plan_run_mode = excluded.plan_run_mode,
           variant = excluded.variant,
           agent_profile = excluded.agent_profile,
           prompt_summary = excluded.prompt_summary,
           initial_prompt = excluded.initial_prompt,
           status = excluded.status,
           pause_requested = excluded.pause_requested,
           selected_step_run_id = excluded.selected_step_run_id,
           pr_number = excluded.pr_number,
            pr_url = excluded.pr_url,
            current_head_sha = excluded.current_head_sha,
            review_baseline_json = excluded.review_baseline_json,
            stabilization_status = excluded.stabilization_status,
            stabilization_blocker = excluded.stabilization_blocker,
            stabilization_next_work = excluded.stabilization_next_work,
            pending_push_json = excluded.pending_push_json,
            updated_unix_ms = excluded.updated_unix_ms,
             archived_unix_ms = excluded.archived_unix_ms
           where auto_run.status != 'aborted' or excluded.status = 'queued'",
        params![
            run.id.as_str(),
            run.harness_id.as_str(),
            run.repo_root.as_str(),
            run.worktree_path.display().to_string(),
            run.worktree_incarnation.as_deref(),
            run.branch.as_str(),
            run.mode.as_str(),
            run.implementation_source.as_str(),
            run.plan_path.as_ref().map(|path| path.display().to_string()),
            plan_run_mode_label(run.plan_run_mode),
            run.variant.as_str(),
            run.agent_profile.as_deref(),
            run.prompt_summary.as_str(),
            run.initial_prompt.as_str(),
            run.status.as_str(),
            bool_to_i64(run.pause_requested),
            run.selected_step_run_id,
            run.pr_number.map(u64_to_i64),
            run.pr_url.as_deref(),
            run.current_head_sha.as_deref(),
            run.review_baseline_json.as_deref(),
            run.stabilization_status.map(|status| status.as_str()),
            run.stabilization_blocker.as_ref().map(|blocker| blocker.as_str()),
            run.stabilization_next_work.as_ref().map(|work| work.as_str()),
            optional_json(&run.pending_push)?,
            u64_to_i64(run.created_unix_ms),
            u64_to_i64(run.updated_unix_ms),
            run.archived_unix_ms.map(u64_to_i64),
            run.adapter_id.as_str(),
        ],
    )
    .map_err(|error| format!("write auto run: {error}"))?;
    emit_auto_run_log(run);
    Ok(())
}

pub(super) fn save_step_with_conn(
    conn: &rusqlite::Connection,
    step: &mut AutoStepRun,
) -> Result<i64, String> {
    if let Some(id) = step.id {
        conn.execute(
            "update auto_step_run
             set run_id = ?1,
                 sequence = ?2,
                 step_key = ?3,
                 reason = ?4,
                 status = ?5,
                 attempt = ?6,
                 started_unix_ms = ?7,
                 finished_unix_ms = ?8,
                  execution_state = ?9,
                  session_endpoint = ?10,
                  session_id = ?11,
                  execution_process_id = ?12,
                  plan_run_id = ?13,
                  commit_sha = ?14,
                  head_sha = ?15,
                  work_guard_json = ?16,
                  blocker = ?17,
                  summary = ?18,
                   error = ?19,
                   session_adapter_id = ?20,
                   execution_process_start_time_ticks = ?21
               where id = ?22 and (status != 'aborted' or ?5 = 'queued')",
            params![
                step.run_id.as_str(),
                usize_to_i64(step.sequence),
                step.step_key.as_str(),
                step.reason.as_deref(),
                step.status.as_str(),
                usize_to_i64(step.attempt),
                step.started_unix_ms.map(u64_to_i64),
                step.finished_unix_ms.map(u64_to_i64),
                step.execution.state.as_deref(),
                step.session.endpoint.as_deref(),
                step.session.id.as_deref(),
                step.execution.process_id.map(i64::from),
                step.plan_run_id.as_deref(),
                step.commit_sha.as_deref(),
                step.head_sha.as_deref(),
                optional_json(&step.work_guard)?,
                step.blocker.as_ref().map(|blocker| blocker.as_str()),
                step.summary.as_deref(),
                step.error.as_deref(),
                step.session.adapter_id.as_deref(),
                step.execution.process_start_time_ticks.map(u64_to_i64),
                id,
            ],
        )
        .map_err(|error| format!("write auto step run: {error}"))?;
        emit_auto_step_log(step);
        Ok(id)
    } else {
        conn.execute(
            "insert into auto_step_run (
               run_id, sequence, step_key, reason, status, attempt, started_unix_ms,
               finished_unix_ms, execution_state, session_endpoint, session_id, execution_process_id,
                 plan_run_id, commit_sha, head_sha, work_guard_json, blocker, summary, error,
                  session_adapter_id, execution_process_start_time_ticks
                ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)",
            params![
                step.run_id.as_str(),
                usize_to_i64(step.sequence),
                step.step_key.as_str(),
                step.reason.as_deref(),
                step.status.as_str(),
                usize_to_i64(step.attempt),
                step.started_unix_ms.map(u64_to_i64),
                step.finished_unix_ms.map(u64_to_i64),
                step.execution.state.as_deref(),
                step.session.endpoint.as_deref(),
                step.session.id.as_deref(),
                step.execution.process_id.map(i64::from),
                step.plan_run_id.as_deref(),
                step.commit_sha.as_deref(),
                step.head_sha.as_deref(),
                optional_json(&step.work_guard)?,
                step.blocker.as_ref().map(|blocker| blocker.as_str()),
                step.summary.as_deref(),
                step.error.as_deref(),
                step.session.adapter_id.as_deref(),
                step.execution.process_start_time_ticks.map(u64_to_i64),
            ],
        )
        .map_err(|error| format!("write auto step run: {error}"))?;
        let id = conn.last_insert_rowid();
        step.id = Some(id);
        emit_auto_step_log(step);
        Ok(id)
    }
}

pub(super) fn load_run_with_conn(
    conn: &rusqlite::Connection,
    run_id: &str,
) -> Result<Option<AutoRun>, String> {
    conn.query_row(
        "select id, harness_id, repo_root, worktree_path, worktree_incarnation, branch, mode, implementation_source, plan_path,
                plan_run_mode, variant, agent_profile, prompt_summary, initial_prompt, status, pause_requested,
                 selected_step_run_id, pr_number, pr_url, current_head_sha, review_baseline_json,
                 stabilization_status, stabilization_blocker, stabilization_next_work, pending_push_json,
                  created_unix_ms, updated_unix_ms, archived_unix_ms, adapter_id
         from auto_run
         where id = ?1",
        params![run_id],
        |row| {
            let mode: String = row.get(6)?;
            let implementation_source: String = row.get(7)?;
            let plan_run_mode: String = row.get(9)?;
            let status: String = row.get(14)?;
            Ok(AutoRun {
                id: row.get(0)?,
                harness_id: row.get(1)?,
                adapter_id: row.get(28)?,
                repo_root: row.get(2)?,
                worktree_path: PathBuf::from(row.get::<_, String>(3)?),
                worktree_incarnation: row.get(4)?,
                branch: row.get(5)?,
                mode: AutoRunMode::parse(&mode).map_err(from_string_error)?,
                implementation_source: AutoImplementationSource::parse(&implementation_source)
                    .map_err(from_string_error)?,
                plan_path: row.get::<_, Option<String>>(8)?.map(PathBuf::from),
                plan_run_mode: parse_plan_run_mode(&plan_run_mode).map_err(from_string_error)?,
                variant: row.get(10)?,
                agent_profile: row.get(11)?,
                prompt_summary: row.get(12)?,
                initial_prompt: row.get(13)?,
                status: AutoRunStatus::parse(&status).map_err(from_string_error)?,
                pause_requested: row.get::<_, i64>(15)? != 0,
                selected_step_run_id: row.get(16)?,
                pr_number: row
                    .get::<_, Option<i64>>(17)?
                    .map(|value| value.max(0) as u64),
                pr_url: row.get(18)?,
                current_head_sha: row.get(19)?,
                review_baseline_json: row.get(20)?,
                stabilization_status: optional_stabilization_status(row.get(21)?)?,
                stabilization_blocker: optional_stabilization_blocker(row.get(22)?)?,
                stabilization_next_work: optional_stabilization_work_kind(row.get(23)?)?,
                pending_push: optional_json_value(row.get::<_, Option<String>>(24)?)?,
                created_unix_ms: i64_to_u64(row.get(25)?, 25),
                updated_unix_ms: i64_to_u64(row.get(26)?, 26),
                archived_unix_ms: row
                    .get::<_, Option<i64>>(27)?
                    .map(|value| value.max(0) as u64),
            })
        },
    )
    .optional()
    .map_err(|error| format!("load auto run: {error}"))
}

pub(super) fn load_steps_with_conn(
    conn: &rusqlite::Connection,
    run_id: &str,
) -> Result<Vec<AutoStepRun>, String> {
    let mut statement = conn
        .prepare(
            "select id, run_id, sequence, step_key, reason, status, attempt, started_unix_ms,
                    finished_unix_ms, execution_state, session_endpoint, session_id, execution_process_id,
                    plan_run_id, commit_sha, head_sha, work_guard_json, blocker, summary, error,
                    session_adapter_id, execution_process_start_time_ticks
             from auto_step_run
             where run_id = ?1
             order by sequence",
        )
        .map_err(|error| format!("prepare auto step load: {error}"))?;
    let rows = statement
        .query_map(params![run_id], |row| {
            let step_key: String = row.get(3)?;
            let status: String = row.get(5)?;
            Ok(AutoStepRun {
                id: row.get(0)?,
                run_id: row.get(1)?,
                sequence: i64_to_usize(row.get(2)?, 2),
                step_key: AutoStepKey::parse(&step_key),
                reason: row.get(4)?,
                status: AutoStepStatus::parse(&status).map_err(from_string_error)?,
                attempt: i64_to_usize(row.get(6)?, 6),
                started_unix_ms: row
                    .get::<_, Option<i64>>(7)?
                    .map(|value| value.max(0) as u64),
                finished_unix_ms: row
                    .get::<_, Option<i64>>(8)?
                    .map(|value| value.max(0) as u64),
                execution: crate::harness::ExecutionRef {
                    state: row.get(9)?,
                    process_id: row
                        .get::<_, Option<i64>>(12)?
                        .map(|value| value.max(0) as u32),
                    process_start_time_ticks: row
                        .get::<_, Option<i64>>(21)?
                        .map(|value| value.max(0) as u64),
                },
                session: crate::harness::SessionRef {
                    adapter_id: row.get(20)?,
                    endpoint: row.get(10)?,
                    id: row.get(11)?,
                },
                plan_run_id: row.get(13)?,
                commit_sha: row.get(14)?,
                head_sha: row.get(15)?,
                work_guard: optional_json_value(row.get::<_, Option<String>>(16)?)?,
                blocker: optional_stabilization_blocker(row.get(17)?)?,
                summary: row.get(18)?,
                error: row.get(19)?,
            })
        })
        .map_err(|error| format!("load auto steps: {error}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read auto steps: {error}"))
}

fn reset_incompatible_active_runs(conn: &rusqlite::Connection) -> Result<(), String> {
    let version = conn
        .query_row(
            "select version from auto_schema_version where id = 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|error| format!("read auto schema version: {error}"))?
        .unwrap_or(0);
    if version >= 4 {
        return Ok(());
    }

    let now = u64_to_i64(unix_ms());
    conn.execute(
        "update auto_step_run
         set status = 'aborted',
             finished_unix_ms = coalesce(finished_unix_ms, ?1),
             error = coalesce(error, 'Archived during PR Stabilization persistence migration')
         where run_id in (
           select id from auto_run
           where archived_unix_ms is null
             and status in ('queued', 'running', 'paused', 'failed')
         )
           and status in ('queued', 'starting', 'running', 'waiting', 'failed')",
        params![now],
    )
    .map_err(|error| format!("archive incompatible auto steps: {error}"))?;
    conn.execute(
        "update auto_run
         set status = 'aborted',
             archived_unix_ms = coalesce(archived_unix_ms, ?1),
             updated_unix_ms = ?1
         where archived_unix_ms is null
           and status in ('queued', 'running', 'paused', 'failed')",
        params![now],
    )
    .map_err(|error| format!("archive incompatible auto runs: {error}"))?;
    Ok(())
}

fn optional_json<T: Serialize>(value: &Option<T>) -> Result<Option<String>, String> {
    value
        .as_ref()
        .map(|value| {
            serde_json::to_string(value).map_err(|error| format!("serialize auto json: {error}"))
        })
        .transpose()
}

fn optional_json_value<T: for<'de> Deserialize<'de>>(
    value: Option<String>,
) -> Result<Option<T>, rusqlite::Error> {
    value
        .map(|value| {
            serde_json::from_str(&value).map_err(|error| from_string_error(error.to_string()))
        })
        .transpose()
}

fn optional_stabilization_status(
    value: Option<String>,
) -> Result<Option<stabilization_model::StabilizationStatus>, rusqlite::Error> {
    value
        .map(|value| {
            stabilization_model::StabilizationStatus::parse(&value).map_err(from_string_error)
        })
        .transpose()
}

fn optional_stabilization_blocker(
    value: Option<String>,
) -> Result<Option<stabilization_model::StabilizationBlocker>, rusqlite::Error> {
    value
        .map(|value| {
            stabilization_model::StabilizationBlocker::parse(&value).map_err(from_string_error)
        })
        .transpose()
}

fn optional_stabilization_work_kind(
    value: Option<String>,
) -> Result<Option<stabilization_model::StabilizationWorkKind>, rusqlite::Error> {
    value
        .map(|value| {
            stabilization_model::StabilizationWorkKind::parse(&value).map_err(from_string_error)
        })
        .transpose()
}

pub(super) fn from_string_error(error: String) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(error.into())
}

pub(super) fn i64_to_usize(value: i64, index: usize) -> usize {
    usize::try_from(value)
        .unwrap_or_else(|_| panic!("SQLite column {index} contained invalid usize: {value}"))
}

pub(super) fn i64_to_u64(value: i64, index: usize) -> u64 {
    u64::try_from(value)
        .unwrap_or_else(|_| panic!("SQLite column {index} contained invalid u64: {value}"))
}

pub(super) fn i64_to_next_u64(value: i64) -> u64 {
    value.max(0) as u64 + 1
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

pub(super) fn table_has_column(
    conn: &rusqlite::Connection,
    table: &str,
    column: &str,
) -> Result<bool, String> {
    let mut statement = conn
        .prepare(&format!("pragma table_info({table})"))
        .map_err(|error| format!("prepare table info: {error}"))?;
    let mut rows = statement
        .query([])
        .map_err(|error| format!("read table info: {error}"))?;
    while let Some(row) = rows
        .next()
        .map_err(|error| format!("read column info: {error}"))?
    {
        let name = row
            .get::<_, String>(1)
            .map_err(|error| format!("read column name: {error}"))?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}
