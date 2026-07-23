use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoRunControlIntent {
    Pause,
    Resume,
    RetryFailed,
    RetryFromStep { step_run_id: i64 },
    AbortStep { step_run_id: i64 },
    AbortRun,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoRunControlEffect {
    PauseRequested,
    Paused,
    Resumed,
    RetriedFailed,
    RetriedFromStep { step_run_id: i64 },
    AbortedStep { step_run_id: i64 },
    AbortedRun,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoExecutorDecision {
    Start,
    AlreadyRunning,
    DoNotStart,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutoRunControlOutcome {
    pub run: PersistedAutoRun,
    pub effect: AutoRunControlEffect,
    pub executor: AutoExecutorDecision,
    pub warnings: Vec<String>,
}

pub fn apply_auto_run_control(
    conn: &rusqlite::Connection,
    run_id: &str,
    intent: AutoRunControlIntent,
) -> Result<AutoRunControlOutcome, String> {
    let mut persisted =
        load_auto_run(conn, run_id)?.ok_or_else(|| format!("auto flow run not found: {run_id}"))?;
    let mut warnings = Vec::new();
    let (effect, executor) = match intent {
        AutoRunControlIntent::Pause => {
            request_auto_run_pause(conn, &mut persisted)?;
            let effect = if persisted.run.status == AutoRunStatus::Paused {
                AutoRunControlEffect::Paused
            } else {
                AutoRunControlEffect::PauseRequested
            };
            (effect, AutoExecutorDecision::DoNotStart)
        }
        AutoRunControlIntent::Resume => {
            if !persisted.run.pause_requested && persisted.run.status != AutoRunStatus::Paused {
                return Err("auto flow run is not paused".to_string());
            }
            let should_execute =
                prepare_auto_run_for_resume(conn, &mut persisted, DEFAULT_OUTPUT_LINES_PER_STEP)?;
            (
                AutoRunControlEffect::Resumed,
                if should_execute {
                    AutoExecutorDecision::Start
                } else if persisted.run.status == AutoRunStatus::Running {
                    AutoExecutorDecision::AlreadyRunning
                } else {
                    AutoExecutorDecision::DoNotStart
                },
            )
        }
        AutoRunControlIntent::RetryFailed => {
            retry_failed_auto_step(conn, &mut persisted)?;
            (
                AutoRunControlEffect::RetriedFailed,
                AutoExecutorDecision::Start,
            )
        }
        AutoRunControlIntent::RetryFromStep { step_run_id } => {
            retry_auto_from_step(conn, &mut persisted, step_run_id)?;
            (
                AutoRunControlEffect::RetriedFromStep { step_run_id },
                AutoExecutorDecision::Start,
            )
        }
        AutoRunControlIntent::AbortStep { step_run_id } => {
            abort_selected_auto_step(conn, &mut persisted, step_run_id, &mut warnings)?;
            (
                AutoRunControlEffect::AbortedStep { step_run_id },
                AutoExecutorDecision::DoNotStart,
            )
        }
        AutoRunControlIntent::AbortRun => {
            abort_auto_run(conn, &mut persisted, &mut warnings)?;
            (
                AutoRunControlEffect::AbortedRun,
                AutoExecutorDecision::DoNotStart,
            )
        }
    };
    Ok(AutoRunControlOutcome {
        run: persisted,
        effect,
        executor,
        warnings,
    })
}

fn abort_selected_auto_step(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
    step_run_id: i64,
    warnings: &mut Vec<String>,
) -> Result<(), String> {
    let step = persisted
        .steps
        .iter_mut()
        .find(|step| step.id == Some(step_run_id))
        .ok_or_else(|| format!("auto flow step not found: {step_run_id}"))?;
    abort_linked_plan_run(conn, step, warnings)?;
    abort_step_recording_warning(conn, step, warnings);
    persisted.run.status = persisted.authoritative_status();
    persisted.run.updated_unix_ms = unix_ms();
    save_auto_run(conn, persisted)
}

fn abort_auto_run(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
    warnings: &mut Vec<String>,
) -> Result<(), String> {
    for step in &mut persisted.steps {
        if matches!(
            step.status,
            AutoStepStatus::Queued
                | AutoStepStatus::Starting
                | AutoStepStatus::Running
                | AutoStepStatus::Waiting
        ) {
            abort_linked_plan_run(conn, step, warnings)?;
            abort_step_recording_warning(conn, step, warnings);
        }
    }
    persisted.run.status = AutoRunStatus::Aborted;
    persisted.run.pause_requested = false;
    persisted.run.updated_unix_ms = unix_ms();
    save_auto_run(conn, persisted)
}

fn abort_linked_plan_run(
    conn: &rusqlite::Connection,
    step: &AutoStepRun,
    warnings: &mut Vec<String>,
) -> Result<(), String> {
    let Some(plan_run_id) = step.plan_run_id.as_deref() else {
        return Ok(());
    };
    let Some(mut plan_run) = load_plan_run(conn, plan_run_id)? else {
        warnings.push(format!("linked plan run not found: {plan_run_id}"));
        return Ok(());
    };
    if matches!(
        plan_run.run.status,
        PlanRunStatus::Done | PlanRunStatus::Failed | PlanRunStatus::Aborted
    ) {
        return Ok(());
    }
    if let Err(error) = abort_plan_run(conn, &mut plan_run) {
        warnings.push(format!("linked plan run {plan_run_id}: {error}"));
    }
    Ok(())
}

fn abort_step_recording_warning(
    conn: &rusqlite::Connection,
    step: &mut AutoStepRun,
    warnings: &mut Vec<String>,
) {
    if matches!(
        step.status,
        AutoStepStatus::Starting | AutoStepStatus::Running
    ) {
        if let Err(error) = abort_auto_step(conn, step) {
            warnings.push(error);
        }
    } else {
        step.status = AutoStepStatus::Aborted;
        step.finished_unix_ms = Some(unix_ms());
    }
}

pub(super) fn request_auto_run_pause(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
) -> Result<(), String> {
    if matches!(
        persisted.run.status,
        AutoRunStatus::Done | AutoRunStatus::Failed | AutoRunStatus::Aborted
    ) {
        return Err("cannot pause a completed auto flow run".to_string());
    }
    persisted.run.pause_requested = true;
    if !persisted.steps.iter().any(|step| {
        matches!(
            step.status,
            AutoStepStatus::Starting | AutoStepStatus::Running | AutoStepStatus::Waiting
        )
    }) {
        persisted.run.status = AutoRunStatus::Paused;
    }
    request_active_linked_plan_pause(conn, persisted)?;
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)
}

pub fn fail_auto_run(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
    error: impl Into<String>,
) -> Result<(), String> {
    persisted.run.pause_requested = false;
    persisted.run.status = AutoRunStatus::Failed;
    persisted.run.updated_unix_ms = unix_ms();
    let error = error.into();
    append_auto_event(
        conn,
        &AutoEvent {
            id: None,
            run_id: persisted.run.id.clone(),
            step_run_id: persisted.run.selected_step_run_id,
            time_unix_ms: persisted.run.updated_unix_ms,
            kind: "run_failed".to_string(),
            data_json: format!("{{\"error\":{}}}", json_string(&error)),
        },
    )?;
    save_run_with_conn(conn, &persisted.run)
}

pub(super) fn retry_failed_auto_step(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
) -> Result<(), String> {
    let failed_index = persisted
        .steps
        .iter()
        .rposition(|step| {
            matches!(
                step.status,
                AutoStepStatus::Failed | AutoStepStatus::Aborted
            )
        })
        .ok_or_else(|| "auto flow run has no failed step to retry".to_string())?;
    if persisted.steps[failed_index].step_key == AutoStepKey::RunPlan
        && let Some(plan_run_id) = persisted.steps[failed_index].plan_run_id.clone()
        && let Some(mut plan_run) = load_plan_run(conn, &plan_run_id)?
    {
        let _ = prepare_plan_run_for_resume(conn, &mut plan_run, DEFAULT_OUTPUT_LINES_PER_STEP)?;
        if plan_run.run.status == PlanRunStatus::Done {
            let summary = format!("plan run {} completed", plan_run.run.id);
            finish_non_agent_step(
                conn,
                &mut persisted.steps[failed_index],
                AutoStepStatus::Done,
                Some(summary.clone()),
                None,
            )?;
            append_step_status_output(
                conn,
                &persisted.steps[failed_index],
                &summary,
                DEFAULT_OUTPUT_LINES_PER_STEP,
            )?;
        } else {
            retry_plan_failed_steps(conn, &mut plan_run)?;
            reset_auto_step_for_retry(&mut persisted.steps[failed_index]);
            append_step_status_output(
                conn,
                &persisted.steps[failed_index],
                "retrying linked plan run failed phases",
                DEFAULT_OUTPUT_LINES_PER_STEP,
            )?;
            save_step_with_conn(conn, &mut persisted.steps[failed_index])?;
        }
        persisted.run.pause_requested = false;
        persisted.run.status = persisted.authoritative_status();
        persisted.run.updated_unix_ms = unix_ms();
        save_run_with_conn(conn, &persisted.run)?;
        return Ok(());
    }

    let step_key = persisted.steps[failed_index].step_key.clone();
    append_step_run(conn, persisted, step_key, Some("manual retry".to_string()))?;
    Ok(())
}

pub(super) fn retry_auto_from_step(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
    selected_step_run_id: i64,
) -> Result<(), String> {
    let selected_index = persisted
        .steps
        .iter()
        .position(|step| step.id == Some(selected_step_run_id))
        .ok_or_else(|| format!("auto flow step not found: {selected_step_run_id}"))?;
    let selected_sequence = persisted.steps[selected_index].sequence;
    if persisted.steps[selected_index].step_key == AutoStepKey::RunPlan
        && let Some(plan_run_id) = persisted.steps[selected_index].plan_run_id.clone()
        && let Some(mut plan_run) = load_plan_run(conn, &plan_run_id)?
    {
        let start_step = plan_run.run.start_step;
        retry_plan_from_step(conn, &mut plan_run, start_step)?;
    }
    for step in persisted
        .steps
        .iter_mut()
        .filter(|step| step.sequence >= selected_sequence)
    {
        reset_auto_step_for_retry(step);
        save_step_with_conn(conn, step)?;
    }
    persisted.run.selected_step_run_id = persisted.steps[selected_index].id;
    persisted.run.pause_requested = false;
    persisted.run.status = persisted.authoritative_status();
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)
}

pub fn archive_auto_run(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
) -> Result<(), String> {
    if matches!(
        persisted.run.status,
        AutoRunStatus::Queued | AutoRunStatus::Running | AutoRunStatus::Paused
    ) {
        return Err("cannot archive a queued or running auto flow run".to_string());
    }
    let now = unix_ms();
    persisted.run.archived_unix_ms = Some(now);
    persisted.run.updated_unix_ms = now;
    save_run_with_conn(conn, &persisted.run)
}

pub(super) fn abort_auto_step(
    conn: &rusqlite::Connection,
    step: &mut AutoStepRun,
) -> Result<(), String> {
    let mut errors = Vec::new();
    if step.session.id.is_some()
        && let Err(error) = crate::harness::cancel_native_session(&step.session)
    {
        errors.push(error);
    }
    if let Some(process_id) = step.execution.process_id
        && let Err(error) =
            crate::harness::terminate_process(process_id, step.execution.process_start_time_ticks)
    {
        errors.push(error);
    }
    step.status = AutoStepStatus::Aborted;
    step.execution.process_id = None;
    step.execution.process_start_time_ticks = None;
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

pub fn reconcile_stale_auto_run(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
) -> Result<bool, String> {
    let mut changed = false;
    for step in &mut persisted.steps {
        if !matches!(
            step.status,
            AutoStepStatus::Starting | AutoStepStatus::Running | AutoStepStatus::Waiting
        ) {
            continue;
        }
        if step.step_key == AutoStepKey::RunPlan && step.plan_run_id.is_some() {
            continue;
        }
        let message = match step.execution.process_id {
            Some(process_id) => format!(
                "Prism restarted while auto flow step {} attempt {} was active in process {process_id}; the attempt was marked failed for retry.",
                step.step_key.as_str(),
                step.attempt
            ),
            None => format!(
                "Prism restarted while auto flow step {} attempt {} was active, but no child process id was recorded.",
                step.step_key.as_str(),
                step.attempt
            ),
        };
        step.status = AutoStepStatus::Failed;
        step.execution.process_id = None;
        step.finished_unix_ms = Some(unix_ms());
        step.error = Some(message.clone());
        save_step_with_conn(conn, step)?;
        if let Some(step_run_id) = step.id {
            append_output_line(
                conn,
                &AutoOutputLine {
                    step_run_id,
                    line_number: next_output_line_number(conn, step_run_id)?,
                    time_unix_ms: unix_ms(),
                    kind: AutoOutputKind::Error,
                    text: message,
                    block_id: None,
                },
            )?;
        }
        changed = true;
    }
    if matches!(
        persisted.run.status,
        AutoRunStatus::Queued | AutoRunStatus::Running | AutoRunStatus::Paused
    ) {
        persisted.run.pause_requested = false;
        persisted.run.status = persisted.authoritative_status();
        persisted.run.updated_unix_ms = unix_ms();
        save_run_with_conn(conn, &persisted.run)?;
        changed = true;
    }
    Ok(changed)
}

pub(super) fn reconcile_linked_plan_runs(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
    max_output_lines_per_step: usize,
) -> Result<bool, String> {
    crate::plan_run::migrate_schema(conn)?;
    let mut changed = false;
    for index in 0..persisted.steps.len() {
        if persisted.steps[index].step_key != AutoStepKey::RunPlan {
            continue;
        }
        let Some(plan_run_id) = persisted.steps[index].plan_run_id.clone() else {
            continue;
        };
        let Some(mut plan_run) = load_plan_run(conn, &plan_run_id)? else {
            if matches!(
                persisted.steps[index].status,
                AutoStepStatus::Starting | AutoStepStatus::Running | AutoStepStatus::Waiting
            ) {
                let error = format!("linked plan run {plan_run_id} was not found");
                fail_step(
                    conn,
                    &mut persisted.steps[index],
                    &error,
                    max_output_lines_per_step,
                )?;
                changed = true;
            }
            continue;
        };
        if plan_run.run.pause_requested || plan_run.run.status == PlanRunStatus::Paused {
            resume_paused_plan_run(conn, &mut plan_run)?;
        }
        let can_resume =
            prepare_plan_run_for_resume(conn, &mut plan_run, max_output_lines_per_step)?;
        let before = persisted.steps[index].status;
        match plan_run.run.status {
            PlanRunStatus::Done => {
                if persisted.steps[index].status != AutoStepStatus::Done {
                    let summary = format!("plan run {} completed", plan_run.run.id);
                    finish_non_agent_step(
                        conn,
                        &mut persisted.steps[index],
                        AutoStepStatus::Done,
                        Some(summary),
                        None,
                    )?;
                }
            }
            PlanRunStatus::Failed | PlanRunStatus::Aborted => {
                if persisted.steps[index].status != AutoStepStatus::Failed {
                    let error = format!(
                        "plan run {} ended with status {}; inspect linked plan dashboard",
                        plan_run.run.id,
                        plan_run_status_label(plan_run.run.status)
                    );
                    finish_non_agent_step(
                        conn,
                        &mut persisted.steps[index],
                        AutoStepStatus::Failed,
                        Some("plan run failed".to_string()),
                        Some(error),
                    )?;
                }
            }
            PlanRunStatus::Paused => {
                if persisted.steps[index].status != AutoStepStatus::Waiting {
                    let summary = format!(
                        "plan run {} paused; resume linked plan run",
                        plan_run.run.id
                    );
                    set_auto_step_waiting(conn, &mut persisted.steps[index], summary)?;
                }
            }
            PlanRunStatus::Draft | PlanRunStatus::Queued => {
                if can_resume && persisted.steps[index].status != AutoStepStatus::Queued {
                    reset_auto_step_for_retry(&mut persisted.steps[index]);
                    save_step_with_conn(conn, &mut persisted.steps[index])?;
                }
            }
            PlanRunStatus::Running => {
                if can_resume {
                    reset_auto_step_for_retry(&mut persisted.steps[index]);
                    save_step_with_conn(conn, &mut persisted.steps[index])?;
                } else if persisted.steps[index].status != AutoStepStatus::Waiting {
                    let summary = format!(
                        "plan run {} is running; Auto Flow is waiting",
                        plan_run.run.id
                    );
                    set_auto_step_waiting(conn, &mut persisted.steps[index], summary)?;
                }
            }
        }
        changed |= persisted.steps[index].status != before;
    }
    if changed {
        persisted.run.status = persisted.authoritative_status();
        persisted.run.updated_unix_ms = unix_ms();
        save_run_with_conn(conn, &persisted.run)?;
    }
    Ok(changed)
}

pub fn prepare_auto_run_for_resume(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
    max_output_lines_per_step: usize,
) -> Result<bool, String> {
    let was_paused = persisted.run.pause_requested || persisted.run.status == AutoRunStatus::Paused;
    let linked_changed = reconcile_linked_plan_runs(conn, persisted, max_output_lines_per_step)?;
    let changed = reconcile_stale_auto_run(conn, persisted)? || linked_changed;
    if was_paused {
        persisted.run.pause_requested = false;
        persisted.run.status = persisted.authoritative_status();
        if matches!(persisted.run.status, AutoRunStatus::Done) {
            persisted.run.status = AutoRunStatus::Paused;
        }
        persisted.run.updated_unix_ms = unix_ms();
        save_run_with_conn(conn, &persisted.run)?;
    }
    if changed {
        append_auto_event(
            conn,
            &AutoEvent {
                id: None,
                run_id: persisted.run.id.clone(),
                step_run_id: persisted.run.selected_step_run_id,
                time_unix_ms: unix_ms(),
                kind: "resume_reconciled".to_string(),
                data_json: "{}".to_string(),
            },
        )?;
    }
    let has_queued_agent_step = persisted.steps.iter().any(|step| {
        step.status == AutoStepStatus::Queued
            && matches!(
                step.step_key,
                AutoStepKey::CreatePlan
                    | AutoStepKey::ReviewPlan
                    | AutoStepKey::RunPlan
                    | AutoStepKey::Implement
                    | AutoStepKey::FixLocalVerify
                    | AutoStepKey::FixReview
                    | AutoStepKey::FixCi
                    | AutoStepKey::Custom(_)
            )
    });
    if has_queued_agent_step
        || has_queued_non_agent_step(persisted)
        || queued_prepare_needs_initial_agent_step(persisted)
        || next_state_machine_step_needed(persisted)
        || implementation_follow_up_step_needed(persisted)
    {
        Ok(true)
    } else {
        let _ = max_output_lines_per_step;
        Ok(false)
    }
}

pub(super) fn reset_auto_step_for_retry(step: &mut AutoStepRun) {
    step.status = AutoStepStatus::Queued;
    step.started_unix_ms = None;
    step.finished_unix_ms = None;
    step.session = crate::harness::SessionRef::default();
    step.execution = crate::harness::ExecutionRef::default();
    step.commit_sha = None;
    step.head_sha = None;
    step.summary = None;
    step.error = None;
}

pub(super) fn request_active_linked_plan_pause(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
) -> Result<(), String> {
    for step in &persisted.steps {
        if step.step_key != AutoStepKey::RunPlan
            || !matches!(
                step.status,
                AutoStepStatus::Queued
                    | AutoStepStatus::Starting
                    | AutoStepStatus::Running
                    | AutoStepStatus::Waiting
            )
        {
            continue;
        }
        let Some(plan_run_id) = step.plan_run_id.as_deref() else {
            continue;
        };
        let Some(mut plan_run) = load_plan_run(conn, plan_run_id)? else {
            continue;
        };
        if !matches!(
            plan_run.run.status,
            PlanRunStatus::Done | PlanRunStatus::Failed | PlanRunStatus::Aborted
        ) {
            request_plan_run_pause(conn, &mut plan_run)?;
        }
    }
    Ok(())
}

pub(super) fn fail_step(
    conn: &rusqlite::Connection,
    step: &mut AutoStepRun,
    error: &str,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    step.status = AutoStepStatus::Failed;
    step.finished_unix_ms = Some(unix_ms());
    step.error = Some(error.to_string());
    let step_id = save_step_with_conn(conn, step)?;
    append_system_output(
        conn,
        step_id,
        AutoOutputKind::Error,
        error,
        None,
        max_output_lines_per_step,
    )
}

pub(super) fn reload_pause_request(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
) -> Result<bool, String> {
    let Some(run) = load_run_with_conn(conn, &persisted.run.id)? else {
        return Ok(false);
    };
    persisted.run.pause_requested = run.pause_requested;
    if run.pause_requested || run.status == AutoRunStatus::Paused {
        persisted.run.status = AutoRunStatus::Paused;
        persisted.run.updated_unix_ms = unix_ms();
        save_run_with_conn(conn, &persisted.run)?;
        return Ok(true);
    }
    Ok(false)
}
