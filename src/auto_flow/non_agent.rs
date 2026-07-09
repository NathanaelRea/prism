use super::*;

pub(super) fn execute_one_non_agent_step(
    conn: &rusqlite::Connection,
    repo: &Repository,
    config: &Config,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    executor: &AutoExecutorConfig,
) -> Result<(), String> {
    start_non_agent_step(conn, persisted, step_index)?;
    let max_output_lines_per_step = executor.max_output_lines_per_step;
    let result = match persisted.steps[step_index].step_key {
        AutoStepKey::ApprovePlan => {
            execute_approve_plan_step(conn, persisted, step_index, max_output_lines_per_step)
        }
        AutoStepKey::RunPlan => execute_run_plan_step(
            conn,
            repo,
            config,
            persisted,
            step_index,
            executor.server_url.clone(),
            max_output_lines_per_step,
        ),
        AutoStepKey::LocalVerify => execute_local_verify_step(
            conn,
            config,
            persisted,
            step_index,
            max_output_lines_per_step,
        ),
        AutoStepKey::CommitImpl => execute_commit_impl_step(
            conn,
            config,
            persisted,
            step_index,
            max_output_lines_per_step,
        ),
        AutoStepKey::PushPr => execute_push_pr_step(
            conn,
            repo,
            config,
            persisted,
            step_index,
            max_output_lines_per_step,
        ),
        AutoStepKey::WaitReview => execute_wait_review_step(
            conn,
            repo,
            config,
            persisted,
            step_index,
            max_output_lines_per_step,
        ),
        AutoStepKey::VerifyReviewFix => execute_verify_review_fix_step(
            conn,
            config,
            persisted,
            step_index,
            max_output_lines_per_step,
        ),
        AutoStepKey::CommitReviewFix => execute_commit_review_fix_step(
            conn,
            repo,
            config,
            persisted,
            step_index,
            max_output_lines_per_step,
        ),
        AutoStepKey::WaitCi => execute_wait_ci_step(
            conn,
            repo,
            config,
            persisted,
            step_index,
            max_output_lines_per_step,
        ),
        AutoStepKey::VerifyCiFix => execute_verify_ci_fix_step(
            conn,
            config,
            persisted,
            step_index,
            max_output_lines_per_step,
        ),
        AutoStepKey::CommitCiFix => execute_commit_ci_fix_step(
            conn,
            repo,
            config,
            persisted,
            step_index,
            max_output_lines_per_step,
        ),
        AutoStepKey::Merge => execute_merge_step(
            conn,
            repo,
            config,
            persisted,
            step_index,
            max_output_lines_per_step,
        ),
        AutoStepKey::Cleanup => execute_cleanup_step(
            conn,
            repo,
            config,
            persisted,
            step_index,
            max_output_lines_per_step,
        ),
        _ => Ok(()),
    };
    if let Err(error) = result {
        fail_step(
            conn,
            &mut persisted.steps[step_index],
            &error,
            max_output_lines_per_step,
        )?;
        return Err(error);
    }
    Ok(())
}

pub(super) fn execute_approve_plan_step(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    let step_id = persisted.steps[step_index]
        .id
        .ok_or_else(|| "auto plan approval step must be saved before output".to_string())?;
    let plan_path = plan_first_plan_path(&persisted.run);
    let summary = format!(
        "plan review complete; approve by resuming this Auto Flow after reviewing {}",
        plan_path.display()
    );
    append_system_output(
        conn,
        step_id,
        AutoOutputKind::Status,
        &summary,
        None,
        max_output_lines_per_step,
    )?;
    finish_non_agent_step(
        conn,
        &mut persisted.steps[step_index],
        AutoStepStatus::Done,
        Some(summary),
        None,
    )?;
    persisted.run.pause_requested = true;
    persisted.run.status = AutoRunStatus::Paused;
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)
}

pub(super) fn execute_run_plan_step(
    conn: &rusqlite::Connection,
    repo: &Repository,
    config: &Config,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    server_url: Option<String>,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    crate::plan_run::migrate_schema(conn)?;
    let step_id = persisted.steps[step_index]
        .id
        .ok_or_else(|| "auto run-plan step must be saved before output".to_string())?;
    let plan_path = auto_plan_path(&persisted.run)?;
    let execution = PlanExecution::prepare(
        &persisted.run.worktree_path,
        config,
        Some(plan_path.as_path()),
    )?;
    let mode = persisted.run.plan_run_mode;
    let launch = execution.launch(Path::new(&persisted.run.repo_root), mode)?;
    let mut plan_run = if let Some(plan_run_id) = persisted.steps[step_index].plan_run_id.as_deref()
    {
        load_plan_run(conn, plan_run_id)?.ok_or_else(|| {
            format!("linked plan run {plan_run_id} was not found for auto run-plan step")
        })?
    } else {
        let plan_run = launch.create_run();
        save_plan_run(conn, &plan_run)?;
        persisted.steps[step_index].plan_run_id = Some(plan_run.run.id.clone());
        save_step_with_conn(conn, &mut persisted.steps[step_index])?;
        plan_run
    };

    let summary = format!("running plan phases from {}", plan_run.run.plan_display);
    append_system_output(
        conn,
        step_id,
        AutoOutputKind::Status,
        &summary,
        None,
        max_output_lines_per_step,
    )?;

    let mut plan_executor = PlanExecutorConfig::new(
        config.tool("opencode"),
        server_url,
        persisted.run.worktree_path.clone(),
        plan_run.run.plan_display.clone(),
    );
    plan_executor.max_output_lines_per_step = max_output_lines_per_step;
    if config.opencode_plan_plugin
        && let Ok(plugin) = prepare_plan_plugin_config(&repo.prism_dir())
    {
        plan_executor = plan_executor.with_plugin_config(plugin);
    }

    let mut output = Vec::new();
    let result = match mode {
        PlanRunMode::Sequential => {
            execute_plan_sequential(conn, &mut plan_run, &plan_executor, &mut output)
        }
        PlanRunMode::Parallel => {
            execute_plan_parallel(conn, &mut plan_run, &plan_executor, &mut output)
        }
    };
    if let Err(error) = result
        && !matches!(
            plan_run.run.status,
            PlanRunStatus::Failed | PlanRunStatus::Aborted
        )
    {
        return Err(error);
    }

    match plan_run.run.status {
        PlanRunStatus::Done => {
            let summary = format!("plan run {} completed", plan_run.run.id);
            append_system_output(
                conn,
                step_id,
                AutoOutputKind::Status,
                &summary,
                None,
                max_output_lines_per_step,
            )?;
            finish_non_agent_step(
                conn,
                &mut persisted.steps[step_index],
                AutoStepStatus::Done,
                Some(summary),
                None,
            )
        }
        PlanRunStatus::Paused => {
            let summary = format!(
                "plan run {} paused; resume linked plan run",
                plan_run.run.id
            );
            append_system_output(
                conn,
                step_id,
                AutoOutputKind::Status,
                &summary,
                None,
                max_output_lines_per_step,
            )?;
            finish_non_agent_step(
                conn,
                &mut persisted.steps[step_index],
                AutoStepStatus::Waiting,
                Some(summary),
                None,
            )
        }
        PlanRunStatus::Failed | PlanRunStatus::Aborted => {
            let error = format!(
                "plan run {} ended with status {}; inspect linked plan dashboard",
                plan_run.run.id,
                plan_run_status_label(plan_run.run.status)
            );
            finish_non_agent_step(
                conn,
                &mut persisted.steps[step_index],
                AutoStepStatus::Failed,
                Some("plan run failed".to_string()),
                Some(error.clone()),
            )?;
            Err(error)
        }
        PlanRunStatus::Draft | PlanRunStatus::Queued | PlanRunStatus::Running => {
            let summary = format!(
                "plan run {} is {}; Auto Flow is waiting",
                plan_run.run.id,
                plan_run_status_label(plan_run.run.status)
            );
            finish_non_agent_step(
                conn,
                &mut persisted.steps[step_index],
                AutoStepStatus::Waiting,
                Some(summary),
                None,
            )
        }
    }
}

