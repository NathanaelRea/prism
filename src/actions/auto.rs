use super::*;

pub(super) enum AutoStartupSource {
    Prompt,
    ExistingPlan,
    DraftPlan,
}

pub(super) fn validate_existing_auto_plan(plan_path: &Path) -> Result<(), String> {
    if !plan_path.is_file() {
        return Err(format!("plan file not found: {}", plan_path.display()));
    }
    if infer_total_phases(plan_path)? == 0 {
        return Err("could not infer phases; add headings like 'Phase 1'".to_string());
    }
    Ok(())
}

pub(super) fn next_auto_step_description(run: &PersistedAutoRun) -> Option<String> {
    let step = run.steps.iter().find(|step| {
        step.status == AutoStepStatus::Queued
            || matches!(step.status, AutoStepStatus::Waiting)
                && matches!(step.step_key, AutoStepKey::RunPlan)
    })?;
    let detail = step.summary.as_deref().or(step.reason.as_deref());
    Some(match detail {
        Some(detail) if !detail.trim().is_empty() => {
            format!("#{} {} ({})", step.sequence, step.step_key.as_str(), detail)
        }
        _ => format!("#{} {}", step.sequence, step.step_key.as_str()),
    })
}

impl Tui {
    pub(crate) fn start_or_focus_selected_auto_run(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let Some(context) = self.selected_worktree_context() else {
            return Ok(());
        };
        let session_path = self.sessions[context.session_index].path.clone();
        let session_branch = self.sessions[context.session_index].branch.clone();
        let session_incarnation = self.sessions[context.session_index].incarnation.clone();
        if let Some(run_id) = self.active_auto_runs.get(&session_path).cloned() {
            self.load_auto_run_snapshot(&context.repo.root, &run_id);
            self.selected_auto_run = Some(run_id);
            self.show_message("focused Auto Flow run")?;
            return Ok(());
        }
        if !context.config.selected_harness()?.describe().headless {
            return Err(format!(
                "harness '{}' does not support managed Auto Flow execution; configure headless_command and headless_prompt_transport",
                context.config.default_harness
            ));
        }
        let is_detached = session_branch == "(detached)";
        if context.config.is_default_branch(&session_branch) || is_detached {
            return if is_detached {
                Err("Auto Flow cannot start on a detached worktree".to_string())
            } else {
                Err("Auto Flow cannot start on the default branch".to_string())
            };
        }
        if selected_dirty(&session_path, &context.config)? {
            return Err("Auto Flow requires a clean worktree at launch".to_string());
        }
        let _ = refresh_repo_policy_cache(&context.repo, &session_path, &context.config);
        let Some(source) = self.prompt_auto_implementation_source(raw)? else {
            return Ok(());
        };
        let (mode, implementation_source, plan_path, plan_run_mode, variant, prompt) = match source
        {
            AutoStartupSource::Prompt => {
                let Some(prompt) =
                    self.prompt_line_dialog(raw, "Auto Flow", "Initial prompt: ", "")?
                else {
                    return Ok(());
                };
                if prompt.trim().is_empty() {
                    return Ok(());
                }
                (
                    AutoRunMode::Standard,
                    AutoImplementationSource::Prompt,
                    None,
                    PlanRunMode::Sequential,
                    "default".to_string(),
                    prompt.trim().to_string(),
                )
            }
            AutoStartupSource::ExistingPlan => {
                raw.suspend()?;
                let selected = select_plan_path(&session_path, &context.config);
                let resume_result = raw.resume();
                resume_result?;
                let plan_path = selected?;
                validate_existing_auto_plan(&plan_path)?;
                let Some(plan_run_mode) = self.prompt_auto_plan_run_mode(raw)? else {
                    return Ok(());
                };
                (
                    AutoRunMode::Standard,
                    AutoImplementationSource::ExistingPlan,
                    Some(plan_path.clone()),
                    plan_run_mode,
                    "plan".to_string(),
                    format!("Run plan phases from {}", plan_path.display()),
                )
            }
            AutoStartupSource::DraftPlan => {
                let plan_path = session_path.join("plan.md");
                if plan_path.exists() {
                    return Err(
                            "worktree/plan.md already exists; choose existing-plan mode or move/remove the file"
                                .to_string(),
                        );
                }
                let Some(prompt) =
                    self.prompt_line_dialog(raw, "Auto Flow", "Task prompt: ", "")?
                else {
                    return Ok(());
                };
                if prompt.trim().is_empty() {
                    return Ok(());
                }
                let Some(plan_run_mode) = self.prompt_auto_plan_run_mode(raw)? else {
                    return Ok(());
                };
                (
                    AutoRunMode::PlanFirst,
                    AutoImplementationSource::DraftPlan,
                    Some(plan_path),
                    plan_run_mode,
                    "draft-plan".to_string(),
                    prompt.trim().to_string(),
                )
            }
        };
        let launch = AutoLaunch::with_options(
            &context.repo.root,
            &session_path,
            AutoLaunchOptions {
                branch: session_branch,
                mode,
                implementation_source,
                plan_path,
                plan_run_mode,
                variant,
                agent_profile: None,
                initial_prompt: prompt,
            },
        )?
        .with_harness(
            context.config.default_harness.clone(),
            context
                .config
                .harness_adapter(&context.config.default_harness)?,
        )
        .with_worktree_incarnation(session_incarnation);
        let mut persisted = launch.create_run();
        crate::observability::with_writable_db(&context.repo, |conn| {
            save_auto_run(conn, &mut persisted)
        })?;
        let run_id = persisted.run.id.clone();
        self.remember_auto_run(persisted.clone());
        self.selected_auto_run = Some(run_id);
        self.spawn_auto_run_executor(context.repo, context.config, persisted);
        self.show_message("started Auto Flow run")?;
        Ok(())
    }

