use super::*;

pub const ARCHIVED_PLAN_RETENTION_MS: u64 = 60 * 60 * 24 * 30 * 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanRunControlIntent {
    Pause,
    Resume,
    AbortRun,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanRunControlEffect {
    PauseRequested,
    Paused,
    Resumed,
    AbortedRun,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanExecutorDecision {
    Start,
    AlreadyRunning,
    DoNotStart,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanRunControlOutcome {
    pub run: PersistedPlanRun,
    pub effect: PlanRunControlEffect,
    pub executor: PlanExecutorDecision,
    pub warnings: Vec<String>,
}

pub fn apply_plan_run_control(
    conn: &rusqlite::Connection,
    run_id: &str,
    intent: PlanRunControlIntent,
) -> Result<PlanRunControlOutcome, String> {
    let mut persisted =
        load_plan_run(conn, run_id)?.ok_or_else(|| format!("plan run not found: {run_id}"))?;
    let mut warnings = Vec::new();
    let (effect, executor) = match intent {
        PlanRunControlIntent::Pause => {
            request_plan_run_pause(conn, &mut persisted)?;
            let effect = if persisted.run.status == PlanRunStatus::Paused {
                PlanRunControlEffect::Paused
            } else {
                PlanRunControlEffect::PauseRequested
            };
            (effect, PlanExecutorDecision::DoNotStart)
        }
        PlanRunControlIntent::Resume => {
            if !persisted.run.pause_requested && persisted.run.status != PlanRunStatus::Paused {
                return Err("plan run is not paused".to_string());
            }
            let should_execute =
                prepare_plan_run_for_resume(conn, &mut persisted, DEFAULT_OUTPUT_LINES_PER_STEP)?;
            (
                PlanRunControlEffect::Resumed,
                if should_execute {
                    PlanExecutorDecision::Start
                } else if persisted.run.status == PlanRunStatus::Running {
                    PlanExecutorDecision::AlreadyRunning
                } else {
                    PlanExecutorDecision::DoNotStart
                },
            )
        }
        PlanRunControlIntent::AbortRun => {
            if let Err(error) = abort_plan_run(conn, &mut persisted) {
                warnings.push(error);
            }
            (
                PlanRunControlEffect::AbortedRun,
                PlanExecutorDecision::DoNotStart,
            )
        }
    };
    Ok(PlanRunControlOutcome {
        run: persisted,
        effect,
        executor,
        warnings,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RecordedProcessState {
    Missing(Option<u32>),
    Same(u32),
    Reused(u32),
    Unverifiable(u32),
}

pub fn prepare_plan_run_for_resume(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedPlanRun,
    max_output_lines_per_step: usize,
) -> Result<bool, String> {
    let mut changed = false;
    let mut has_live_child = false;
    for step in &mut persisted.steps {
        if !matches!(
            step.status,
            PlanStepStatus::Starting | PlanStepStatus::Running
        ) {
            continue;
        }
        if step.execution.process_id.is_some() {
            match recorded_process_state(
                step.execution.process_id,
                step.execution.process_identity,
            )? {
                RecordedProcessState::Same(_) | RecordedProcessState::Unverifiable(_) => {
                    has_live_child = true;
                    continue;
                }
                RecordedProcessState::Missing(_) | RecordedProcessState::Reused(_) => {}
            }
        }
        let message = format!(
            "phase {} was interrupted before completion and was queued for resume",
            step.step
        );
        reset_step_for_retry(step);
        append_system_output(
            conn,
            step,
            PlanOutputKind::System,
            &message,
            max_output_lines_per_step,
        )?;
        save_step_with_conn(conn, step)?;
        changed = true;
    }
    if has_live_child {
        return Ok(false);
    }
    if persisted.run.pause_requested || persisted.run.status == PlanRunStatus::Paused || changed {
        persisted.run.pause_requested = false;
        persisted.run.status = persisted.aggregate_status();
        persisted.run.updated_unix_ms = unix_ms();
        save_run_with_conn(conn, &persisted.run)?;
    }
    Ok(true)
}

pub fn abort_plan_step(conn: &rusqlite::Connection, step: &mut PlanStepRun) -> Result<(), String> {
    let mut errors = Vec::new();
    if step.session.id.is_some()
        && let Err(error) = crate::harness::cancel_native_session(&step.session)
    {
        errors.push(error);
    }
    if let Some(process_id) = step.execution.process_id
        && let Err(error) =
            terminate_recorded_process(process_id, step.execution.process_identity, "plan phase")
    {
        errors.push(error);
    }
    step.status = PlanStepStatus::Aborted;
    step.execution.process_id = None;
    step.execution.process_identity = None;
    step.finished_unix_ms = Some(unix_ms());
    step.error = if errors.is_empty() {
        Some("aborted".to_string())
    } else {
        Some(format!("aborted with errors: {}", errors.join("; ")))
    };
    save_step_with_conn(conn, step)?;
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

pub fn abort_plan_run(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedPlanRun,
) -> Result<(), String> {
    let mut errors = Vec::new();
    for step in &mut persisted.steps {
        match step.status {
            PlanStepStatus::Starting | PlanStepStatus::Running => {
                if let Err(error) = abort_plan_step(conn, step) {
                    errors.push(format!("step {}: {error}", step.step));
                }
            }
            PlanStepStatus::Queued => {
                step.status = PlanStepStatus::Aborted;
                step.finished_unix_ms = Some(unix_ms());
                step.error = Some("aborted".to_string());
            }
            PlanStepStatus::Done
            | PlanStepStatus::Failed
            | PlanStepStatus::Aborted
            | PlanStepStatus::Skipped => {}
        }
    }
    persisted.run.status = persisted.aggregate_status();
    persisted.run.pause_requested = false;
    persisted.run.updated_unix_ms = unix_ms();
    save_plan_run(conn, persisted)?;
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

pub fn reconcile_stale_plan_run(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedPlanRun,
    max_output_lines_per_step: usize,
) -> Result<bool, String> {
    reconcile_stale_plan_run_with_observer(
        conn,
        persisted,
        max_output_lines_per_step,
        recorded_process_state,
    )
}

pub(super) fn reconcile_stale_plan_run_with_observer(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedPlanRun,
    max_output_lines_per_step: usize,
    mut observe: impl FnMut(Option<u32>, Option<u64>) -> Result<RecordedProcessState, String>,
) -> Result<bool, String> {
    let mut changed = false;
    let mut changed_run_status = false;
    let repo_root = persisted.run.repo_root.clone();
    let run_id = persisted.run.id.clone();
    for step in &mut persisted.steps {
        if !matches!(
            step.status,
            PlanStepStatus::Starting | PlanStepStatus::Running
        ) {
            continue;
        }
        match observe(step.execution.process_id, step.execution.process_identity)? {
            RecordedProcessState::Same(process_id)
            | RecordedProcessState::Unverifiable(process_id) => {
                if reconcile_plan_step_from_server(conn, step, max_output_lines_per_step)
                    .unwrap_or(false)
                {
                    changed = true;
                }
                let message = format!(
                    "Prism restarted while phase {} was running in process {process_id}; stdout cannot be reattached, so Prism is showing persisted state until new OpenCode status is available.",
                    step.step
                );
                if append_unique_system_output(
                    conn,
                    step,
                    PlanOutputKind::System,
                    &message,
                    max_output_lines_per_step,
                )? {
                    changed = true;
                }
                append_stale_reconciliation_log(
                    &repo_root,
                    &run_id,
                    step,
                    "kept-running-live-process",
                );
            }
            RecordedProcessState::Reused(process_id) => {
                let message = format!(
                    "Prism restarted while phase {} was running, and process id {process_id} now belongs to a different process.",
                    step.step
                );
                mark_stale_step_failed(conn, step, &message, max_output_lines_per_step)?;
                changed = true;
                changed_run_status = true;
                append_stale_reconciliation_log(&repo_root, &run_id, step, "failed-reused-process");
            }
            RecordedProcessState::Missing(process_id) => {
                let message = process_id.map_or_else(
                    || {
                        format!(
                            "Prism restarted while phase {} was marked running, but no child process id was recorded.",
                            step.step
                        )
                    },
                    |process_id| {
                        format!(
                            "Prism restarted while phase {} was running, and recorded process {process_id} is no longer running.",
                            step.step
                        )
                    },
                );
                mark_stale_step_failed(conn, step, &message, max_output_lines_per_step)?;
                changed = true;
                changed_run_status = true;
                append_stale_reconciliation_log(
                    &repo_root,
                    &run_id,
                    step,
                    "failed-missing-process",
                );
            }
        }
    }
    if changed_run_status
        && matches!(
            persisted.run.status,
            PlanRunStatus::Queued | PlanRunStatus::Running | PlanRunStatus::Paused
        )
    {
        persisted.run.status = persisted.aggregate_status();
        persisted.run.updated_unix_ms = unix_ms();
        save_run_with_conn(conn, &persisted.run)?;
        changed = true;
    }
    Ok(changed)
}

pub(super) fn mark_stale_step_failed(
    conn: &rusqlite::Connection,
    step: &mut PlanStepRun,
    message: &str,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    step.status = PlanStepStatus::Failed;
    step.execution.process_id = None;
    step.finished_unix_ms = Some(unix_ms());
    step.error = Some(message.to_string());
    append_unique_system_output(
        conn,
        step,
        PlanOutputKind::Error,
        message,
        max_output_lines_per_step,
    )?;
    save_step_with_conn(conn, step)
}

pub(super) fn recorded_process_state(
    process_id: Option<u32>,
    stored_identity: Option<u64>,
) -> Result<RecordedProcessState, String> {
    let Some(process_id) = process_id else {
        return Ok(RecordedProcessState::Missing(None));
    };
    let recorded = crate::process::RecordedProcess::from_stored(process_id, stored_identity);
    crate::process::observe_process(recorded)
        .map(|observation| recorded_process_state_from_observation(process_id, observation))
        .map_err(|error| format!("inspect plan phase process {process_id}: {error}"))
}

pub(super) fn recorded_process_state_from_observation(
    process_id: u32,
    observation: crate::process::ProcessObservation,
) -> RecordedProcessState {
    match observation {
        crate::process::ProcessObservation::RunningSameProcess => {
            RecordedProcessState::Same(process_id)
        }
        crate::process::ProcessObservation::Missing => {
            RecordedProcessState::Missing(Some(process_id))
        }
        crate::process::ProcessObservation::IdentityReused => {
            RecordedProcessState::Reused(process_id)
        }
        crate::process::ProcessObservation::RunningUnverifiable => {
            RecordedProcessState::Unverifiable(process_id)
        }
    }
}

fn terminate_recorded_process(
    process_id: u32,
    stored_identity: Option<u64>,
    label: &str,
) -> Result<(), String> {
    let recorded = crate::process::RecordedProcess::from_stored(process_id, stored_identity);
    match crate::process::terminate_recorded_process(recorded, std::time::Duration::from_secs(1))
        .map_err(|error| format!("terminate {label} process {process_id}: {error}"))?
    {
        crate::process::TerminationOutcome::Terminated
        | crate::process::TerminationOutcome::AlreadyExited
        | crate::process::TerminationOutcome::IdentityReused => Ok(()),
        crate::process::TerminationOutcome::Unverifiable => Err(format!(
            "refusing to terminate {label} process {process_id}: reusable process identity is unavailable"
        )),
    }
}

pub(super) fn append_unique_system_output(
    conn: &rusqlite::Connection,
    step: &PlanStepRun,
    kind: PlanOutputKind,
    text: &str,
    max_output_lines_per_step: usize,
) -> Result<bool, String> {
    if output_line_exists(conn, &step.run_id, step.step, kind, text)? {
        return Ok(false);
    }
    append_system_output(conn, step, kind, text, max_output_lines_per_step)?;
    Ok(true)
}

pub(super) fn output_line_exists(
    conn: &rusqlite::Connection,
    run_id: &str,
    step: usize,
    kind: PlanOutputKind,
    text: &str,
) -> Result<bool, String> {
    let exists: i64 = conn
        .query_row(
            "select exists(
               select 1 from plan_output_line
               where run_id = ?1 and step = ?2 and kind = ?3 and text = ?4
             )",
            params![run_id, usize_to_i64(step), kind.as_str(), text],
            |row| row.get(0),
        )
        .map_err(|error| format!("check plan output line existence: {error}"))?;
    Ok(exists != 0)
}

pub(super) fn append_stale_reconciliation_log(
    repo_root: &str,
    run_id: &str,
    step: &PlanStepRun,
    transition: &str,
) {
    let repo = Repository {
        root: PathBuf::from(repo_root),
    };
    let server_url = step.session.endpoint.as_deref().unwrap_or("none");
    let session_id = step.session.id.as_deref().unwrap_or("none");
    let process_id = step
        .execution
        .process_id
        .map(|process_id| process_id.to_string())
        .unwrap_or_else(|| "none".to_string());
    let _ = crate::observability::append_runtime_message(
        &repo,
        &format!(
            "plan stale reconciliation run_id={run_id} step={} process_id={process_id} server_url={server_url} session_id={session_id} transition={transition}",
            step.step
        ),
    );
}

pub fn retry_failed_steps(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedPlanRun,
) -> Result<(), String> {
    let mut first = None;
    for step in &mut persisted.steps {
        if matches!(
            step.status,
            PlanStepStatus::Failed | PlanStepStatus::Aborted
        ) {
            reset_step_for_retry(step);
            first.get_or_insert(step.step);
            save_step_with_conn(conn, step)?;
        }
    }
    if first.is_none() {
        return Err("plan run has no failed phases to retry".to_string());
    }
    persisted.run.selected_step = first.unwrap_or(persisted.run.selected_step);
    persisted.run.status = persisted.aggregate_status();
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)
}

pub fn retry_from_step(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedPlanRun,
    selected_step: usize,
) -> Result<(), String> {
    let mut found = false;
    for step in &mut persisted.steps {
        if step.step < selected_step {
            continue;
        }
        found = true;
        reset_step_for_retry(step);
        save_step_with_conn(conn, step)?;
    }
    if !found {
        return Err(format!("plan phase not found: {selected_step}"));
    }
    persisted.run.selected_step = selected_step;
    persisted.run.status = persisted.aggregate_status();
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)
}

pub fn skip_plan_step(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedPlanRun,
    selected_step: usize,
) -> Result<(), String> {
    let step = persisted
        .steps
        .iter_mut()
        .find(|step| step.step == selected_step)
        .ok_or_else(|| format!("plan phase not found: {selected_step}"))?;
    if matches!(
        step.status,
        PlanStepStatus::Starting | PlanStepStatus::Running
    ) {
        return Err(format!("plan phase {selected_step} is running"));
    }
    step.status = PlanStepStatus::Skipped;
    step.execution.process_id = None;
    step.finished_unix_ms = Some(unix_ms());
    step.error = None;
    step.active_tool = None;
    append_system_output(
        conn,
        step,
        PlanOutputKind::System,
        "phase skipped",
        DEFAULT_OUTPUT_LINES_PER_STEP,
    )?;
    save_step_with_conn(conn, step)?;
    persisted.run.status = persisted.aggregate_status();
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)
}

pub fn request_plan_run_pause(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedPlanRun,
) -> Result<(), String> {
    if matches!(
        persisted.run.status,
        PlanRunStatus::Done | PlanRunStatus::Failed | PlanRunStatus::Aborted
    ) {
        return Err("cannot pause a completed plan run".to_string());
    }
    persisted.run.pause_requested = true;
    if !persisted.steps.iter().any(|step| {
        matches!(
            step.status,
            PlanStepStatus::Starting | PlanStepStatus::Running
        )
    }) {
        persisted.run.status = PlanRunStatus::Paused;
    }
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)
}

pub fn resume_paused_plan_run(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedPlanRun,
) -> Result<(), String> {
    if !persisted.run.pause_requested && persisted.run.status != PlanRunStatus::Paused {
        return Err("plan run is not paused".to_string());
    }
    persisted.run.pause_requested = false;
    persisted.run.status = persisted.aggregate_status();
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)
}

pub fn archive_plan_run(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedPlanRun,
) -> Result<(), String> {
    if matches!(
        persisted.run.status,
        PlanRunStatus::Queued | PlanRunStatus::Running | PlanRunStatus::Paused
    ) {
        return Err("cannot dismiss a queued or running plan run".to_string());
    }
    let now = unix_ms();
    persisted.run.archived_unix_ms = Some(now);
    persisted.run.updated_unix_ms = now;
    save_run_with_conn(conn, &persisted.run)
}

pub fn cleanup_stale_archived_plan_runs(
    conn: &rusqlite::Connection,
    retention_ms: u64,
) -> Result<usize, String> {
    let cutoff = unix_ms().saturating_sub(retention_ms);
    conn.execute(
        "delete from plan_run
         where archived_unix_ms is not null and archived_unix_ms <= ?1",
        params![u64_to_i64(cutoff)],
    )
    .map_err(|error| format!("cleanup archived plan runs: {error}"))
}

pub(super) fn reload_pause_request(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedPlanRun,
) -> Result<bool, String> {
    let Some(run) = load_run_with_conn(conn, &persisted.run.id)? else {
        return Ok(false);
    };
    persisted.run.pause_requested = run.pause_requested;
    if run.pause_requested || run.status == PlanRunStatus::Paused {
        persisted.run.status = PlanRunStatus::Paused;
        persisted.run.updated_unix_ms = unix_ms();
        save_run_with_conn(conn, &persisted.run)?;
        return Ok(true);
    }
    Ok(false)
}

pub(super) fn reset_step_for_retry(step: &mut PlanStepRun) {
    step.status = PlanStepStatus::Queued;
    step.execution = crate::harness::ExecutionRef::default();
    step.session = crate::harness::SessionRef::default();
    step.agent_variant = None;
    step.started_unix_ms = None;
    step.finished_unix_ms = None;
    step.exit_code = None;
    step.latest_message = None;
    step.active_tool = None;
    step.todos.clear();
    step.summary = None;
    step.error = None;
}