pub(super) fn start_non_agent_step(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
) -> Result<(), String> {
    let step = &mut persisted.steps[step_index];
    step.status = AutoStepStatus::Running;
    step.started_unix_ms = Some(unix_ms());
    step.finished_unix_ms = None;
    step.error = None;
    persisted.run.selected_step_run_id = step.id;
    persisted.run.status = AutoRunStatus::Running;
    persisted.run.updated_unix_ms = unix_ms();
    save_step_with_conn(conn, step)?;
    save_run_with_conn(conn, &persisted.run)
}

pub(super) fn execute_local_verify_step(
    conn: &rusqlite::Connection,
    config: &Config,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    let result =
        crate::verify::run_auto_verify(config, &persisted.run.worktree_path, VerifyMode::Normal);
    let summary = format_verify_result(&result);
    let step_id = persisted.steps[step_index]
        .id
        .ok_or_else(|| "auto verify step must be saved before output".to_string())?;
    append_system_output(
        conn,
        step_id,
        if result.passed {
            AutoOutputKind::Status
        } else {
            AutoOutputKind::Error
        },
        &summary,
        None,
        max_output_lines_per_step,
    )?;
    if result.passed {
        finish_non_agent_step(
            conn,
            &mut persisted.steps[step_index],
            AutoStepStatus::Done,
            Some("local verification passed".to_string()),
            None,
        )?;
        return Ok(());
    }

    finish_non_agent_step(
        conn,
        &mut persisted.steps[step_index],
        AutoStepStatus::Failed,
        Some("local verification failed".to_string()),
        Some(summary.clone()),
    )?;
    if persisted.next_attempt_for(&AutoStepKey::FixLocalVerify) <= MAX_LOCAL_VERIFY_ATTEMPTS {
        append_step_run(conn, persisted, AutoStepKey::FixLocalVerify, Some(summary))?;
        Ok(())
    } else {
        Err(format!(
            "local verification failed after {MAX_LOCAL_VERIFY_ATTEMPTS} repair attempts"
        ))
    }
}

pub(super) fn execute_commit_impl_step(
    conn: &rusqlite::Connection,
    config: &Config,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    let message = implementation_commit_message(&persisted.run);
    let result = crate::git::commit_if_dirty(&persisted.run.worktree_path, config, &message)?;
    let step = &mut persisted.steps[step_index];
    step.commit_sha = result.commit_sha.clone();
    step.head_sha = result
        .commit_sha
        .clone()
        .or_else(|| crate::git::current_head_sha(&persisted.run.worktree_path, config).ok());
    persisted.run.current_head_sha = step.head_sha.clone();
    let status = if result.committed {
        AutoStepStatus::Done
    } else {
        AutoStepStatus::Skipped
    };
    let summary = if result.committed {
        format!(
            "committed implementation as {}",
            result.commit_sha.as_deref().unwrap_or("unknown")
        )
    } else {
        result.message
    };
    let step_id = step
        .id
        .ok_or_else(|| "auto commit step must be saved before output".to_string())?;
    append_system_output(
        conn,
        step_id,
        AutoOutputKind::Status,
        &summary,
        None,
        max_output_lines_per_step,
    )?;
    finish_non_agent_step(conn, step, status, Some(summary), None)?;
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)
}

pub(super) fn execute_push_pr_step(
    conn: &rusqlite::Connection,
    repo: &Repository,
    config: &Config,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    let head_sha = crate::git::current_head_sha(&persisted.run.worktree_path, config)?;
    crate::git::push_current_branch(&persisted.run.worktree_path, config)?;

    let mut cache = crate::github::load_pr_cache(repo, &persisted.run.branch);
    crate::lifecycle::refresh_branch_pr_cache(
        repo,
        config,
        &persisted.run.branch,
        &persisted.run.worktree_path,
        &mut cache,
        true,
    );
    if cache.summary.is_none() {
        let body = auto_pr_body(config, &persisted.run);
        crate::lifecycle::create_pull_request(
            repo,
            config,
            &persisted.run.branch,
            &persisted.run.worktree_path,
            &body,
            None,
            &mut cache,
        )?;
    }
    if cache.summary.is_none() {
        crate::lifecycle::refresh_branch_pr_cache(
            repo,
            config,
            &persisted.run.branch,
            &persisted.run.worktree_path,
            &mut cache,
            true,
        );
    }
    let summary = cache
        .summary
        .as_ref()
        .ok_or_else(|| "push/create PR completed but no PR summary was found".to_string())?;
    persisted.run.pr_number = Some(summary.number);
    persisted.run.pr_url = Some(summary.url.clone());
    persisted.run.current_head_sha = Some(if summary.head_sha.trim().is_empty() {
        head_sha.clone()
    } else {
        summary.head_sha.clone()
    });
    let step = &mut persisted.steps[step_index];
    step.head_sha = persisted.run.current_head_sha.clone();
    persisted.run.review_baseline_json = Some(review_baseline_json(summary));
    let message = format!("PR #{} {}", summary.number, summary.url);
    let step_id = step
        .id
        .ok_or_else(|| "auto push PR step must be saved before output".to_string())?;
    append_system_output(
        conn,
        step_id,
        AutoOutputKind::Status,
        &message,
        None,
        max_output_lines_per_step,
    )?;
    finish_non_agent_step(conn, step, AutoStepStatus::Done, Some(message), None)?;
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)
}