    pub(super) fn prompt_auto_implementation_source(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<Option<AutoStartupSource>, String> {
        let answer = self.prompt_choice_dialog(
            raw,
            crate::view::ChoiceList {
                title: "Auto Flow: Implementation Source".to_string(),
                choices: [("p", "prompt"), ("e", "existing plan"), ("d", "draft plan")]
                    .into_iter()
                    .map(|(key, label)| crate::view::KeyChoice {
                        key: key.to_string(),
                        label: label.to_string(),
                    })
                    .collect(),
            },
        )?;
        Ok(match answer.as_deref() {
            Some("p") => Some(AutoStartupSource::Prompt),
            Some("e") => Some(AutoStartupSource::ExistingPlan),
            Some("d") => Some(AutoStartupSource::DraftPlan),
            _ => None,
        })
    }

    pub(super) fn prompt_auto_plan_run_mode(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<Option<PlanRunMode>, String> {
        let answer = self.prompt_choice_dialog(
            raw,
            crate::view::ChoiceList {
                title: "Auto Flow: Plan Execution".to_string(),
                choices: [("s", "sequential"), ("p", "parallel")]
                    .into_iter()
                    .map(|(key, label)| crate::view::KeyChoice {
                        key: key.to_string(),
                        label: label.to_string(),
                    })
                    .collect(),
            },
        )?;
        Ok(match answer.as_deref() {
            Some("s") => Some(PlanRunMode::Sequential),
            Some("p") => Some(PlanRunMode::Parallel),
            _ => None,
        })
    }

    pub(super) fn spawn_auto_run_executor(
        &self,
        repo: crate::repo::Repository,
        config: crate::config::Config,
        mut persisted: crate::auto_flow::PersistedAutoRun,
    ) {
        thread::spawn(move || {
            let worktree_path = persisted.run.worktree_path.clone();
            let Ok(harness_config) = config.harness_config(&persisted.run.harness_id) else {
                let error = format!(
                    "auto run harness '{}' is no longer configured",
                    persisted.run.harness_id
                );
                let _ = crate::observability::with_writable_db(&repo, |conn| {
                    crate::auto_flow::fail_auto_run(conn, &mut persisted, error)
                });
                return;
            };
            if harness_config.adapter != persisted.run.adapter_id {
                let error = format!(
                    "auto run harness '{}' was recorded with adapter '{}', but it is now configured as '{}'",
                    persisted.run.harness_id, persisted.run.adapter_id, harness_config.adapter
                );
                let _ = crate::observability::with_writable_db(&repo, |conn| {
                    crate::auto_flow::fail_auto_run(conn, &mut persisted, error)
                });
                return;
            }
            let server_url =
                crate::harness::Harness::new(&persisted.run.harness_id, &harness_config)
                    .prepare_server(&repo, &config, &persisted.run.branch, &worktree_path)
                    .ok()
                    .flatten()
                    .map(|runtime| runtime.server_url);
            let executor = AutoExecutorConfig::for_harness(
                persisted.run.harness_id.clone(),
                harness_config,
                server_url,
                worktree_path,
                format!("Auto Flow {}", persisted.run.prompt_summary),
            );
            let _ = crate::observability::with_writable_db(&repo, |conn| {
                execute_auto_initial_step(
                    conn,
                    &repo,
                    &config,
                    &mut persisted,
                    &executor,
                    &mut io::sink(),
                )
            });
        });
    }

    pub(crate) fn abort_selected_auto_run_or_step(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<bool, String> {
        let Some(dashboard) = self.current_auto_dashboard() else {
            return Ok(false);
        };
        let answer = self.prompt_choice_dialog(
            raw,
            crate::view::ChoiceList {
                title: "Abort Auto Flow".to_string(),
                choices: [("s", "selected step"), ("a", "whole run")]
                    .into_iter()
                    .map(|(key, label)| crate::view::KeyChoice {
                        key: key.to_string(),
                        label: label.to_string(),
                    })
                    .collect(),
            },
        )?;
        let Some(answer) = answer else {
            return Ok(true);
        };
        let repo = Repository {
            root: PathBuf::from(&dashboard.run.run.repo_root),
        };
        let run_id = dashboard.run.run.id.clone();
        let intent = if answer == "a" {
            AutoRunControlIntent::AbortRun
        } else {
            let step_run_id = dashboard
                .run
                .run
                .selected_step_run_id
                .or_else(|| dashboard.run.steps.first().and_then(|step| step.id))
                .ok_or_else(|| "auto flow run has no selected step".to_string())?;
            AutoRunControlIntent::AbortStep { step_run_id }
        };
        let outcome = crate::observability::with_writable_db(&repo, |conn| {
            apply_auto_run_control(conn, &run_id, intent)
        })?;
        self.remember_auto_run(outcome.run);
        if outcome.warnings.is_empty() {
            self.show_message("abort recorded for Auto Flow")?;
        } else {
            self.show_message(&format!(
                "abort recorded for Auto Flow with warnings: {}",
                outcome.warnings.join("; ")
            ))?;
        }
        Ok(true)
    }

    pub(crate) fn retry_failed_auto_step(&mut self) -> Result<bool, String> {
        let Some(dashboard) = self.current_auto_dashboard() else {
            return Ok(false);
        };
        let repo = Repository {
            root: PathBuf::from(&dashboard.run.run.repo_root),
        };
        let config = Config::load(&repo);
        let run_id = dashboard.run.run.id.clone();
        let outcome = crate::observability::with_writable_db(&repo, |conn| {
            apply_auto_run_control(conn, &run_id, AutoRunControlIntent::RetryFailed)
        })?;
        let persisted = outcome.run;
        self.remember_auto_run(persisted.clone());
        if outcome.executor == AutoExecutorDecision::Start {
            self.spawn_auto_run_executor(repo, config, persisted);
        }
        self.show_message("retrying Auto Flow step")?;
        Ok(true)
    }

    pub(crate) fn retry_auto_from_selected_step(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<bool, String> {
        let Some(dashboard) = self.current_auto_dashboard() else {
            return Ok(false);
        };
        let selected = dashboard
            .run
            .run
            .selected_step_run_id
            .or_else(|| dashboard.run.steps.first().and_then(|step| step.id));
        let Some(selected) = selected else {
            return Ok(true);
        };
        let should_retry =
            self.confirm_action_dialog(raw, "Retry Auto Flow", "Retry from selected step?", false)?;
        if !should_retry {
            return Ok(true);
        }
        let repo = Repository {
            root: PathBuf::from(&dashboard.run.run.repo_root),
        };
        let config = Config::load(&repo);
        let run_id = dashboard.run.run.id.clone();
        let outcome = crate::observability::with_writable_db(&repo, |conn| {
            apply_auto_run_control(
                conn,
                &run_id,
                AutoRunControlIntent::RetryFromStep {
                    step_run_id: selected,
                },
            )
        })?;
        let persisted = outcome.run;
        self.remember_auto_run(persisted.clone());
        if outcome.executor == AutoExecutorDecision::Start {
            self.spawn_auto_run_executor(repo, config, persisted);
        }
        self.show_message("retrying Auto Flow from selected step")?;
        Ok(true)
    }

    pub(crate) fn toggle_selected_auto_pause(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<bool, String> {
        let Some(dashboard) = self.current_auto_dashboard() else {
            return Ok(false);
        };
        let repo = Repository {
            root: PathBuf::from(&dashboard.run.run.repo_root),
        };
        let run_id = dashboard.run.run.id.clone();
        let resuming =
            dashboard.run.run.pause_requested || dashboard.run.run.status == AutoRunStatus::Paused;
        if resuming && !self.confirm_resume_auto_step(raw, &dashboard.run)? {
            self.show_message("Auto Flow resume cancelled")?;
            return Ok(true);
        }
        let intent = if resuming {
            AutoRunControlIntent::Resume
        } else {
            AutoRunControlIntent::Pause
        };
        let outcome = crate::observability::with_writable_db(&repo, |conn| {
            apply_auto_run_control(conn, &run_id, intent)
        })?;
        let executor = outcome.executor;
        let persisted = outcome.run;
        self.remember_auto_run(persisted.clone());
        if !resuming {
            self.show_message("Auto Flow will pause before the next step")?;
        } else if executor == AutoExecutorDecision::Start {
            self.spawn_auto_run_executor(repo.clone(), Config::load(&repo), persisted);
            self.show_message("resumed Auto Flow run")?;
        } else if executor == AutoExecutorDecision::AlreadyRunning {
            self.show_message("resumed Auto Flow run; work is already running")?;
        } else {
            self.show_message("Auto Flow has no queued agent step")?;
        }
        Ok(true)
    }

    pub(super) fn confirm_resume_auto_step(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
        run: &PersistedAutoRun,
    ) -> Result<bool, String> {
        let description = next_auto_step_description(run)
            .unwrap_or_else(|| "determine the next Auto Flow step".to_string());
        self.confirm_action_dialog(
            raw,
            "Resume Auto Flow",
            &format!("Next: {description}. Continue?"),
            true,
        )
    }

    pub(crate) fn dismiss_selected_auto_run(&mut self) -> Result<bool, String> {
        let Some(dashboard) = self.current_auto_dashboard() else {
            return Ok(false);
        };
        let repo = Repository {
            root: PathBuf::from(&dashboard.run.run.repo_root),
        };
        let run_id = dashboard.run.run.id.clone();
        crate::observability::with_writable_db(&repo, |conn| {
            let mut run = load_auto_run(conn, &run_id)?
                .ok_or_else(|| format!("auto flow run not found: {run_id}"))?;
            archive_auto_run(conn, &mut run)
        })?;
        self.auto_runs.remove(&run_id);
        self.active_auto_runs.retain(|_, active| active != &run_id);
        if self.selected_auto_run.as_deref() == Some(run_id.as_str()) {
            self.selected_auto_run = None;
        }
        self.selected_auto_step_by_run.remove(&run_id);
        self.auto_output_state_by_run.remove(&run_id);
        self.show_message("dismissed Auto Flow run")?;
        Ok(true)
    }
}
