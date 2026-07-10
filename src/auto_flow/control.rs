use super::*;

pub fn request_auto_run_pause(
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

pub fn resume_paused_auto_run(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
) -> Result<(), String> {
    if !persisted.run.pause_requested && persisted.run.status != AutoRunStatus::Paused {
        return Err("auto flow run is not paused".to_string());
    }
    persisted.run.pause_requested = false;
    persisted.run.status = persisted.aggregate_status();
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

pub fn retry_failed_auto_step(
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
        persisted.run.status = persisted.aggregate_status();
        persisted.run.updated_unix_ms = unix_ms();
        save_run_with_conn(conn, &persisted.run)?;
        return Ok(());
    }

    let step_key = persisted.steps[failed_index].step_key.clone();
    append_step_run(conn, persisted, step_key, Some("manual retry".to_string()))?;
    Ok(())
}

pub fn retry_auto_from_step(
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
    persisted.run.status = persisted.aggregate_status();
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

pub fn abort_auto_step(conn: &rusqlite::Connection, step: &mut AutoStepRun) -> Result<(), String> {
    let mut errors = Vec::new();
    if let (Some(server_url), Some(session_id)) = (
        step.opencode_server_url.as_deref(),
        step.opencode_session_id.as_deref(),
    ) && let Err(error) = crate::opencode::abort_session(server_url, session_id)
    {
        errors.push(error);
    }
    if let Some(process_id) = step.process_id
        && let Err(error) = terminate_process(process_id)
    {
        errors.push(error);
    }
    step.status = AutoStepStatus::Aborted;
    step.process_id = None;
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
        let message = match step.process_id {
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
        step.process_id = None;
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
    if changed
        && matches!(
            persisted.run.status,
            AutoRunStatus::Queued | AutoRunStatus::Running | AutoRunStatus::Paused
        )
    {
        persisted.run.pause_requested = false;
        persisted.run.status = persisted.aggregate_status();
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
        persisted.run.status = persisted.aggregate_status();
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
    let linked_changed = reconcile_linked_plan_runs(conn, persisted, max_output_lines_per_step)?;
    let changed = reconcile_stale_auto_run(conn, persisted)? || linked_changed;
    if matches!(persisted.run.status, AutoRunStatus::Paused) {
        persisted.run.pause_requested = false;
        persisted.run.status = persisted.aggregate_status();
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
    step.opencode_session_id = None;
    step.process_id = None;
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

#[cfg(unix)]
pub(super) fn terminate_process(process_id: u32) -> Result<(), String> {
    let result = unsafe { libc::kill(process_id as libc::pid_t, libc::SIGTERM) };
    if result == 0 {
        Ok(())
    } else {
        Err(format!(
            "terminate opencode process {process_id}: {}",
            std::io::Error::last_os_error()
        ))
    }
}

#[cfg(not(unix))]
pub(super) fn terminate_process(process_id: u32) -> Result<(), String> {
    Command::new("taskkill")
        .args(["/PID", &process_id.to_string(), "/T", "/F"])
        .status()
        .map_err(|error| format!("terminate opencode process {process_id}: {error}"))
        .and_then(|status| {
            if status.success() {
                Ok(())
            } else {
                Err(format!("terminate opencode process {process_id}: {status}"))
            }
        })
}