pub(super) fn execute_wait_review_step(
    conn: &rusqlite::Connection,
    repo: &Repository,
    config: &Config,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    let step_id = persisted.steps[step_index]
        .id
        .ok_or_else(|| "auto review wait step must be saved before output".to_string())?;
    if !config.auto.review_wait_enabled {
        append_system_output(
            conn,
            step_id,
            AutoOutputKind::Status,
            "review wait disabled; continuing",
            None,
            max_output_lines_per_step,
        )?;
        finish_non_agent_step(
            conn,
            &mut persisted.steps[step_index],
            AutoStepStatus::Skipped,
            Some("review wait disabled".to_string()),
            None,
        )?;
        return Ok(());
    }

    let deadline = unix_ms().saturating_add(config.auto.review_max_wait_seconds * 1000);
    loop {
        let outcome = poll_review_feedback(repo, config, persisted)?;
        append_auto_event(
            conn,
            &AutoEvent {
                id: None,
                run_id: persisted.run.id.clone(),
                step_run_id: Some(step_id),
                time_unix_ms: unix_ms(),
                kind: "review_wait_poll".to_string(),
                data_json: format!("{{\"summary\":{}}}", json_string(&outcome.summary)),
            },
        )?;
        append_system_output(
            conn,
            step_id,
            AutoOutputKind::Status,
            &outcome.summary,
            None,
            max_output_lines_per_step,
        )?;

        if let Some(prompt) = outcome.fix_prompt {
            finish_non_agent_step(
                conn,
                &mut persisted.steps[step_index],
                AutoStepStatus::Done,
                Some(outcome.summary),
                None,
            )?;
            if persisted.next_attempt_for(&AutoStepKey::FixReview) <= MAX_REVIEW_FIX_ATTEMPTS {
                append_step_run(conn, persisted, AutoStepKey::FixReview, Some(prompt))?;
                return Ok(());
            }
            return Err(format!(
                "review feedback remained after {MAX_REVIEW_FIX_ATTEMPTS} repair attempts"
            ));
        }

        if outcome.complete {
            finish_non_agent_step(
                conn,
                &mut persisted.steps[step_index],
                AutoStepStatus::Skipped,
                Some(outcome.summary),
                None,
            )?;
            return Ok(());
        }

        if unix_ms() >= deadline {
            let summary = format!(
                "review wait timed out after {} second(s)",
                config.auto.review_max_wait_seconds
            );
            let status = if config.auto.review_continue_on_timeout {
                AutoStepStatus::Skipped
            } else {
                AutoStepStatus::Failed
            };
            finish_non_agent_step(
                conn,
                &mut persisted.steps[step_index],
                status,
                Some(summary.clone()),
                if status == AutoStepStatus::Failed {
                    Some(summary.clone())
                } else {
                    None
                },
            )?;
            if status == AutoStepStatus::Failed {
                return Err(summary);
            }
            return Ok(());
        }

        persisted.steps[step_index].status = AutoStepStatus::Waiting;
        save_step_with_conn(conn, &mut persisted.steps[step_index])?;
        std::thread::sleep(std::time::Duration::from_secs(
            config.auto.review_poll_interval_seconds,
        ));
        if reload_pause_request(conn, persisted)? {
            return Ok(());
        }
        persisted.steps[step_index].status = AutoStepStatus::Running;
        save_step_with_conn(conn, &mut persisted.steps[step_index])?;
    }
}

pub(super) fn execute_verify_review_fix_step(
    conn: &rusqlite::Connection,
    config: &Config,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    let result =
        crate::verify::run_auto_verify(config, &persisted.run.worktree_path, VerifyMode::ReviewFix);
    let summary = format_verify_result(&result);
    let step_id = persisted.steps[step_index]
        .id
        .ok_or_else(|| "auto review verify step must be saved before output".to_string())?;
    append_system_output(
        conn,
        step_id,
        if result.passed {
            AutoOutputKind::Status
        } else {
            AutoOutputKind::Error
        },
        &summary,
        None,
        max_output_lines_per_step,
    )?;
    if result.passed {
        finish_non_agent_step(
            conn,
            &mut persisted.steps[step_index],
            AutoStepStatus::Done,
            Some("review-fix verification passed".to_string()),
            None,
        )?;
        return Ok(());
    }
    finish_non_agent_step(
        conn,
        &mut persisted.steps[step_index],
        AutoStepStatus::Failed,
        Some("review-fix verification failed".to_string()),
        Some(summary.clone()),
    )?;
    if persisted.next_attempt_for(&AutoStepKey::FixLocalVerify) <= MAX_LOCAL_VERIFY_ATTEMPTS {
        append_step_run(conn, persisted, AutoStepKey::FixLocalVerify, Some(summary))?;
        Ok(())
    } else {
        Err(format!(
            "review-fix verification failed after {MAX_LOCAL_VERIFY_ATTEMPTS} repair attempts"
        ))
    }
}

