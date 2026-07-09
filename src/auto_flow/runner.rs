use super::*;

pub fn execute_auto_initial_step(
    conn: &rusqlite::Connection,
    repo: &Repository,
    config: &Config,
    persisted: &mut PersistedAutoRun,
    executor: &AutoExecutorConfig,
    output: &mut dyn Write,
) -> Result<(), String> {
    persisted.run.pause_requested = false;
    persisted.run.status = AutoRunStatus::Running;
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)?;

    complete_queued_prepare(conn, persisted, executor.max_output_lines_per_step)?;
    if !persisted.steps.iter().any(|step| {
        matches!(
            step.step_key,
            AutoStepKey::CreatePlan
                | AutoStepKey::ReviewPlan
                | AutoStepKey::RunPlan
                | AutoStepKey::Implement
                | AutoStepKey::FixLocalVerify
                | AutoStepKey::FixReview
                | AutoStepKey::FixCi
        )
    }) {
        let (step_key, reason) = initial_agent_step(persisted);
        append_step_run(conn, persisted, step_key, Some(reason.to_string()))?;
    }

    if reload_pause_request(conn, persisted)? {
        return Ok(());
    }

    loop {
        if reload_pause_request(conn, persisted)? {
            return Ok(());
        }

        if let Some(step_index) = next_queued_agent_step(persisted) {
            if let Err(error) =
                execute_one_agent_step(conn, persisted, step_index, executor, output)
            {
                persisted.run.status = AutoRunStatus::Failed;
                persisted.run.pause_requested = false;
                persisted.run.updated_unix_ms = unix_ms();
                save_run_with_conn(conn, &persisted.run)?;
                return Err(error);
            }
            pause_before_next_auto_step_with_context(conn, repo, config, persisted)?;
            continue;
        }

        if let Some(step_index) = next_queued_non_agent_step(persisted) {
            if let Err(error) =
                execute_one_non_agent_step(conn, repo, config, persisted, step_index, executor)
            {
                persisted.run.status = AutoRunStatus::Failed;
                persisted.run.pause_requested = false;
                persisted.run.updated_unix_ms = unix_ms();
                save_run_with_conn(conn, &persisted.run)?;
                return Err(error);
            }
            pause_before_next_auto_step_with_context(conn, repo, config, persisted)?;
            continue;
        }

        if ensure_next_auto_step_with_context(conn, repo, config, persisted)? {
            continue;
        }

        persisted.run.pause_requested = false;
        persisted.run.status = persisted.aggregate_status();
        if matches!(persisted.run.status, AutoRunStatus::Queued) {
            persisted.run.status = AutoRunStatus::Paused;
        }
        persisted.run.updated_unix_ms = unix_ms();
        save_run_with_conn(conn, &persisted.run)?;
        return Ok(());
    }
}

pub(super) fn complete_queued_prepare(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    for step in &mut persisted.steps {
        if step.step_key != AutoStepKey::Prepare || step.status != AutoStepStatus::Queued {
            continue;
        }
        let now = unix_ms();
        step.status = AutoStepStatus::Done;
        step.started_unix_ms = Some(now);
        step.finished_unix_ms = Some(now);
        step.summary = Some("prepared worktree for headless execution".to_string());
        let step_id = save_step_with_conn(conn, step)?;
        append_system_output(
            conn,
            step_id,
            AutoOutputKind::System,
            "prepared worktree for headless execution",
            None,
            max_output_lines_per_step,
        )?;
    }
    persisted.run.status = persisted.aggregate_status();
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)
}

pub(super) fn queued_prepare_needs_initial_agent_step(persisted: &PersistedAutoRun) -> bool {
    persisted
        .steps
        .iter()
        .any(|step| step.step_key == AutoStepKey::Prepare && step.status == AutoStepStatus::Queued)
        && !persisted.steps.iter().any(|step| {
            matches!(
                step.step_key,
                AutoStepKey::CreatePlan | AutoStepKey::RunPlan | AutoStepKey::Implement
            )
        })
}
