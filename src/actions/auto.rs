use super::*;

pub(super) enum AutoStartupSource {
    Prompt,
    ExistingPlan,
    DraftPlan,
    ExistingPullRequest,
    Disable,
}

pub(super) fn auto_startup_choices(auto_active: bool) -> crate::view::ChoiceList {
    let start_choice = |key, label| {
        if auto_active {
            crate::view::KeyChoice::disabled(key, label)
        } else {
            crate::view::KeyChoice::new(key, label)
        }
    };
    crate::view::ChoiceList {
        title: "Auto Flow".to_string(),
        choices: vec![
            if auto_active {
                crate::view::KeyChoice::new("x", "disable Auto Flow")
            } else {
                crate::view::KeyChoice::disabled("x", "disable Auto Flow")
            },
            start_choice("r", "existing pull request"),
            start_choice("p", "prompt"),
            start_choice("e", "existing plan"),
            start_choice("d", "draft plan"),
        ],
    }
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
    pub(crate) fn toggle_selected_merge_intent(&mut self) -> Result<(), String> {
        let context = self
            .selected_worktree_context()
            .ok_or_else(|| "no worktree selected".to_string())?;
        let session_path = self.sessions[context.session_index].path.clone();
        if let Some(run_id) = self.active_auto_runs.get(&session_path).cloned() {
            let outcome = crate::observability::with_writable_db(&context.repo, |conn| {
                crate::integration::toggle_merge_intent(conn, &run_id, context.config.auto.merge)
            })?;
            self.load_auto_run_snapshot(&context.repo.root, &run_id);
            crate::worker::ensure_running()?;
            crate::worker::wake()?;
            return self.show_message(match outcome.state {
                crate::integration::MergeIntentState::Armed => {
                    "added pull request to the merge queue"
                }
                crate::integration::MergeIntentState::Withdrawn => {
                    "removed pull request from the merge queue"
                }
                _ => "updated pull request merge intent",
            });
        }

        let summary = self.sessions[context.session_index]
            .pr
            .trusted_summary()?
            .cloned()
            .ok_or_else(|| {
                "guarded merge requires a current Change Request observation".to_string()
            })?;
        if summary.merged || !summary.state.eq_ignore_ascii_case("OPEN") {
            return Err("guarded merge requires an open Change Request".to_string());
        }
        let artifact = crate::coding::cached_change_request_artifact(&summary)?;
        let run_id = crate::operations::WorkflowOperations::launch_named(
            &context.repo.root,
            "builtin:merge",
            vec![artifact],
            "local:tui".to_string(),
        )?;
        crate::worker::ensure_running()?;
        crate::worker::wake()?;
        self.invalidate_workflow_snapshots();
        self.show_message(&format!(
            "guarded merge Workflow {} is awaiting evidence and Approval",
            run_id.as_str()
        ))
    }

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
        let worktree_session_id = self.sessions[context.session_index]
            .worktree_session_id
            .clone();
        let active_run_id = self.active_auto_runs.get(&session_path).cloned();
        if let Some(run_id) = active_run_id.as_ref() {
            self.load_auto_run_snapshot(&context.repo.root, run_id);
            self.selected_auto_run = Some(run_id.clone());
        }
        let Some(source) = self.prompt_auto_implementation_source(raw, active_run_id.is_some())?
        else {
            return Ok(());
        };
        if matches!(source, AutoStartupSource::Disable) {
            let Some(run_id) = self.active_auto_runs.get(&session_path).cloned() else {
                self.show_message("Auto Flow is already disabled")?;
                return Ok(());
            };
            return self.disable_auto_run(&context.repo, &session_path, &run_id);
        }
        if let Some(run_id) = self.active_auto_runs.get(&session_path).cloned() {
            self.load_auto_run_snapshot(&context.repo.root, &run_id);
            self.selected_auto_run = Some(run_id);
            self.show_message("Auto Flow became active; press A again to disable it")?;
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
                let plan_path =
                    raw.suspend_for(|| select_plan_path(&session_path, &context.config))?;
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
            AutoStartupSource::ExistingPullRequest => (
                AutoRunMode::Standard,
                AutoImplementationSource::ExistingPullRequest,
                None,
                PlanRunMode::Sequential,
                "existing-pr".to_string(),
                format!("Stabilize existing pull request for branch {session_branch}"),
            ),
            AutoStartupSource::Disable => unreachable!("disable is handled before launch"),
        };
        let source = match implementation_source {
            AutoImplementationSource::Prompt => "prompt",
            AutoImplementationSource::ExistingPlan => "existing_plan",
            AutoImplementationSource::DraftPlan => "draft_plan",
            AutoImplementationSource::ExistingPullRequest => "existing_change_request",
        };
        let mut task = serde_json::json!({
            "implementation_source": source,
            "task": prompt,
            "branch": session_branch,
            "variant": variant,
            "plan_run_mode": format!("{plan_run_mode:?}").to_lowercase(),
            "worktree_session_id": worktree_session_id,
            "worktree_incarnation": session_incarnation,
            "mode": format!("{mode:?}").to_lowercase(),
        });
        if let Some(path) = plan_path {
            let content = std::fs::read_to_string(&path).map_err(|error| {
                format!("read immutable Plan input {}: {error}", path.display())
            })?;
            task["plan"] = serde_json::Value::String(content);
            task["plan_display"] = serde_json::Value::String(path.display().to_string());
        }
        let run_id = crate::operations::WorkflowOperations::launch_named(
            &context.repo.root,
            "builtin:coding",
            vec![crate::run::ArtifactInput {
                name: "task".to_string(),
                artifact_type: "builtin:task@1".to_string(),
                payload: task,
                trust: crate::run::TrustClass::Trusted,
                sensitivity: crate::run::Sensitivity::Internal,
            }],
            "local:tui".to_string(),
        )?;
        crate::worker::ensure_running()?;
        crate::worker::wake()?;
        self.invalidate_workflow_snapshots();
        self.show_message(&format!(
            "Workflow {} queued on headless worker",
            run_id.as_str()
        ))?;
        Ok(())
    }

    pub(super) fn prompt_auto_implementation_source(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
        auto_active: bool,
    ) -> Result<Option<AutoStartupSource>, String> {
        let answer = self.prompt_choice_dialog(raw, auto_startup_choices(auto_active))?;
        Ok(match answer.as_deref() {
            Some("p") => Some(AutoStartupSource::Prompt),
            Some("e") => Some(AutoStartupSource::ExistingPlan),
            Some("d") => Some(AutoStartupSource::DraftPlan),
            Some("r") => Some(AutoStartupSource::ExistingPullRequest),
            Some("x") => Some(AutoStartupSource::Disable),
            _ => None,
        })
    }

    fn disable_auto_run(
        &mut self,
        repo: &Repository,
        worktree_path: &Path,
        run_id: &str,
    ) -> Result<(), String> {
        let receipt = crate::workspace_state::control_repository_workflow(
            repo,
            crate::workspace_state::ControlAction::Stop,
            "auto",
            run_id,
        )?;
        self.invalidate_workflow_snapshots();
        self.load_auto_run_snapshot(&repo.root, run_id);
        self.active_auto_runs.remove(worktree_path);
        if self.selected_auto_run.as_deref() == Some(run_id) {
            self.selected_auto_run = None;
        }
        if receipt.warnings.is_empty() {
            self.show_message("disabled Auto Flow")?;
        } else {
            self.show_message(&format!(
                "Auto Flow stopped with cancellation warnings: {}",
                receipt.warnings.join("; ")
            ))?;
        }
        Ok(())
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
                    .map(|(key, label)| crate::view::KeyChoice::new(key, label))
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
        _config: crate::config::Config,
        persisted: crate::auto_flow::PersistedAutoRun,
    ) -> Result<(), String> {
        crate::observability::with_writable_db(&repo, |conn| {
            crate::execution::enqueue(
                conn,
                &crate::execution::WorkflowIdentity::new(
                    crate::execution::WorkflowKind::Auto,
                    &persisted.run.id,
                ),
            )
        })?;
        crate::worker::ensure_running()?;
        crate::worker::wake()
    }

    pub(crate) fn abort_selected_auto_run_or_step(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<bool, String> {
        let Some(dashboard) = self.current_auto_dashboard() else {
            return Ok(false);
        };
        let selected_step_run_id = dashboard
            .run
            .run
            .selected_step_run_id
            .or_else(|| dashboard.run.steps.first().and_then(|step| step.id));
        let selected_active = selected_step_run_id
            .and_then(|id| dashboard.run.steps.iter().find(|step| step.id == Some(id)))
            .is_some_and(|step| {
                matches!(
                    step.status,
                    AutoStepStatus::Queued
                        | AutoStepStatus::Starting
                        | AutoStepStatus::Running
                        | AutoStepStatus::Waiting
                )
            });
        let run_active = self
            .workflow_controls(
                Path::new(&dashboard.run.run.repo_root),
                crate::execution::WorkflowKind::Auto,
                &dashboard.run.run.id,
            )
            .is_some_and(|controls| controls.stop)
            && dashboard.run.steps.iter().any(|step| {
                matches!(
                    step.status,
                    AutoStepStatus::Queued
                        | AutoStepStatus::Starting
                        | AutoStepStatus::Running
                        | AutoStepStatus::Waiting
                )
            });
        let answer = self.prompt_choice_dialog(
            raw,
            crate::view::ChoiceList {
                title: "Abort Auto Flow".to_string(),
                choices: vec![
                    if selected_active {
                        crate::view::KeyChoice::new("s", "cancel selected action")
                    } else {
                        crate::view::KeyChoice::disabled("s", "cancel selected action")
                    },
                    if run_active {
                        crate::view::KeyChoice::new("a", "abort all pending actions")
                    } else {
                        crate::view::KeyChoice::disabled("a", "abort all pending actions")
                    },
                ],
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
        if intent == AutoRunControlIntent::AbortRun {
            let receipt = crate::workspace_state::control_repository_workflow(
                &repo,
                crate::workspace_state::ControlAction::Stop,
                "auto",
                &run_id,
            )?;
            self.load_auto_run_snapshot(&repo.root, &run_id);
            if receipt.warnings.is_empty() {
                self.show_message("abort recorded for Auto Flow")?;
            } else {
                self.show_message(&format!(
                    "abort recorded for Auto Flow with warnings: {}",
                    receipt.warnings.join("; ")
                ))?;
            }
            return Ok(true);
        }
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
        reject_claimed_control(
            &repo,
            crate::execution::WorkflowKind::Auto,
            &run_id,
            "retry",
        )?;
        let outcome = crate::observability::with_writable_db(&repo, |conn| {
            apply_auto_run_control(conn, &run_id, AutoRunControlIntent::RetryFailed)
        })?;
        let persisted = outcome.run;
        self.remember_auto_run(persisted.clone());
        if outcome.executor == AutoExecutorDecision::Start {
            self.spawn_auto_run_executor(repo, config, persisted)?;
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
        reject_claimed_control(
            &repo,
            crate::execution::WorkflowKind::Auto,
            &run_id,
            "retry",
        )?;
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
            self.spawn_auto_run_executor(repo, config, persisted)?;
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
        let controls = self
            .workflow_controls(&repo.root, crate::execution::WorkflowKind::Auto, &run_id)
            .cloned()
            .unwrap_or_default();
        let resuming = controls.resume;
        if !resuming && !controls.pause {
            return Err("pause/resume is not available for this Auto Flow run".to_string());
        }
        if resuming && !self.confirm_resume_auto_step(raw, &dashboard.run)? {
            self.show_message("Auto Flow resume cancelled")?;
            return Ok(true);
        }
        let action = if resuming {
            crate::workspace_state::ControlAction::Resume
        } else {
            crate::workspace_state::ControlAction::Pause
        };
        let receipt =
            crate::workspace_state::control_repository_workflow(&repo, action, "auto", &run_id)?;
        self.load_auto_run_snapshot(&repo.root, &run_id);
        if !resuming {
            self.show_message("Auto Flow will pause before the next step")?;
        } else {
            let suffix = if receipt.warnings.is_empty() {
                String::new()
            } else {
                format!("; warnings: {}", receipt.warnings.join("; "))
            };
            self.show_message(&format!("resumed Auto Flow run{suffix}"))?;
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
        let step_ids = dashboard
            .run
            .steps
            .iter()
            .filter_map(|step| step.id)
            .collect::<BTreeSet<_>>();
        let repository = crate::session::WorktreeRepositoryKey::new(repo.root.clone());
        crate::observability::with_writable_db(&repo, |conn| {
            let mut run = load_auto_run(conn, &run_id)?
                .ok_or_else(|| format!("auto flow run not found: {run_id}"))?;
            archive_auto_run(conn, &mut run)
        })?;
        self.invalidate_workflow_snapshots();
        self.auto_runs.remove(&run_id);
        self.active_auto_runs.retain(|_, active| active != &run_id);
        if self.selected_auto_run.as_deref() == Some(run_id.as_str()) {
            self.selected_auto_run = None;
        }
        self.selected_auto_step_by_run.remove(&run_id);
        self.auto_output_state_by_run.remove(&run_id);
        self.auto_output_cache
            .borrow_mut()
            .retain(|(cached_repository, step_run_id), _| {
                cached_repository != &repository || !step_ids.contains(step_run_id)
            });
        self.show_message("dismissed Auto Flow run")?;
        Ok(true)
    }
}