pub(super) fn execute_commit_review_fix_step(
    conn: &rusqlite::Connection,
    repo: &Repository,
    config: &Config,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    let mut cache = crate::github::load_pr_cache(repo, &persisted.run.branch);
    crate::lifecycle::refresh_branch_pr_cache(
        repo,
        config,
        &persisted.run.branch,
        &persisted.run.worktree_path,
        &mut cache,
        true,
    );
    let guard_facts = repair_guard_facts(config, persisted, cache.summary.as_ref());
    let message = repair_commit_message(config, "repair_commit_review", "fix: cr");
    let result = crate::git::commit_if_dirty(&persisted.run.worktree_path, config, &message)?;
    let head_sha = crate::git::current_head_sha(&persisted.run.worktree_path, config).ok();
    persisted.run.current_head_sha = head_sha.clone();
    if let Some(summary) = cache.summary.as_ref() {
        persisted.run.pr_number = Some(summary.number);
        persisted.run.pr_url = Some(summary.url.clone());
        persisted.run.review_baseline_json = Some(review_baseline_json(summary));
    }
    if result.committed && config.auto.push_repairs {
        crate::git::push_current_branch(&persisted.run.worktree_path, config)?;
        persisted.run.pending_push = None;
    } else if result.committed {
        persisted.run.pending_push = result.commit_sha.clone().map(|commit_sha| {
            pending_push_guard(
                stabilization_model::RepairKind::Review,
                commit_sha,
                head_sha.clone().unwrap_or_default(),
                guard_facts,
                persisted.steps[step_index]
                    .work_guard
                    .as_ref()
                    .map(|guard| guard.review_thread_ids.clone())
                    .unwrap_or_default(),
            )
        });
        persisted.run.stabilization_status =
            Some(stabilization_model::StabilizationStatus::Blocked);
        persisted.run.stabilization_blocker =
            Some(stabilization_model::StabilizationBlocker::PendingPush);
        persisted.run.stabilization_next_work =
            Some(stabilization_model::StabilizationWorkKind::PushPendingRepair);
    }

    let step = &mut persisted.steps[step_index];
    step.commit_sha = result.commit_sha.clone();
    step.head_sha = persisted.run.current_head_sha.clone();
    let status = if result.committed {
        AutoStepStatus::Done
    } else {
        AutoStepStatus::Skipped
    };
    let summary = if result.committed {
        repair_commit_summary(
            config,
            "review",
            result.commit_sha.as_deref().unwrap_or("unknown"),
        )
    } else {
        result.message
    };
    let step_id = step
        .id
        .ok_or_else(|| "auto review commit step must be saved before output".to_string())?;
    append_system_output(
        conn,
        step_id,
        AutoOutputKind::Status,
        &summary,
        None,
        max_output_lines_per_step,
    )?;
    finish_non_agent_step(conn, step, status, Some(summary), None)?;
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)
}

pub(super) fn execute_wait_ci_step(
    conn: &rusqlite::Connection,
    repo: &Repository,
    config: &Config,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    let step_id = persisted.steps[step_index]
        .id
        .ok_or_else(|| "auto CI wait step must be saved before output".to_string())?;
    if !config.auto.ci_wait_enabled {
        append_system_output(
            conn,
            step_id,
            AutoOutputKind::Status,
            "CI wait disabled; continuing",
            None,
            max_output_lines_per_step,
        )?;
        finish_non_agent_step(
            conn,
            &mut persisted.steps[step_index],
            AutoStepStatus::Skipped,
            Some("CI wait disabled".to_string()),
            None,
        )?;
        return Ok(());
    }

    let deadline = unix_ms().saturating_add(config.auto.ci_max_wait_seconds * 1000);
    loop {
        let outcome = poll_ci_status(repo, config, persisted)?;
        append_auto_event(
            conn,
            &AutoEvent {
                id: None,
                run_id: persisted.run.id.clone(),
                step_run_id: Some(step_id),
                time_unix_ms: unix_ms(),
                kind: "ci_wait_poll".to_string(),
                data_json: format!(
                    "{{\"state\":{},\"summary\":{}}}",
                    json_string(outcome.state.label()),
                    json_string(&outcome.summary)
                ),
            },
        )?;
        append_system_output(
            conn,
            step_id,
            AutoOutputKind::Status,
            &outcome.summary,
            None,
            max_output_lines_per_step,
        )?;

        match outcome.state {
            PrCheckState::Success => {
                finish_non_agent_step(
                    conn,
                    &mut persisted.steps[step_index],
                    AutoStepStatus::Done,
                    Some(outcome.summary),
                    None,
                )?;
                return Ok(());
            }
            PrCheckState::Failed | PrCheckState::Mixed => {
                finish_non_agent_step(
                    conn,
                    &mut persisted.steps[step_index],
                    AutoStepStatus::Done,
                    Some(outcome.summary.clone()),
                    None,
                )?;
                if persisted.next_attempt_for(&AutoStepKey::FixCi) <= MAX_CI_FIX_ATTEMPTS {
                    append_step_run(conn, persisted, AutoStepKey::FixCi, Some(outcome.prompt))?;
                    return Ok(());
                }
                let error =
                    format!("CI remained failing after {MAX_CI_FIX_ATTEMPTS} repair attempts");
                return Err(error);
            }
            PrCheckState::Pending | PrCheckState::Unknown => {}
        }

        if unix_ms() >= deadline {
            let summary = format!(
                "CI wait timed out after {} second(s)",
                config.auto.ci_max_wait_seconds
            );
            finish_non_agent_step(
                conn,
                &mut persisted.steps[step_index],
                AutoStepStatus::Failed,
                Some(summary.clone()),
                Some(summary.clone()),
            )?;
            return Err(summary);
        }

        persisted.steps[step_index].status = AutoStepStatus::Waiting;
        save_step_with_conn(conn, &mut persisted.steps[step_index])?;
        std::thread::sleep(std::time::Duration::from_secs(
            config.auto.ci_poll_interval_seconds,
        ));
        if reload_pause_request(conn, persisted)? {
            return Ok(());
        }
        persisted.steps[step_index].status = AutoStepStatus::Running;
        save_step_with_conn(conn, &mut persisted.steps[step_index])?;
    }
}

pub(super) fn execute_verify_ci_fix_step(
    conn: &rusqlite::Connection,
    config: &Config,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    let result =
        crate::verify::run_auto_verify(config, &persisted.run.worktree_path, VerifyMode::Normal);
    let summary = format_verify_result(&result);
    let step_id = persisted.steps[step_index]
        .id
        .ok_or_else(|| "auto CI verify step must be saved before output".to_string())?;
    append_system_output(
        conn,
        step_id,
        if result.passed {
            AutoOutputKind::Status
        } else {
            AutoOutputKind::Error
        },
        &summary,
        None,
        max_output_lines_per_step,
    )?;
    if result.passed {
        finish_non_agent_step(
            conn,
            &mut persisted.steps[step_index],
            AutoStepStatus::Done,
            Some("CI-fix verification passed".to_string()),
            None,
        )?;
        return Ok(());
    }
    finish_non_agent_step(
        conn,
        &mut persisted.steps[step_index],
        AutoStepStatus::Failed,
        Some("CI-fix verification failed".to_string()),
        Some(summary.clone()),
    )?;
    if persisted.next_attempt_for(&AutoStepKey::FixLocalVerify) <= MAX_LOCAL_VERIFY_ATTEMPTS {
        append_step_run(conn, persisted, AutoStepKey::FixLocalVerify, Some(summary))?;
        Ok(())
    } else {
        Err(format!(
            "CI-fix verification failed after {MAX_LOCAL_VERIFY_ATTEMPTS} repair attempts"
        ))
    }
}

