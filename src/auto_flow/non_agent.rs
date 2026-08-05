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
    crate::execution::validate_installed_claim(conn)?;
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
        AutoStepKey::UpdateBranch => execute_update_branch_step(
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
    let launch = execution
        .launch(Path::new(&persisted.run.repo_root), mode)?
        .with_harness(
            persisted.run.harness_id.clone(),
            persisted.run.adapter_id.clone(),
        );
    let mut plan_run = if let Some(plan_run_id) = persisted.steps[step_index].plan_run_id.as_deref()
    {
        load_plan_run(conn, plan_run_id)?.ok_or_else(|| {
            format!("linked plan run {plan_run_id} was not found for auto run-plan step")
        })?
    } else {
        let plan_run = launch.create_run();
        persisted.steps[step_index].plan_run_id = Some(plan_run.run.id.clone());
        save_step_with_conn(conn, &mut persisted.steps[step_index])?;
        save_plan_run(conn, &plan_run)?;
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

    let harness_config = config
        .harness_config(&persisted.run.harness_id)
        .map_err(|_| {
            format!(
                "auto run harness '{}' is no longer configured",
                persisted.run.harness_id
            )
        })?;
    let mut plan_executor = PlanExecutorConfig::for_harness(
        persisted.run.harness_id.clone(),
        harness_config.clone(),
        server_url,
        persisted.run.worktree_path.clone(),
        plan_run.run.plan_display.clone(),
    );
    plan_executor.max_output_lines_per_step = max_output_lines_per_step;
    if harness_config.adapter == "opencode"
        && config.opencode_plan_plugin
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
    crate::execution::validate_installed_claim(conn)?;
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
    crate::execution::validate_installed_claim(conn)?;
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
    persisted.run.status = persisted.authoritative_status();
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
    if !config.auto.push_initial {
        let step = &mut persisted.steps[step_index];
        let step_id = step
            .id
            .ok_or_else(|| "auto push PR step must be saved before output".to_string())?;
        let message = "initial push/create PR disabled by auto.push_initial".to_string();
        append_system_output(
            conn,
            step_id,
            AutoOutputKind::Status,
            &message,
            None,
            max_output_lines_per_step,
        )?;
        finish_non_agent_step(conn, step, AutoStepStatus::Skipped, Some(message), None)?;
        persisted.run.updated_unix_ms = unix_ms();
        return save_run_with_conn(conn, &persisted.run);
    }

    let mut cache = crate::remote::load_pr_cache(repo, &persisted.run.branch);
    let _ = crate::remote::dispatcher::refresh_change_request_cache(
        repo,
        &persisted.run.branch,
        &mut cache,
        &persisted.run.worktree_path,
        config,
        true,
    );
    let create_target = if cache.trusted_summary()?.is_none() {
        let (origin, _) = crate::remote::dispatcher::create_change_request_targets(
            &persisted.run.worktree_path,
            config,
        )?;
        Some(origin)
    } else {
        None
    };
    let expected_push = crate::remote::dispatcher::prepare_push(
        &persisted.run.worktree_path,
        config,
        &persisted.run.branch,
    )?;
    run_initial_push_checks(
        config,
        &persisted.run.worktree_path,
        create_target.is_some(),
    )?;
    let current_push = crate::remote::dispatcher::prepare_push(
        &persisted.run.worktree_path,
        config,
        &persisted.run.branch,
    )?;
    if current_push != expected_push {
        return Err(
            "Auto Flow push remote, branch, or HEAD changed during configured checks".to_string(),
        );
    }
    let head_sha = crate::git::current_head_sha(&persisted.run.worktree_path, config)?;
    crate::execution::validate_installed_claim(conn)?;
    crate::lifecycle::push_branch(
        config,
        &persisted.run.worktree_path,
        &persisted.run.branch,
        current_push.set_upstream,
    )?;
    let pushed_source = crate::remote::dispatcher::prepare_push(
        &persisted.run.worktree_path,
        config,
        &persisted.run.branch,
    )?;
    if !crate::remote::dispatcher::same_push_target(&current_push, &pushed_source) {
        return Err("Auto Flow push destination changed while pushing".to_string());
    }

    crate::remote::dispatcher::refresh_change_request_cache(
        repo,
        &persisted.run.branch,
        &mut cache,
        &persisted.run.worktree_path,
        config,
        true,
    )?;
    if cache.trusted_summary()?.is_none() {
        let target = create_target.ok_or_else(|| {
            "existing change request disappeared after the initial push".to_string()
        })?;
        run_create_checks_after_push(
            config,
            &persisted.run.worktree_path,
            &persisted.run.branch,
            &head_sha,
        )?;
        let guard = crate::remote::dispatcher::prepare_create_change_request(
            &persisted.run.worktree_path,
            config,
            &persisted.run.branch,
            &target,
            &pushed_source,
        )?;
        let body = auto_pr_body(config, &persisted.run);
        crate::execution::validate_installed_claim(conn)?;
        crate::remote::dispatcher::create_change_request(
            repo,
            config,
            &persisted.run.worktree_path,
            &body,
            &guard,
            &mut cache,
        )?;
    }
    if cache.trusted_summary()?.is_none() {
        crate::remote::dispatcher::refresh_change_request_cache(
            repo,
            &persisted.run.branch,
            &mut cache,
            &persisted.run.worktree_path,
            config,
            true,
        )?;
    }
    let summary = cache
        .trusted_summary()?
        .ok_or_else(|| "push/create PR completed but no PR summary was found".to_string())?;
    save_observed_change_request_identity(
        conn,
        &persisted.run.id,
        summary.change_request_identity.as_ref(),
    )?;
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

pub(super) fn run_initial_push_checks(
    config: &Config,
    path: &std::path::Path,
    creating_change_request: bool,
) -> Result<(), String> {
    if creating_change_request {
        crate::lifecycle::run_pre_pr_checks(config, path)?;
    }
    crate::lifecycle::run_pre_push_checks(config, path)
}

fn run_create_checks_after_push(
    config: &Config,
    path: &std::path::Path,
    branch: &str,
    pushed_head_sha: &str,
) -> Result<(), String> {
    crate::lifecycle::run_pre_pr_checks(config, path)?;
    let current_branch = crate::git::current_branch_name(path, config)?
        .ok_or_else(|| "cannot create a change request from detached HEAD".to_string())?;
    let current_head = crate::git::current_head_sha(path, config)?;
    if current_branch != branch || current_head != pushed_head_sha {
        return Err("branch or HEAD changed during pre-PR checks after push".to_string());
    }
    Ok(())
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
        let outcome = poll_review_feedback(conn, repo, config, persisted)?;
        let work = stabilization_execute::observe_plan_and_save(conn, repo, config, persisted)?;
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

        if stabilization_execute::advance_review_wait(
            conn,
            persisted,
            step_index,
            work,
            outcome.summary,
            outcome.fix_prompt,
        )? != stabilization_execute::WaitProgress::KeepWaiting
        {
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
        interruptible_execution_sleep(conn, config.auto.review_poll_interval_seconds)?;
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
    crate::execution::validate_installed_claim(conn)?;
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
    let mut cache = crate::remote::load_pr_cache(repo, &persisted.run.branch);
    if let Some(guard) = persisted.steps[step_index].work_guard.as_ref() {
        cache.authorize_guarded_refresh(
            guard.change_request_identity.as_ref(),
            guard.pr_head_sha.as_deref(),
        );
    }
    crate::git::fetch_origin(&persisted.run.worktree_path, config)?;
    crate::remote::dispatcher::refresh_change_request_cache(
        repo,
        &persisted.run.branch,
        &mut cache,
        &persisted.run.worktree_path,
        config,
        true,
    )?;
    let current_guard = current_work_guard(config, persisted, &cache)?;
    let pr_summary = cache.trusted_summary()?.cloned();
    save_observed_change_request_identity(
        conn,
        &persisted.run.id,
        pr_summary
            .as_ref()
            .and_then(|summary| summary.change_request_identity.as_ref()),
    )?;
    let pr_number = pr_summary.as_ref().map(|summary| summary.number);
    if let stabilization_execute::RepairCommitGate::Invalidated { summary } =
        stabilization_execute::validate_and_begin_repair_commit(
            conn,
            repo,
            config,
            persisted,
            step_index,
            stabilization_model::RepairKind::Review,
            stabilization_execute::RepairCommitObservation {
                guard: current_guard,
                pr_number,
            },
        )?
    {
        let step_id = persisted.steps[step_index]
            .id
            .ok_or_else(|| "repair commit step must be saved before output".to_string())?;
        append_system_output(
            conn,
            step_id,
            AutoOutputKind::Status,
            &summary,
            None,
            max_output_lines_per_step,
        )?;
        return Ok(());
    }
    let message = stabilization_execute::repair_commit_message(
        config,
        &stabilization_model::RepairKind::Review,
    );
    crate::execution::validate_installed_claim(conn)?;
    let result = stabilization_execute::commit_repair_changes(
        &persisted.run.worktree_path,
        config,
        persisted.steps[step_index]
            .work_guard
            .as_ref()
            .and_then(|guard| guard.local_head_sha.as_deref()),
        &message,
    )?;
    let local_head = crate::git::current_head_sha(&persisted.run.worktree_path, config).ok();
    let outcome = stabilization_execute::complete_repair_commit(
        conn,
        repo,
        config,
        persisted,
        step_index,
        stabilization_model::RepairKind::Review,
        result,
        local_head,
        pr_summary,
        &mut cache,
    )?;
    let step_id = persisted.steps[step_index]
        .id
        .ok_or_else(|| "auto review commit step must be saved before output".to_string())?;
    append_system_output(
        conn,
        step_id,
        AutoOutputKind::Status,
        &outcome.summary,
        None,
        max_output_lines_per_step,
    )?;
    Ok(())
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
        let outcome = poll_ci_status(conn, repo, config, persisted)?;
        let work = stabilization_execute::observe_plan_and_save(conn, repo, config, persisted)?;
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

        if stabilization_execute::advance_ci_wait(
            conn,
            persisted,
            step_index,
            work,
            outcome.summary,
            outcome.prompt,
        )? != stabilization_execute::WaitProgress::KeepWaiting
        {
            return Ok(());
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
        interruptible_execution_sleep(conn, config.auto.ci_poll_interval_seconds)?;
        if reload_pause_request(conn, persisted)? {
            return Ok(());
        }
        persisted.steps[step_index].status = AutoStepStatus::Running;
        save_step_with_conn(conn, &mut persisted.steps[step_index])?;
    }
}

fn interruptible_execution_sleep(conn: &rusqlite::Connection, seconds: u64) -> Result<(), String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    while std::time::Instant::now() < deadline {
        crate::execution::validate_installed_claim(conn)?;
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        std::thread::sleep(remaining.min(std::time::Duration::from_millis(250)));
    }
    Ok(())
}

pub(super) fn execute_verify_ci_fix_step(
    conn: &rusqlite::Connection,
    config: &Config,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    crate::execution::validate_installed_claim(conn)?;
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
    let mut cache = crate::remote::load_pr_cache(repo, &persisted.run.branch);
    if let Some(guard) = persisted.steps[step_index].work_guard.as_ref() {
        cache.authorize_guarded_refresh(
            guard.change_request_identity.as_ref(),
            guard.pr_head_sha.as_deref(),
        );
    }
    crate::git::fetch_origin(&persisted.run.worktree_path, config)?;
    crate::remote::dispatcher::refresh_change_request_cache(
        repo,
        &persisted.run.branch,
        &mut cache,
        &persisted.run.worktree_path,
        config,
        true,
    )?;
    let current_guard = current_work_guard(config, persisted, &cache)?;
    let pr_summary = cache.trusted_summary()?.cloned();
    save_observed_change_request_identity(
        conn,
        &persisted.run.id,
        pr_summary
            .as_ref()
            .and_then(|summary| summary.change_request_identity.as_ref()),
    )?;
    let pr_number = pr_summary.as_ref().map(|summary| summary.number);
    if let stabilization_execute::RepairCommitGate::Invalidated { summary } =
        stabilization_execute::validate_and_begin_repair_commit(
            conn,
            repo,
            config,
            persisted,
            step_index,
            stabilization_model::RepairKind::Ci,
            stabilization_execute::RepairCommitObservation {
                guard: current_guard,
                pr_number,
            },
        )?
    {
        let step_id = persisted.steps[step_index]
            .id
            .ok_or_else(|| "repair commit step must be saved before output".to_string())?;
        append_system_output(
            conn,
            step_id,
            AutoOutputKind::Status,
            &summary,
            None,
            max_output_lines_per_step,
        )?;
        return Ok(());
    }
    let message =
        stabilization_execute::repair_commit_message(config, &stabilization_model::RepairKind::Ci);
    crate::execution::validate_installed_claim(conn)?;
    let result = stabilization_execute::commit_repair_changes(
        &persisted.run.worktree_path,
        config,
        persisted.steps[step_index]
            .work_guard
            .as_ref()
            .and_then(|guard| guard.local_head_sha.as_deref()),
        &message,
    )?;
    let local_head = crate::git::current_head_sha(&persisted.run.worktree_path, config).ok();
    let outcome = stabilization_execute::complete_repair_commit(
        conn,
        repo,
        config,
        persisted,
        step_index,
        stabilization_model::RepairKind::Ci,
        result,
        local_head,
        pr_summary,
        &mut cache,
    )?;
    let step_id = persisted.steps[step_index]
        .id
        .ok_or_else(|| "auto CI commit step must be saved before output".to_string())?;
    append_system_output(
        conn,
        step_id,
        AutoOutputKind::Status,
        &outcome.summary,
        None,
        max_output_lines_per_step,
    )?;
    Ok(())
}

pub(super) fn execute_update_branch_step(
    conn: &rusqlite::Connection,
    repo: &Repository,
    config: &Config,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    if !crate::integration::merge_intent_enabled(conn, &persisted.run.id, config.auto.merge)? {
        let summary = "merge intent was withdrawn before the reserved base update".to_string();
        finish_non_agent_step(
            conn,
            &mut persisted.steps[step_index],
            AutoStepStatus::Skipped,
            Some(summary),
            None,
        )?;
        return Ok(());
    }
    let intent = crate::integration::active_merge_intent(conn, &persisted.run.id)?
        .ok_or_else(|| "reserved base update has no armed merge intent".to_string())?;
    if !matches!(
        intent.placement,
        crate::integration::IntegrationPlacement::Reserved
            | crate::integration::IntegrationPlacement::Updating
    ) {
        return Err("pull request no longer owns the base-update reservation".to_string());
    }
    let recovering_update = intent.placement == crate::integration::IntegrationPlacement::Updating;
    let expected_guard = persisted.steps[step_index]
        .work_guard
        .clone()
        .ok_or_else(|| "base update step is missing its integration guard".to_string())?;
    let expected_local_head = expected_guard
        .local_head_sha
        .as_deref()
        .ok_or_else(|| "base update guard has no local HEAD".to_string())?;
    let expected_base = expected_guard
        .base_sha
        .as_deref()
        .ok_or_else(|| "base update guard has no target branch HEAD".to_string())?;

    crate::execution::validate_installed_claim(conn)?;
    let mut cache = crate::remote::load_pr_cache(repo, &persisted.run.branch);
    crate::remote::dispatcher::refresh_change_request_cache(
        repo,
        &persisted.run.branch,
        &mut cache,
        &persisted.run.worktree_path,
        config,
        true,
    )?;
    let summary = cache
        .trusted_summary()?
        .cloned()
        .ok_or_else(|| "pull request disappeared before its reserved base update".to_string())?;
    if recovering_update {
        let local_head = crate::git::current_head_sha(&persisted.run.worktree_path, config)?;
        if summary.head_sha != expected_local_head {
            if local_head != summary.head_sha {
                return Err(
                    "interrupted base update cannot be reconciled: local and pull request heads disagree"
                        .to_string(),
                );
            }
            return finish_updated_branch_step(
                conn,
                persisted,
                step_index,
                &summary,
                local_head,
                max_output_lines_per_step,
            );
        }
        if local_head != expected_local_head {
            return Err(
                "interrupted base update changed local HEAD without an authoritative pull request update"
                    .to_string(),
            );
        }
    }
    let target_repository = summary
        .change_request_identity
        .as_ref()
        .ok_or_else(|| "pull request has no canonical identity".to_string())?
        .target_repository()
        .map_err(|error| error.to_string())?;
    crate::remote::dispatcher::refresh_repository_policy_for(
        repo,
        &persisted.run.worktree_path,
        config,
        Some(&target_repository),
    )?;
    let mut effective_config = config.clone();
    effective_config.auto.merge = true;
    let snapshot = stabilization_observe::build_auto_run_stabilization_snapshot(
        repo,
        &persisted.run,
        &effective_config,
    );
    let current_work = stabilization_plan::plan(&snapshot);
    if current_work.kind != stabilization_model::StabilizationWorkKind::UpdateBranch {
        return Err(format!(
            "reserved base update was invalidated: {}",
            current_work.reason
        ));
    }
    if let stabilization_execute::WorkGuardDecision::Invalidated { reason } =
        stabilization_execute::decide_work_guard(
            &stabilization_model::RepairKind::Merge,
            &expected_guard,
            &current_work.guard,
        )
    {
        return Err(format!("reserved base update was invalidated: {reason}"));
    }

    let target_remote = stabilization_observe::target_remote_name(
        &persisted.run.worktree_path,
        config,
        Some(&target_repository),
    )?;
    let target_branch = expected_guard
        .authorized_target_branch
        .as_deref()
        .ok_or_else(|| "base update guard has no target branch".to_string())?;
    if !recovering_update {
        crate::integration::mark_updating(conn, &persisted.run.id)?;
    }
    let merged_head = crate::git::merge_remote_branch_guarded(
        &persisted.run.worktree_path,
        &target_remote,
        target_branch,
        expected_local_head,
        expected_base,
        config,
    )?;
    let expected_push = crate::remote::dispatcher::prepare_push(
        &persisted.run.worktree_path,
        config,
        &persisted.run.branch,
    )?;
    validate_base_update_push_guard(&expected_push, &expected_push, &merged_head, false)?;
    crate::lifecycle::run_pre_push_checks(config, &persisted.run.worktree_path)?;
    let current_push = crate::remote::dispatcher::prepare_push(
        &persisted.run.worktree_path,
        config,
        &persisted.run.branch,
    )?;
    validate_base_update_push_guard(
        &expected_push,
        &current_push,
        &merged_head,
        crate::git::selected_dirty(&persisted.run.worktree_path, config)?,
    )?;
    crate::execution::validate_installed_claim(conn)?;
    crate::lifecycle::push_branch(
        config,
        &persisted.run.worktree_path,
        &persisted.run.branch,
        current_push.set_upstream,
    )?;
    crate::remote::dispatcher::refresh_change_request_cache(
        repo,
        &persisted.run.branch,
        &mut cache,
        &persisted.run.worktree_path,
        config,
        true,
    )?;
    let refreshed = cache
        .trusted_summary()?
        .ok_or_else(|| "pull request disappeared after its reserved base update".to_string())?;
    if refreshed.head_sha != merged_head {
        return Err("updated pull request head is not yet authoritatively visible".to_string());
    }
    finish_updated_branch_step(
        conn,
        persisted,
        step_index,
        refreshed,
        merged_head,
        max_output_lines_per_step,
    )
}

pub(super) fn validate_base_update_push_guard(
    expected: &crate::remote::dispatcher::PushGuard,
    current: &crate::remote::dispatcher::PushGuard,
    merged_head: &str,
    dirty: bool,
) -> Result<(), String> {
    if expected.expected_head_sha != merged_head {
        return Err("reserved base update produced an unexpected local HEAD".to_string());
    }
    if current != expected {
        return Err("reserved base-update push guard changed during pre-push checks".to_string());
    }
    if dirty {
        return Err(
            "worktree became dirty during reserved base-update pre-push checks".to_string(),
        );
    }
    Ok(())
}

fn finish_updated_branch_step(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    summary: &crate::remote::PrSummary,
    updated_head: String,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    let identity = summary
        .change_request_identity
        .clone()
        .ok_or_else(|| "updated pull request has no canonical identity".to_string())?;
    crate::integration::synchronize_managed_generation(
        conn,
        &persisted.run.id,
        &crate::integration::CandidateGeneration {
            change_request_identity: identity.clone(),
            target_branch: summary.base_ref.clone(),
            pr_number: summary.number,
            head_sha: summary.head_sha.clone(),
        },
    )?;
    save_observed_change_request_identity(conn, &persisted.run.id, Some(&identity))?;
    persisted.run.current_head_sha = Some(updated_head.clone());
    persisted.run.pr_number = Some(summary.number);
    persisted.run.pr_url = Some(summary.url.clone());
    persisted.run.review_baseline_json = Some(review_baseline_json(summary));
    let result = format!(
        "updated PR #{} with {} at {}",
        summary.number, summary.base_ref, updated_head
    );
    let step_id = persisted.steps[step_index]
        .id
        .ok_or_else(|| "base update step must be saved before output".to_string())?;
    append_system_output(
        conn,
        step_id,
        AutoOutputKind::Status,
        &result,
        None,
        max_output_lines_per_step,
    )?;
    finish_non_agent_step(
        conn,
        &mut persisted.steps[step_index],
        AutoStepStatus::Done,
        Some(result),
        None,
    )?;
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
    let merge_enabled = crate::integration::ensure_default_merge_intent(
        conn,
        &persisted.run.id,
        config.auto.merge,
    )?;
    if !merge_enabled {
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
    if crate::integration::active_merge_intent(conn, &persisted.run.id)?.is_some_and(|intent| {
        matches!(
            intent.placement,
            crate::integration::IntegrationPlacement::Submitting
                | crate::integration::IntegrationPlacement::Submitted
        )
    }) {
        let summary = "reconciling an interrupted provider merge submission".to_string();
        append_system_output(
            conn,
            step_id,
            AutoOutputKind::Status,
            &summary,
            None,
            max_output_lines_per_step,
        )?;
        set_auto_step_waiting(conn, &mut persisted.steps[step_index], summary)?;
        persisted.run.stabilization_status =
            Some(stabilization_model::StabilizationStatus::Waiting);
        persisted.run.stabilization_blocker =
            Some(stabilization_model::StabilizationBlocker::ReadyToAutoMerge);
        persisted.run.stabilization_next_work =
            Some(stabilization_model::StabilizationWorkKind::Merge);
        persisted.run.status = persisted.authoritative_status();
        persisted.run.updated_unix_ms = unix_ms();
        save_run_with_conn(conn, &persisted.run)?;
        reconcile_waiting_merge_until_complete(
            conn,
            repo,
            config,
            persisted,
            step_index,
            max_output_lines_per_step,
        )?;
        return Ok(());
    }

    crate::execution::validate_installed_claim(conn)?;
    let verify =
        crate::verify::run_auto_verify(config, &persisted.run.worktree_path, VerifyMode::Normal);
    crate::git::fetch_origin(&persisted.run.worktree_path, config)?;
    let mut effective_config = config.clone();
    effective_config.auto.merge = true;
    let snapshot = stabilization_observe::build_auto_run_stabilization_snapshot(
        repo,
        &persisted.run,
        &effective_config,
    );
    let expected_guard = persisted.steps[step_index]
        .work_guard
        .as_ref()
        .ok_or_else(|| "auto merge step is missing its stabilization work guard".to_string())?;
    let observed_identity = snapshot
        .pull_request
        .as_ref()
        .and_then(|pull_request| pull_request.change_request_identity.as_ref());
    save_observed_change_request_identity(conn, &persisted.run.id, observed_identity)?;
    let persisted_identity = load_observed_change_request_identity(conn, &persisted.run.id)?;
    let authorization = stabilization_execute::authorize_auto_merge(
        &snapshot,
        persisted.run.pr_number,
        expected_guard,
    );
    let submission_mode = if snapshot.repository.merge_queue_required {
        crate::remote::MergeSubmissionMode::NativeQueue
    } else {
        crate::remote::MergeSubmissionMode::Immediate
    };
    let gate = if persisted_identity.as_ref() != observed_identity {
        MergeGateOutcome {
            allowed: false,
            summary: "merge blocked: canonical change request identity was not durably observed"
                .to_string(),
        }
    } else if !verify.passed {
        MergeGateOutcome {
            allowed: false,
            summary: format!("merge blocked:\n- {}", format_verify_result(&verify)),
        }
    } else {
        match &authorization {
            stabilization_execute::MergeAuthorization::Authorized(_) => MergeGateOutcome {
                allowed: true,
                summary: "fresh stabilization observation authorized auto-merge".to_string(),
            },
            stabilization_execute::MergeAuthorization::ReviewResolutionRequired {
                state, ..
            }
            | stabilization_execute::MergeAuthorization::Blocked(state) => MergeGateOutcome {
                allowed: false,
                summary: format!("merge blocked:\n- {}", state.reason),
            },
        }
    };
    if !gate.allowed {
        append_system_output(
            conn,
            step_id,
            AutoOutputKind::Error,
            &gate.summary,
            None,
            max_output_lines_per_step,
        )?;
        finish_non_agent_step(
            conn,
            &mut persisted.steps[step_index],
            AutoStepStatus::Failed,
            Some("merge blocked by final gate".to_string()),
            Some(gate.summary.clone()),
        )?;
        return Err(gate.summary);
    }

    if let Some(pull_request) = snapshot.pull_request.as_ref()
        && let Some(identity) = pull_request.change_request_identity.clone()
    {
        crate::integration::synchronize_generation(
            conn,
            &persisted.run.id,
            &crate::integration::CandidateGeneration {
                change_request_identity: identity,
                target_branch: pull_request.base_ref.clone(),
                pr_number: pull_request.number,
                head_sha: pull_request.head_sha.clone(),
            },
        )?;
        let intent = crate::integration::active_merge_intent(conn, &persisted.run.id)?
            .ok_or_else(|| "merge intent was withdrawn before provider submission".to_string())?;
        if intent.placement != crate::integration::IntegrationPlacement::Reserved {
            return Err("pull request no longer owns the integration lane".to_string());
        }
        crate::integration::mark_submitting(conn, &persisted.run.id)?;
    }
    crate::execution::validate_installed_claim(conn)?;
    let execution = match stabilization_execute::execute_merge_authorization_with_mode(
        config,
        &persisted.run.worktree_path,
        authorization,
        submission_mode,
    ) {
        Ok(execution) => execution,
        Err(error) => {
            keep_waiting_for_merge(
                conn,
                persisted,
                step_index,
                format!("provider merge invocation was uncertain; reconciling: {error}"),
                max_output_lines_per_step,
            )?;
            return match reconcile_waiting_merge_until_complete(
                conn,
                repo,
                config,
                persisted,
                step_index,
                max_output_lines_per_step,
            )? {
                MergeReconciliationProgress::RetrySubmission => Err(error),
                MergeReconciliationProgress::Done => Ok(()),
                MergeReconciliationProgress::Waiting => Ok(()),
            };
        }
    };
    crate::integration::mark_submitted(conn, &persisted.run.id)?;
    let (result, mutation_state) = match execution {
        stabilization_execute::ManualMergeExecution::Merged { result } => (result, "merged"),
        stabilization_execute::ManualMergeExecution::Pending { result } => (result, "accepted"),
        stabilization_execute::ManualMergeExecution::Uncertain { result } => (result, "uncertain"),
        stabilization_execute::ManualMergeExecution::Blocked(_) => {
            unreachable!("the final gate only passes an authorized merge")
        }
    };
    let pr_number = result
        .summary
        .change_request
        .id
        .display_number()
        .ok_or_else(|| "change request mutation has no display number".to_string())?;
    let mut cache = crate::remote::load_pr_cache(repo, &persisted.run.branch);
    crate::remote::dispatcher::record_change_request_summary(
        repo,
        &persisted.run.branch,
        &mut cache,
        result.summary.clone(),
    )?;
    append_system_output(
        conn,
        step_id,
        AutoOutputKind::Status,
        &gate.summary,
        None,
        max_output_lines_per_step,
    )?;
    if mutation_state != "merged" {
        let summary = if mutation_state == "accepted" {
            format!(
                "merge accepted for PR #{pr_number} and pending (provider state: {})",
                result.native_state
            )
        } else {
            format!(
                "merge outcome for PR #{pr_number} is uncertain; reconciling (provider state: {})",
                result.native_state
            )
        };
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
        )?;
        persisted.run.stabilization_status =
            Some(stabilization_model::StabilizationStatus::Waiting);
        persisted.run.stabilization_blocker =
            Some(stabilization_model::StabilizationBlocker::ReadyToAutoMerge);
        persisted.run.stabilization_next_work =
            Some(stabilization_model::StabilizationWorkKind::Merge);
        persisted.run.updated_unix_ms = unix_ms();
        save_run_with_conn(conn, &persisted.run)?;
        reconcile_waiting_merge_until_complete(
            conn,
            repo,
            config,
            persisted,
            step_index,
            max_output_lines_per_step,
        )?;
        return Ok(());
    }
    let observed = crate::remote::dispatcher::wait_for_change_request_merged(
        &persisted.run.worktree_path,
        &result.summary.change_request,
        config,
    )?;
    crate::remote::dispatcher::record_change_request_summary(
        repo,
        &persisted.run.branch,
        &mut cache,
        observed.clone(),
    )?;
    if observed.lifecycle != crate::remote::LifecycleState::Merged {
        let error = format!(
            "PR #{pr_number} merge command completed, but the provider has not marked it merged yet"
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
    crate::remote::dispatcher::refresh_change_request_cache(
        repo,
        &persisted.run.branch,
        &mut cache,
        &persisted.run.worktree_path,
        config,
        true,
    )?;
    stabilization_execute::observe_plan_and_save(conn, repo, config, persisted)?;
    let done = format!("merged PR #{pr_number}");
    finish_merged_integration(conn, persisted, step_index, done, max_output_lines_per_step)
}

fn finish_merged_integration(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    summary: String,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    let original = persisted.clone();
    let result = (|| {
        let transaction =
            crate::flight_recorder::TransactionTrace::begin("auto_run.complete_integration");
        let tx =
            rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)
                .map_err(|error| format!("begin integration completion transaction: {error}"))?;
        append_merge_reconciliation_output(
            &tx,
            &persisted.steps[step_index],
            AutoOutputKind::Status,
            &summary,
            max_output_lines_per_step,
        )?;
        finish_non_agent_step(
            &tx,
            &mut persisted.steps[step_index],
            AutoStepStatus::Done,
            Some(summary),
            None,
        )?;
        persisted.run.stabilization_status = Some(stabilization_model::StabilizationStatus::Done);
        persisted.run.stabilization_blocker =
            Some(stabilization_model::StabilizationBlocker::Merged);
        persisted.run.stabilization_next_work =
            Some(stabilization_model::StabilizationWorkKind::Done);
        persisted.run.status = persisted.authoritative_status();
        persisted.run.updated_unix_ms = unix_ms();
        save_run_with_conn(&tx, &persisted.run)?;
        if crate::integration::active_merge_intent(&tx, &persisted.run.id)?.is_some() {
            crate::integration::complete_merge_in_transaction(&tx, &persisted.run.id)?;
        }
        tx.commit()
            .map_err(|error| format!("commit integration completion transaction: {error}"))?;
        transaction.committed();
        Ok(())
    })();
    if result.is_err() {
        *persisted = original;
    }
    result
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MergeReconciliationProgress {
    Waiting,
    RetrySubmission,
    Done,
}

pub(super) fn reconcile_waiting_merge_until_complete(
    conn: &rusqlite::Connection,
    repo: &Repository,
    config: &Config,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    max_output_lines_per_step: usize,
) -> Result<MergeReconciliationProgress, String> {
    loop {
        crate::execution::validate_installed_claim(conn)?;
        let worktree_path = persisted.run.worktree_path.clone();
        let progress = reconcile_waiting_merge_step_with(
            conn,
            repo,
            persisted,
            step_index,
            max_output_lines_per_step,
            |expected| {
                crate::remote::dispatcher::wait_for_change_request_merged(
                    &worktree_path,
                    expected,
                    config,
                )
            },
        )?;
        if progress != MergeReconciliationProgress::Waiting {
            return Ok(progress);
        }

        interruptible_execution_sleep(conn, config.auto.ci_poll_interval_seconds)?;
        if reload_pause_request(conn, persisted)? {
            return Ok(MergeReconciliationProgress::Waiting);
        }
    }
}

pub(super) fn reconcile_waiting_merge_step_with<F>(
    conn: &rusqlite::Connection,
    repo: &Repository,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    max_output_lines_per_step: usize,
    observe: F,
) -> Result<MergeReconciliationProgress, String>
where
    F: FnOnce(&crate::remote::ChangeRequest) -> Result<crate::remote::ChangeRequestSummary, String>,
{
    let expected = match pending_merge_change_request(conn, persisted, step_index) {
        Ok(expected) => expected,
        Err(error) => {
            fail_waiting_merge_step(
                conn,
                persisted,
                step_index,
                &error,
                max_output_lines_per_step,
                MergeReservationFailure::Retain,
            )?;
            return Err(error);
        }
    };
    let observed = match observe(&expected) {
        Ok(observed) => observed,
        Err(error) => {
            return keep_waiting_for_merge(
                conn,
                persisted,
                step_index,
                format!("merge observation unavailable; keeping the submission reserved: {error}"),
                max_output_lines_per_step,
            );
        }
    };
    if let Err(error) = validate_pending_merge_observation(&expected, &observed) {
        fail_waiting_merge_step(
            conn,
            persisted,
            step_index,
            &error,
            max_output_lines_per_step,
            MergeReservationFailure::Release,
        )?;
        return Err(error);
    }
    let pr_number = observed
        .change_request
        .id
        .display_number()
        .or(persisted.run.pr_number)
        .ok_or_else(|| "pending merge change request has no display number".to_string())?;
    let mut cache = crate::remote::load_pr_cache(repo, &persisted.run.branch);
    crate::remote::dispatcher::record_change_request_summary(
        repo,
        &persisted.run.branch,
        &mut cache,
        observed.clone(),
    )?;

    match &observed.lifecycle {
        crate::remote::LifecycleState::Merged => {
            let summary = format!("merged PR #{pr_number}");
            finish_merged_integration(
                conn,
                persisted,
                step_index,
                summary,
                max_output_lines_per_step,
            )?;
            Ok(MergeReconciliationProgress::Done)
        }
        crate::remote::LifecycleState::Closed => {
            let error =
                format!("PR #{pr_number} closed without merging after its merge was accepted");
            fail_waiting_merge_step(
                conn,
                persisted,
                step_index,
                &error,
                max_output_lines_per_step,
                MergeReservationFailure::Release,
            )?;
            Err(error)
        }
        crate::remote::LifecycleState::Open
            if observed.queue_state == crate::remote::QueueState::NotQueued =>
        {
            if crate::integration::active_merge_intent(conn, &persisted.run.id)?.is_some_and(
                |intent| intent.placement == crate::integration::IntegrationPlacement::Submitting,
            ) {
                requeue_unobserved_merge_submission(
                    conn,
                    persisted,
                    step_index,
                    pr_number,
                    max_output_lines_per_step,
                )?;
                return Ok(MergeReconciliationProgress::RetrySubmission);
            }
            let error = format!("PR #{pr_number} is no longer queued after its merge was accepted");
            fail_waiting_merge_step(
                conn,
                persisted,
                step_index,
                &error,
                max_output_lines_per_step,
                MergeReservationFailure::Release,
            )?;
            Err(error)
        }
        crate::remote::LifecycleState::Open | crate::remote::LifecycleState::Unknown(_) => {
            if crate::integration::active_merge_intent(conn, &persisted.run.id)?.is_some_and(
                |intent| intent.placement == crate::integration::IntegrationPlacement::Submitting,
            ) && matches!(
                observed.queue_state,
                crate::remote::QueueState::Queued
                    | crate::remote::QueueState::Running
                    | crate::remote::QueueState::Blocked
            ) {
                crate::integration::mark_submitted(conn, &persisted.run.id)?;
            }
            keep_waiting_for_merge(
                conn,
                persisted,
                step_index,
                format!(
                    "merge for PR #{pr_number} is still pending (provider state: {})",
                    merge_pending_state(&observed)
                ),
                max_output_lines_per_step,
            )
        }
    }
}

fn requeue_unobserved_merge_submission(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    pr_number: u64,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    let original = persisted.clone();
    let result = (|| {
        let transaction =
            crate::flight_recorder::TransactionTrace::begin("auto_run.retry_merge_submission");
        let tx =
            rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)
                .map_err(|error| format!("begin merge submission retry transaction: {error}"))?;
        let summary = format!(
            "provider has no merge or queue entry for PR #{pr_number}; retrying guarded submission"
        );
        append_merge_reconciliation_output(
            &tx,
            &persisted.steps[step_index],
            AutoOutputKind::Status,
            &summary,
            max_output_lines_per_step,
        )?;
        crate::integration::retry_unobserved_submission(&tx, &persisted.run.id)?;
        reset_auto_step_for_retry(&mut persisted.steps[step_index]);
        save_step_with_conn(&tx, &mut persisted.steps[step_index])?;
        persisted.run.status = persisted.authoritative_status();
        persisted.run.updated_unix_ms = unix_ms();
        save_run_with_conn(&tx, &persisted.run)?;
        tx.commit()
            .map_err(|error| format!("commit merge submission retry transaction: {error}"))?;
        transaction.committed();
        Ok(())
    })();
    if result.is_err() {
        *persisted = original;
    }
    result
}

fn keep_waiting_for_merge(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    summary: String,
    max_output_lines_per_step: usize,
) -> Result<MergeReconciliationProgress, String> {
    append_merge_reconciliation_output(
        conn,
        &persisted.steps[step_index],
        AutoOutputKind::Status,
        &summary,
        max_output_lines_per_step,
    )?;
    set_auto_step_waiting(conn, &mut persisted.steps[step_index], summary.clone())?;
    persisted.run.stabilization_status = Some(stabilization_model::StabilizationStatus::Waiting);
    persisted.run.status = persisted.authoritative_status();
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)?;
    append_auto_event(
        conn,
        &AutoEvent {
            id: None,
            run_id: persisted.run.id.clone(),
            step_run_id: persisted.steps[step_index].id,
            time_unix_ms: unix_ms(),
            kind: "merge_wait_poll".to_string(),
            data_json: format!("{{\"summary\":{}}}", json_string(&summary)),
        },
    )?;
    Ok(MergeReconciliationProgress::Waiting)
}

fn validate_pending_merge_observation(
    expected: &crate::remote::ChangeRequest,
    observed: &crate::remote::ChangeRequestSummary,
) -> Result<(), String> {
    let observed = &observed.change_request;
    if observed.id != expected.id
        || observed.source_repository != expected.source_repository
        || observed.target_repository != expected.target_repository
        || observed.source_branch != expected.source_branch
        || observed.target_branch != expected.target_branch
    {
        return Err(
            "change request identity or target changed while merge was pending".to_string(),
        );
    }
    if observed.head_sha != expected.head_sha {
        return Err("change request head changed while merge was pending".to_string());
    }
    Ok(())
}

fn pending_merge_change_request(
    conn: &rusqlite::Connection,
    persisted: &PersistedAutoRun,
    step_index: usize,
) -> Result<crate::remote::ChangeRequest, String> {
    let guard = persisted.steps[step_index]
        .work_guard
        .as_ref()
        .ok_or_else(|| "waiting merge step has no persisted work guard".to_string())?;
    let identity = guard
        .change_request_identity
        .as_ref()
        .ok_or_else(|| "waiting merge step has no canonical change request identity".to_string())?;
    if load_observed_change_request_identity(conn, &persisted.run.id)?.as_ref() != Some(identity) {
        return Err(
            "canonical change request identity changed or was lost while merge was pending"
                .to_string(),
        );
    }
    let display_number = persisted
        .run
        .pr_number
        .ok_or_else(|| "waiting merge step has no change request number".to_string())?;
    let source_repository = identity
        .source_repository()
        .map_err(|error| error.to_string())?;
    let target_repository = identity
        .target_repository()
        .map_err(|error| error.to_string())?;
    Ok(crate::remote::ChangeRequest {
        id: identity
            .change_request_id(Some(display_number))
            .map_err(|error| error.to_string())?,
        source_repository,
        target_repository,
        source_branch: persisted.run.branch.clone(),
        target_branch: guard
            .authorized_target_branch
            .clone()
            .ok_or_else(|| "waiting merge step has no authorized target branch".to_string())?,
        head_sha: guard
            .pr_head_sha
            .clone()
            .ok_or_else(|| "waiting merge step has no authorized head SHA".to_string())?,
    })
}

fn merge_pending_state(observed: &crate::remote::ChangeRequestSummary) -> String {
    match &observed.queue_state {
        crate::remote::QueueState::NotQueued => "not queued".to_string(),
        crate::remote::QueueState::Queued => "queued".to_string(),
        crate::remote::QueueState::Running => "running".to_string(),
        crate::remote::QueueState::Blocked => "blocked".to_string(),
        crate::remote::QueueState::Complete => "complete".to_string(),
        crate::remote::QueueState::Unknown(native) => native.clone(),
    }
}

fn append_merge_reconciliation_output(
    conn: &rusqlite::Connection,
    step: &AutoStepRun,
    kind: AutoOutputKind,
    summary: &str,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    let step_id = step
        .id
        .ok_or_else(|| "waiting merge step must be saved before output".to_string())?;
    append_system_output(
        conn,
        step_id,
        kind,
        summary,
        None,
        max_output_lines_per_step,
    )
}

#[derive(Clone, Copy)]
enum MergeReservationFailure {
    Retain,
    Release,
}

fn fail_waiting_merge_step(
    conn: &rusqlite::Connection,
    persisted: &mut PersistedAutoRun,
    step_index: usize,
    error: &str,
    max_output_lines_per_step: usize,
    reservation: MergeReservationFailure,
) -> Result<(), String> {
    let original = persisted.clone();
    let result = (|| {
        let transaction =
            crate::flight_recorder::TransactionTrace::begin("auto_run.fail_waiting_merge");
        let tx =
            rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)
                .map_err(|error| format!("begin waiting merge failure transaction: {error}"))?;
        fail_step(
            &tx,
            &mut persisted.steps[step_index],
            error,
            max_output_lines_per_step,
        )?;
        persisted.run.stabilization_status =
            Some(stabilization_model::StabilizationStatus::Escalated);
        persisted.run.stabilization_blocker =
            Some(stabilization_model::StabilizationBlocker::ObservationFailed);
        persisted.run.stabilization_next_work =
            Some(stabilization_model::StabilizationWorkKind::Escalate);
        persisted.run.status = AutoRunStatus::Failed;
        persisted.run.pause_requested = false;
        persisted.run.updated_unix_ms = unix_ms();
        save_run_with_conn(&tx, &persisted.run)?;
        if matches!(reservation, MergeReservationFailure::Release) {
            crate::integration::release_submitted_reservation_in_transaction(
                &tx,
                &persisted.run.id,
            )?;
        }
        tx.commit()
            .map_err(|error| format!("commit waiting merge failure transaction: {error}"))?;
        transaction.committed();
        Ok(())
    })();
    if result.is_err() {
        *persisted = original;
    }
    result
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

    let expected_incarnation = persisted
        .run
        .worktree_incarnation
        .as_deref()
        .filter(|incarnation| !incarnation.is_empty())
        .ok_or_else(|| {
            "auto cleanup retained the worktree because this run has no persisted worktree incarnation"
                .to_string()
        })?;
    let deletion_pending = crate::session::worktree_deletion_is_pending(
        repo,
        &persisted.run.worktree_path,
        &persisted.run.branch,
        expected_incarnation,
    )?;
    let worktree_removed = crate::session::worktree_removal_is_complete(
        repo,
        &persisted.run.worktree_path,
        &persisted.run.branch,
        expected_incarnation,
    )?;
    if !deletion_pending
        && crate::session::worktree_incarnation(&persisted.run.worktree_path)
            != expected_incarnation
    {
        return Err(format!(
            "worktree {} was replaced while deletion was pending; retained the replacement",
            persisted.run.branch
        ));
    }
    match worktree_removed
        .then_some(crate::worktrunk::ApprovalStatus::Approved)
        .map(Ok)
        .unwrap_or_else(|| crate::worktrunk::approval_status(repo, config))
        .map_err(|error| error.to_string())?
    {
        crate::worktrunk::ApprovalStatus::Pending => {
            return Err(format!(
                "auto cleanup requires interactive approval for Worktrunk project commands; retained the worktree and Prism metadata. Run:\n{}",
                crate::worktrunk::approval_command_display(repo, config)
            ));
        }
        crate::worktrunk::ApprovalStatus::Approved
        | crate::worktrunk::ApprovalStatus::NotWorktrunk => {}
    }
    crate::execution::validate_installed_claim(conn)?;
    let outcome = crate::session::delete_worktree_session_if_current(
        repo,
        config,
        &persisted.run.worktree_path,
        &persisted.run.branch,
        Some(expected_incarnation),
    )?;
    let (status, summary, error) = match outcome {
        crate::session::DeleteWorktreeOutcome::Deleted => (
            AutoStepStatus::Done,
            "deleted local session data, worktree, and branch".to_string(),
            None,
        ),
        crate::session::DeleteWorktreeOutcome::BranchRetained { error, .. } => (
            AutoStepStatus::Failed,
            format!("worktree removed, but branch was retained: {error}"),
            Some(error),
        ),
        crate::session::DeleteWorktreeOutcome::DeletedWithWarnings { errors, .. } => {
            let error = errors.join("; ");
            (
                AutoStepStatus::Failed,
                format!("worktree deletion completed with warnings: {error}"),
                Some(error),
            )
        }
    };
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
        status,
        Some(summary),
        error.clone(),
    )?;
    persisted.run.updated_unix_ms = unix_ms();
    save_run_with_conn(conn, &persisted.run)?;
    if let Some(error) = error {
        Err(error)
    } else {
        Ok(())
    }
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
    step.execution.process_id = None;
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
    pub(super) review_thread_ids: Vec<String>,
    pub(super) complete: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CiPollOutcome {
    pub(super) state: PrCheckState,
    pub(super) summary: String,
    pub(super) prompt: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MergeGateOutcome {
    allowed: bool,
    summary: String,
}

pub(super) fn poll_ci_status(
    conn: &rusqlite::Connection,
    repo: &Repository,
    config: &Config,
    persisted: &mut PersistedAutoRun,
) -> Result<CiPollOutcome, String> {
    let mut cache = crate::remote::load_pr_cache(repo, &persisted.run.branch);
    crate::remote::dispatcher::refresh_change_request_cache(
        repo,
        &persisted.run.branch,
        &mut cache,
        &persisted.run.worktree_path,
        config,
        true,
    )?;
    let summary = cache
        .trusted_summary()?
        .ok_or_else(|| "CI wait could not find pull request summary".to_string())?;
    save_observed_change_request_identity(
        conn,
        &persisted.run.id,
        summary.change_request_identity.as_ref(),
    )?;
    persisted.run.pr_number = Some(summary.number);
    persisted.run.pr_url = Some(summary.url.clone());
    persisted.run.current_head_sha = Some(summary.head_sha.clone());
    evaluate_ci_status(
        config,
        &persisted.run.branch,
        summary,
        cache.trusted_details()?,
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
    conn: &rusqlite::Connection,
    repo: &Repository,
    config: &Config,
    persisted: &mut PersistedAutoRun,
) -> Result<ReviewPollOutcome, String> {
    let mut cache = crate::remote::load_pr_cache(repo, &persisted.run.branch);
    crate::remote::dispatcher::refresh_change_request_cache(
        repo,
        &persisted.run.branch,
        &mut cache,
        &persisted.run.worktree_path,
        config,
        true,
    )?;
    let summary = cache
        .trusted_summary()?
        .ok_or_else(|| "review wait could not find pull request summary".to_string())?;
    save_observed_change_request_identity(
        conn,
        &persisted.run.id,
        summary.change_request_identity.as_ref(),
    )?;
    persisted.run.pr_number = Some(summary.number);
    persisted.run.pr_url = Some(summary.url.clone());
    persisted.run.current_head_sha = Some(summary.head_sha.clone());
    if persisted.run.review_baseline_json.is_none() {
        persisted.run.review_baseline_json = Some(review_baseline_json(summary));
    }
    evaluate_review_feedback(config, persisted, summary, cache.trusted_details()?)
}

pub(super) fn evaluate_review_feedback(
    config: &Config,
    persisted: &mut PersistedAutoRun,
    summary: &crate::remote::PrSummary,
    details: Option<&crate::remote::PrDetails>,
) -> Result<ReviewPollOutcome, String> {
    let baseline = parse_review_baseline(persisted.run.review_baseline_json.as_deref());
    let after = baseline
        .as_ref()
        .filter(|baseline| baseline.head_sha == summary.head_sha)
        .map(|baseline| baseline.updated_at.as_str());
    let Some(details) = details else {
        return Ok(ReviewPollOutcome {
            summary: "PR details are not available yet; waiting for review feedback".to_string(),
            fix_prompt: None,
            review_thread_ids: Vec::new(),
            complete: false,
        });
    };
    let mut feedback = actionable_review_feedback(
        details,
        ReviewFeedbackFilter {
            after,
            authors: &[],
        },
    );
    // Conversation-resolution policy applies to every unresolved thread, including
    // feedback that predates this run's repair baseline.
    feedback.inline_comments = actionable_review_feedback(
        details,
        ReviewFeedbackFilter {
            after: None,
            authors: &[],
        },
    )
    .inline_comments;
    if feedback.is_actionable() {
        let prompt =
            render_auto_review_fix_prompt(summary.number, &persisted.run.branch, &feedback);
        return Ok(ReviewPollOutcome {
            summary: format_review_feedback_summary(&feedback),
            fix_prompt: Some(prompt),
            review_thread_ids: crate::review::review_thread_ids(&feedback),
            complete: false,
        });
    }
    if config.auto.review_requirement == crate::config::ReviewRequirement::Resolved {
        let review_comment_count = details
            .review_comments
            .iter()
            .filter(|comment| !comment.body.trim().is_empty())
            .count();
        return Ok(ReviewPollOutcome {
            summary: if review_comment_count == 0 {
                "no review comments found yet".to_string()
            } else {
                format!("all {review_comment_count} review comment(s) are resolved")
            },
            fix_prompt: None,
            review_thread_ids: Vec::new(),
            complete: review_comment_count > 0,
        });
    }
    if !has_configured_reviewer_requested(summary, config) {
        return Ok(ReviewPollOutcome {
            summary:
                "no automated reviewer feedback or pending configured reviewer found; continuing"
                    .to_string(),
            fix_prompt: None,
            review_thread_ids: Vec::new(),
            complete: true,
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
            review_thread_ids: Vec::new(),
            complete: true,
        });
    }
    if summary.review_decision == "APPROVED" {
        return Ok(ReviewPollOutcome {
            summary: "review decision is approved; continuing".to_string(),
            fix_prompt: None,
            review_thread_ids: Vec::new(),
            complete: true,
        });
    }
    Ok(ReviewPollOutcome {
        summary: "no review feedback found yet".to_string(),
        fix_prompt: None,
        review_thread_ids: Vec::new(),
        complete: false,
    })
}

pub(super) fn has_configured_reviewer_requested(
    summary: &crate::remote::PrSummary,
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

pub(super) fn review_baseline_json(summary: &crate::remote::PrSummary) -> String {
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
        "Resolve the actionable review feedback for PR #{pr_number} on branch {branch}. Commit your changes, but do not push.\n\n"
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

fn current_work_guard(
    config: &Config,
    persisted: &PersistedAutoRun,
    cache: &crate::remote::PrCache,
) -> Result<stabilization_model::WorkGuard, String> {
    let summary = cache.trusted_summary()?;
    let remote_head_sha = stabilization_observe::push_remote_head_sha(
        &persisted.run.worktree_path,
        &persisted.run.branch,
        config,
    )?;
    let base_sha = match summary {
        Some(summary) => crate::remote::dispatcher::fetch_change_request_base_head_sha(
            &persisted.run.worktree_path,
            config,
            summary,
        )?,
        None => None,
    };
    let review_thread_ids = cache
        .trusted_details()?
        .map(|details| {
            let feedback = stabilization_observe::stabilization_review_feedback(
                details,
                persisted.run.review_baseline_json.as_deref(),
            );
            crate::review::review_thread_ids(&feedback)
        })
        .unwrap_or_default();
    Ok(stabilization_model::WorkGuard {
        change_request_identity: summary
            .and_then(|summary| summary.change_request_identity.clone()),
        authorized_target_branch: summary.map(|summary| summary.base_ref.clone()),
        local_head_sha: Some(crate::git::current_head_sha(
            &persisted.run.worktree_path,
            config,
        )?),
        remote_head_sha,
        pr_head_sha: summary
            .map(|summary| summary.head_sha.clone())
            .filter(|sha| !sha.trim().is_empty()),
        base_sha,
        review_thread_ids,
    })
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
        AutoImplementationSource::ExistingPullRequest => {
            Err("existing-PR auto flow does not have a plan path".to_string())
        }
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