pub(super) fn execute_commit_ci_fix_step(
    conn: &rusqlite::Connection,
    repo: &Repository,
    config: &Config,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    let mut cache = crate::github::load_pr_cache(repo, &persisted.run.branch);
    crate::lifecycle::refresh_branch_pr_cache(
        repo,
        config,
        &persisted.run.branch,
        &persisted.run.worktree_path,
        &mut cache,
        true,
    );
    let guard_facts = repair_guard_facts(config, persisted, cache.summary.as_ref());
    let message = repair_commit_message(config, "repair_commit_ci", "fix: ci");
    let result = crate::git::commit_if_dirty(&persisted.run.worktree_path, config, &message)?;
    if !result.committed {
        let summary = "CI fix produced no commitable changes".to_string();
        finish_non_agent_step(
            conn,
            &mut persisted.steps[step_index],
            AutoStepStatus::Failed,
            Some(summary.clone()),
            Some(summary.clone()),
        )?;
        return Err(summary);
    }
    let local_head = crate::git::current_head_sha(&persisted.run.worktree_path, config).ok();
    persisted.run.current_head_sha = local_head.clone();
    if let Some(summary) = cache.summary.as_ref() {
        persisted.run.pr_number = Some(summary.number);
        persisted.run.pr_url = Some(summary.url.clone());
        persisted.run.review_baseline_json = Some(review_baseline_json(summary));
    }
    if config.auto.push_repairs {
        crate::git::push_current_branch(&persisted.run.worktree_path, config)?;
        persisted.run.pending_push = None;
    } else {
        persisted.run.pending_push = result.commit_sha.clone().map(|commit_sha| {
            pending_push_guard(
                stabilization_model::RepairKind::Ci,
                commit_sha,
                local_head.clone().unwrap_or_default(),
                guard_facts,
                Vec::new(),
            )
        });
        persisted.run.stabilization_status =
            Some(stabilization_model::StabilizationStatus::Blocked);
        persisted.run.stabilization_blocker =
            Some(stabilization_model::StabilizationBlocker::PendingPush);
        persisted.run.stabilization_next_work =
            Some(stabilization_model::StabilizationWorkKind::PushPendingRepair);
    }

    let step = &mut persisted.steps[step_index];
    step.commit_sha = result.commit_sha.clone();
    step.head_sha = persisted.run.current_head_sha.clone();
    let summary = repair_commit_summary(
        config,
        "CI",
        result.commit_sha.as_deref().unwrap_or("unknown"),
    );
    let step_id = step
        .id
        .ok_or_else(|| "auto CI commit step must be saved before output".to_string())?;
    append_system_output(
        conn,
        step_id,
        AutoOutputKind::Status,
        &summary,
        None,
        max_output_lines_per_step,
    )?;
    finish_non_agent_step(conn, step, AutoStepStatus::Done, Some(summary), None)?;
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)
}

pub(super) fn execute_merge_step(
    conn: &rusqlite::Connection,
    repo: &Repository,
    config: &Config,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    let step_id = persisted.steps[step_index]
        .id
        .ok_or_else(|| "auto merge step must be saved before output".to_string())?;
    if !config.auto.merge {
        let summary = "auto.merge is false; PR is ready for manual merge".to_string();
        append_system_output(
            conn,
            step_id,
            AutoOutputKind::Status,
            &summary,
            None,
            max_output_lines_per_step,
        )?;
        finish_non_agent_step(
            conn,
            &mut persisted.steps[step_index],
            AutoStepStatus::Skipped,
            Some(summary),
            None,
        )?;
        return Ok(());
    }

    let verify =
        crate::verify::run_auto_verify(config, &persisted.run.worktree_path, VerifyMode::Normal);
    let local_head = crate::git::current_head_sha(&persisted.run.worktree_path, config)?;
    let dirty = crate::git::selected_dirty(&persisted.run.worktree_path, config)?;
    let mut cache = crate::github::load_pr_cache(repo, &persisted.run.branch);
    crate::lifecycle::refresh_branch_pr_cache(
        repo,
        config,
        &persisted.run.branch,
        &persisted.run.worktree_path,
        &mut cache,
        true,
    );
    let summary = cache
        .summary
        .as_ref()
        .ok_or_else(|| "merge gate could not find pull request summary".to_string())?;
    persisted.run.pr_number = Some(summary.number);
    persisted.run.pr_url = Some(summary.url.clone());
    persisted.run.current_head_sha = Some(summary.head_sha.clone());

    let gate = evaluate_merge_gate(
        config,
        persisted,
        summary,
        cache.details.as_ref(),
        &local_head,
        dirty,
        &verify,
    );
    append_system_output(
        conn,
        step_id,
        if gate.allowed {
            AutoOutputKind::Status
        } else {
            AutoOutputKind::Error
        },
        &gate.summary,
        None,
        max_output_lines_per_step,
    )?;
    if !gate.allowed {
        finish_non_agent_step(
            conn,
            &mut persisted.steps[step_index],
            AutoStepStatus::Failed,
            Some("merge blocked by final gate".to_string()),
            Some(gate.summary.clone()),
        )?;
        return Err(gate.summary);
    }

    if !summary.merged {
        crate::lifecycle::merge_pull_request(config, &persisted.run.worktree_path, summary.number)?;
    }
    let merged =
        crate::github::wait_for_pr_merged(&persisted.run.worktree_path, summary.number, config)?;
    if !merged {
        let error = format!(
            "PR #{} merge command completed, but GitHub has not marked it merged yet",
            summary.number
        );
        finish_non_agent_step(
            conn,
            &mut persisted.steps[step_index],
            AutoStepStatus::Failed,
            Some("merge verification incomplete".to_string()),
            Some(error.clone()),
        )?;
        return Err(error);
    }

    let done = format!("merged PR #{}", summary.number);
    append_system_output(
        conn,
        step_id,
        AutoOutputKind::Status,
        &done,
        None,
        max_output_lines_per_step,
    )?;
    finish_non_agent_step(
        conn,
        &mut persisted.steps[step_index],
        AutoStepStatus::Done,
        Some(done),
        None,
    )?;
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)
}

pub(super) fn execute_cleanup_step(
    conn: &rusqlite::Connection,
    repo: &Repository,
    config: &Config,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    let step_id = persisted.steps[step_index]
        .id
        .ok_or_else(|| "auto cleanup step must be saved before output".to_string())?;
    if !config.auto.cleanup_after_merge {
        let summary =
            "auto.cleanup_after_merge is false; leaving local worktree/session data".to_string();
        append_system_output(
            conn,
            step_id,
            AutoOutputKind::Status,
            &summary,
            None,
            max_output_lines_per_step,
        )?;
        finish_non_agent_step(
            conn,
            &mut persisted.steps[step_index],
            AutoStepStatus::Skipped,
            Some(summary),
            None,
        )?;
        return Ok(());
    }

    let warnings = cleanup_warnings(repo, config, &persisted.run.worktree_path);
    if !warnings.is_empty() {
        append_system_output(
            conn,
            step_id,
            AutoOutputKind::Status,
            &format!("cleanup warnings:\n- {}", warnings.join("\n- ")),
            None,
            max_output_lines_per_step,
        )?;
    }

    crate::lifecycle::delete_worktree_session(
        repo,
        config,
        &persisted.run.worktree_path,
        &persisted.run.branch,
    )?;
    let summary = "deleted local session data, worktree, and branch".to_string();
    append_system_output(
        conn,
        step_id,
        AutoOutputKind::Status,
        &summary,
        None,
        max_output_lines_per_step,
    )?;
    finish_non_agent_step(
        conn,
        &mut persisted.steps[step_index],
        AutoStepStatus::Done,
        Some(summary),
        None,
    )?;
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)
}

pub(super) fn finish_non_agent_step(
    conn: &rusqlite::Connection,
    step: &mut AutoStepRun,
    status: AutoStepStatus,
    summary: Option<String>,
    error: Option<String>,
) -> Result<(), String> {
    step.status = status;
    step.finished_unix_ms = Some(unix_ms());
    step.summary = summary;
    step.error = error;
    save_step_with_conn(conn, step)?;
    Ok(())
}

pub(super) fn set_auto_step_waiting(
    conn: &rusqlite::Connection,
    step: &mut AutoStepRun,
    summary: String,
) -> Result<(), String> {
    step.status = AutoStepStatus::Waiting;
    step.finished_unix_ms = None;
    step.process_id = None;
    step.summary = Some(summary);
    step.error = None;
    save_step_with_conn(conn, step).map(|_| ())
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub(super) struct ReviewBaseline {
    pub(super) head_sha: String,
    pub(super) updated_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ReviewPollOutcome {
    pub(super) summary: String,
    pub(super) fix_prompt: Option<String>,
    pub(super) complete: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CiPollOutcome {
    pub(super) state: PrCheckState,
    pub(super) summary: String,
    pub(super) prompt: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct MergeGateOutcome {
    pub(super) allowed: bool,
    pub(super) summary: String,
}

pub(super) fn evaluate_merge_gate(
    config: &Config,
    persisted: &PersistedAutoRun,
    summary: &PrSummary,
    details: Option<&PrDetails>,
    local_head: &str,
    dirty: bool,
    verify: &VerifyResult,
) -> MergeGateOutcome {
    let mut blockers = Vec::new();
    if dirty {
        blockers.push("worktree is dirty".to_string());
    }
    if !verify.passed {
        blockers.push("final local verification failed".to_string());
    }
    if summary.draft {
        blockers.push("PR is draft".to_string());
    }
    if summary.check_state() != PrCheckState::Success {
        blockers.push(format!("CI state is {}", summary.check_state().label()));
    }
    if summary.head_sha.trim().is_empty() {
        blockers.push("PR head SHA is unknown".to_string());
    } else if summary.head_sha.trim() != local_head.trim() {
        blockers.push(format!(
            "PR head {} does not match local head {}",
            empty_or_unknown(&summary.head_sha),
            empty_or_unknown(local_head)
        ));
    }

    let review_blocker = merge_gate_review_blocker(config, persisted, summary, details);
    if let Some(blocker) = review_blocker {
        blockers.push(blocker);
    }

    if blockers.is_empty() {
        MergeGateOutcome {
            allowed: true,
            summary: format!(
                "merge gate passed for PR #{} at head {}",
                summary.number,
                empty_or_unknown(local_head)
            ),
        }
    } else {
        MergeGateOutcome {
            allowed: false,
            summary: format!("merge blocked:\n- {}", blockers.join("\n- ")),
        }
    }
}

pub(super) fn merge_gate_review_blocker(
    config: &Config,
    persisted: &PersistedAutoRun,
    summary: &PrSummary,
    details: Option<&PrDetails>,
) -> Option<String> {
    if !config.auto.review_wait_enabled {
        return None;
    }
    let details = details?;
    let baseline = parse_review_baseline(persisted.run.review_baseline_json.as_deref());
    let after = baseline
        .as_ref()
        .filter(|baseline| baseline.head_sha == summary.head_sha)
        .map(|baseline| baseline.updated_at.as_str());
    let authors = config
        .auto
        .review_reviewer_identities
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let feedback = actionable_review_feedback(
        details,
        ReviewFeedbackFilter {
            after,
            authors: &authors,
        },
    );
    feedback.is_actionable().then(|| {
        format!(
            "actionable review feedback remains: {} inline, {} review body, {} PR comment(s)",
            feedback.inline_comments.len(),
            feedback.review_bodies.len(),
            feedback.pr_comments.len()
        )
    })
}

pub(super) fn poll_ci_status(
    repo: &Repository,
    config: &Config,
    persisted: &mut PersistedAutoRun,
) -> Result<CiPollOutcome, String> {
    let mut cache = crate::github::load_pr_cache(repo, &persisted.run.branch);
    crate::lifecycle::refresh_branch_pr_cache(
        repo,
        config,
        &persisted.run.branch,
        &persisted.run.worktree_path,
        &mut cache,
        true,
    );
    let summary = cache
        .summary
        .as_ref()
        .ok_or_else(|| "CI wait could not find pull request summary".to_string())?;
    persisted.run.pr_number = Some(summary.number);
    persisted.run.pr_url = Some(summary.url.clone());
    persisted.run.current_head_sha = Some(summary.head_sha.clone());
    evaluate_ci_status(
        config,
        &persisted.run.branch,
        summary,
        cache.details.as_ref(),
    )
}

pub(super) fn evaluate_ci_status(
    config: &Config,
    branch: &str,
    summary: &PrSummary,
    details: Option<&PrDetails>,
) -> Result<CiPollOutcome, String> {
    let state = summary.check_state();
    let details = details.cloned().unwrap_or_default();
    let failures = details.failing_checks.len().max(details.ci_failures.len());
    let prompt = crate::ci::build_ci_failure_prompt_from_input(
        crate::ci::CiFailurePromptInput {
            branch,
            summary,
            details: &details,
        },
        config,
    );
    let summary_text = match state {
        PrCheckState::Success => {
            format!("CI passed for head {}", empty_or_unknown(&summary.head_sha))
        }
        PrCheckState::Failed => {
            format!(
                "CI failed for head {} with {} failing check detail(s)",
                empty_or_unknown(&summary.head_sha),
                failures
            )
        }
        PrCheckState::Mixed => {
            format!(
                "CI is mixed for head {} with {} failing check detail(s)",
                empty_or_unknown(&summary.head_sha),
                failures
            )
        }
        PrCheckState::Pending => {
            format!(
                "CI is still running for head {}",
                empty_or_unknown(&summary.head_sha)
            )
        }
        PrCheckState::Unknown => {
            format!(
                "CI status is unknown for head {}; waiting for checks",
                empty_or_unknown(&summary.head_sha)
            )
        }
    };
    Ok(CiPollOutcome {
        state,
        summary: summary_text,
        prompt,
    })
}

pub(super) fn poll_review_feedback(
    repo: &Repository,
    config: &Config,
    persisted: &mut PersistedAutoRun,
) -> Result<ReviewPollOutcome, String> {
    let mut cache = crate::github::load_pr_cache(repo, &persisted.run.branch);
    crate::lifecycle::refresh_branch_pr_cache(
        repo,
        config,
        &persisted.run.branch,
        &persisted.run.worktree_path,
        &mut cache,
        true,
    );
    let summary = cache
        .summary
        .as_ref()
        .ok_or_else(|| "review wait could not find pull request summary".to_string())?;
    persisted.run.pr_number = Some(summary.number);
    persisted.run.pr_url = Some(summary.url.clone());
    persisted.run.current_head_sha = Some(summary.head_sha.clone());
    if persisted.run.review_baseline_json.is_none() {
        persisted.run.review_baseline_json = Some(review_baseline_json(summary));
    }
    evaluate_review_feedback(config, persisted, summary, cache.details.as_ref())
}

pub(super) fn evaluate_review_feedback(
    config: &Config,
    persisted: &mut PersistedAutoRun,
    summary: &crate::github::PrSummary,
    details: Option<&crate::github::PrDetails>,
) -> Result<ReviewPollOutcome, String> {
    let baseline = parse_review_baseline(persisted.run.review_baseline_json.as_deref());
    let after = baseline
        .as_ref()
        .filter(|baseline| baseline.head_sha == summary.head_sha)
        .map(|baseline| baseline.updated_at.as_str());
    if !has_configured_reviewer_requested(summary, config) {
        return Ok(ReviewPollOutcome {
            summary:
                "no automated reviewer feedback or pending configured reviewer found; continuing"
                    .to_string(),
            fix_prompt: None,
            complete: true,
        });
    }
    let Some(details) = details else {
        return Ok(ReviewPollOutcome {
            summary: "PR details are not available yet; waiting for review feedback".to_string(),
            fix_prompt: None,
            complete: false,
        });
    };
    let authors = config
        .auto
        .review_reviewer_identities
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let feedback = actionable_review_feedback(
        details,
        ReviewFeedbackFilter {
            after,
            authors: &authors,
        },
    );
    if feedback.is_actionable() {
        let prompt =
            render_auto_review_fix_prompt(summary.number, &persisted.run.branch, &feedback);
        return Ok(ReviewPollOutcome {
            summary: format_review_feedback_summary(&feedback),
            fix_prompt: Some(prompt),
            complete: false,
        });
    }
    let total_feedback =
        details.comments.len() + details.reviews.len() + details.review_comments.len();
    if total_feedback > 0 {
        return Ok(ReviewPollOutcome {
            summary: format!(
                "no actionable review feedback; skipped {} resolved, old, empty, or filtered item(s)",
                feedback.skipped_resolved_inline
                    + feedback.skipped_old
                    + feedback.skipped_empty
                    + feedback.skipped_author
            ),
            fix_prompt: None,
            complete: true,
        });
    }
    if summary.review_decision == "APPROVED" {
        return Ok(ReviewPollOutcome {
            summary: "review decision is approved; continuing".to_string(),
            fix_prompt: None,
            complete: true,
        });
    }
    Ok(ReviewPollOutcome {
        summary: "no review feedback found yet".to_string(),
        fix_prompt: None,
        complete: false,
    })
}

pub(super) fn has_configured_reviewer_requested(
    summary: &crate::github::PrSummary,
    config: &Config,
) -> bool {
    if config.auto.review_reviewer_identities.is_empty() {
        return !summary.requested_reviewers.is_empty();
    }
    summary.requested_reviewers.iter().any(|reviewer| {
        config
            .auto
            .review_reviewer_identities
            .iter()
            .any(|configured| reviewer.eq_ignore_ascii_case(configured))
    })
}

pub(super) fn review_baseline_json(summary: &crate::github::PrSummary) -> String {
    serde_json::to_string(&ReviewBaseline {
        head_sha: summary.head_sha.clone(),
        updated_at: summary.updated_at.clone(),
    })
    .unwrap_or_else(|_| "{}".to_string())
}

pub(super) fn parse_review_baseline(value: Option<&str>) -> Option<ReviewBaseline> {
    value.and_then(|value| serde_json::from_str(value).ok())
}

pub(super) fn render_auto_review_fix_prompt(
    pr_number: u64,
    branch: &str,
    feedback: &ReviewFeedback<'_>,
) -> String {
    let mut prompt = format!(
        "Resolve the actionable review feedback for PR #{pr_number} on branch {branch}. Stop without committing.\n\n"
    );
    if !feedback.inline_comments.is_empty() {
        prompt.push_str("Inline review comments:\n\n");
        for comment in &feedback.inline_comments {
            let line = if comment.line.trim().is_empty() {
                String::new()
            } else {
                format!(" line {}", comment.line)
            };
            prompt.push_str(&format!(
                "- {}{} by {}\n\n{}\n\n",
                crate::util::empty_dash(&comment.path),
                line,
                crate::util::empty_dash(&comment.author),
                comment.body.trim()
            ));
        }
    }
    if !feedback.review_bodies.is_empty() {
        prompt.push_str("Review bodies:\n\n");
        for review in &feedback.review_bodies {
            let state = if review.state.trim().is_empty() {
                String::new()
            } else {
                format!(" ({})", review.state.trim())
            };
            prompt.push_str(&format!(
                "- Review from {}{}\n\n{}\n\n",
                crate::util::empty_dash(&review.author),
                state,
                review.body.trim()
            ));
        }
    }
    if !feedback.pr_comments.is_empty() {
        prompt.push_str("PR comments:\n\n");
        for comment in &feedback.pr_comments {
            prompt.push_str(&format!(
                "- Comment from {}\n\n{}\n\n",
                crate::util::empty_dash(&comment.author),
                comment.body.trim()
            ));
        }
    }
    prompt
}

pub(super) fn format_review_feedback_summary(feedback: &ReviewFeedback<'_>) -> String {
    format!(
        "found actionable review feedback: {} inline, {} review body, {} PR comment(s)",
        feedback.inline_comments.len(),
        feedback.review_bodies.len(),
        feedback.pr_comments.len()
    )
}

pub(super) fn cleanup_warnings(
    repo: &Repository,
    config: &Config,
    worktree_path: &Path,
) -> Vec<String> {
    crate::session::discover_sessions(repo, config)
        .ok()
        .and_then(|sessions| {
            sessions
                .into_iter()
                .find(|session| session.path == worktree_path)
                .map(|session| session.deletion_warnings())
        })
        .unwrap_or_default()
}

pub(super) fn empty_or_unknown(value: &str) -> &str {
    if value.trim().is_empty() {
        "unknown"
    } else {
        value.trim()
    }
}

pub(super) fn format_verify_result(result: &VerifyResult) -> String {
    let mut lines = Vec::new();
    lines.push(if result.passed {
        "local verification passed".to_string()
    } else {
        "local verification failed".to_string()
    });
    for check in &result.checks {
        let state = if check.passed { "passed" } else { "failed" };
        lines.push(format!("- {}: {state}: {}", check.label, check.message));
    }
    lines.join("\n")
}

pub(super) fn implementation_commit_message(run: &AutoRun) -> String {
    let summary = run.prompt_summary.trim();
    if summary.is_empty() {
        "implement auto flow task".to_string()
    } else {
        format!("implement {summary}")
    }
}

#[derive(Clone)]
struct RepairGuardFacts {
    remote_head_sha: Option<String>,
    pr_number: Option<u64>,
    pr_head_sha: Option<String>,
    base_sha: Option<String>,
}

fn repair_guard_facts(
    config: &Config,
    persisted: &PersistedAutoRun,
    summary: Option<&PrSummary>,
) -> RepairGuardFacts {
    let remote_head_sha = crate::git::remote_branch_head_sha(
        &persisted.run.worktree_path,
        &persisted.run.branch,
        config,
    )
    .ok()
    .flatten();
    let base_sha = summary.and_then(|summary| {
        crate::git::remote_branch_head_sha(&persisted.run.worktree_path, &summary.base_ref, config)
            .ok()
            .flatten()
    });
    RepairGuardFacts {
        remote_head_sha,
        pr_number: summary
            .map(|summary| summary.number)
            .or(persisted.run.pr_number),
        pr_head_sha: summary
            .map(|summary| summary.head_sha.clone())
            .filter(|sha| !sha.trim().is_empty())
            .or_else(|| persisted.run.current_head_sha.clone()),
        base_sha,
    }
}

fn pending_push_guard(
    repair_kind: stabilization_model::RepairKind,
    commit_sha: String,
    expected_local_head_sha: String,
    facts: RepairGuardFacts,
    guarded_review_thread_ids: Vec<String>,
) -> stabilization_model::PendingPushGuard {
    stabilization_model::PendingPushGuard {
        repair_kind,
        commit_sha,
        expected_local_head_sha,
        expected_remote_head_sha: facts.remote_head_sha,
        pr_number: facts.pr_number,
        expected_pr_head_sha: facts.pr_head_sha,
        expected_base_sha: facts.base_sha,
        guarded_review_thread_ids,
    }
}

fn repair_commit_message(config: &Config, template_name: &str, default: &str) -> String {
    config
        .prompt_template(template_name)
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .unwrap_or(default)
        .to_string()
}

fn repair_commit_summary(config: &Config, label: &str, commit_sha: &str) -> String {
    if config.auto.push_repairs {
        format!("committed {label} fixes as {commit_sha} and pushed")
    } else {
        format!("committed {label} fixes as {commit_sha}; pending guarded push")
    }
}

pub(super) fn auto_pr_body(config: &Config, run: &AutoRun) -> String {
    let template = config
        .prompt_templates
        .get("pr_body")
        .map(String::as_str)
        .unwrap_or("Automated Prism run for: {prompt_summary}\n\nAuto run: {auto_run_id}");
    template
        .replace("{prompt_summary}", &run.prompt_summary)
        .replace("{auto_run_id}", &run.id)
        .replace("{branch}", &run.branch)
        .replace("{head_sha}", run.current_head_sha.as_deref().unwrap_or(""))
}

pub(super) fn plan_first_plan_path(run: &AutoRun) -> PathBuf {
    run.plan_path
        .clone()
        .unwrap_or_else(|| run.worktree_path.join("plan.md"))
}

pub(super) fn auto_plan_path(run: &AutoRun) -> Result<PathBuf, String> {
    match run.implementation_source {
        AutoImplementationSource::Prompt => {
            Err("prompt auto flow does not have a plan path".to_string())
        }
        AutoImplementationSource::ExistingPlan => run
            .plan_path
            .clone()
            .ok_or_else(|| "existing-plan auto flow requires a plan path".to_string()),
        AutoImplementationSource::DraftPlan => Ok(plan_first_plan_path(run)),
    }
}

pub(super) fn plan_run_status_label(status: PlanRunStatus) -> &'static str {
    match status {
        PlanRunStatus::Draft => "draft",
        PlanRunStatus::Queued => "queued",
        PlanRunStatus::Running => "running",
        PlanRunStatus::Paused => "paused",
        PlanRunStatus::Done => "done",
        PlanRunStatus::Failed => "failed",
        PlanRunStatus::Aborted => "aborted",
    }
}

pub(super) fn plan_run_mode_label(mode: PlanRunMode) -> &'static str {
    match mode {
        PlanRunMode::Sequential => "sequential",
        PlanRunMode::Parallel => "parallel",
    }
}

pub(super) fn parse_plan_run_mode(value: &str) -> Result<PlanRunMode, String> {
    match value {
        "sequential" => Ok(PlanRunMode::Sequential),
        "parallel" => Ok(PlanRunMode::Parallel),
        _ => Err(format!("unknown plan run mode: {value}")),
    }
}
