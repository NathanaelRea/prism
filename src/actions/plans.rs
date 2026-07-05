use super::*;

impl Tui {
    pub(crate) fn start_selected_worktree_plan_run(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let Some(context) = self.selected_worktree_context() else {
            return Ok(());
        };
        let path = self.sessions[context.session_index].path.clone();
        self.start_plan_run_for_scope(raw, context.repo, context.config, path)
    }

    pub(super) fn start_plan_run_for_scope(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
        repo: crate::repo::Repository,
        config: crate::config::Config,
        scope_path: PathBuf,
    ) -> Result<(), String> {
        raw.suspend()?;
        let execution = PlanExecution::prepare(&scope_path, &config, None);
        let resume_result = raw.resume();
        resume_result?;
        let execution = execution?;
        let mode =
            self.prompt_line_dialog(raw, "Plan Run", "Run phases in parallel? [y/N] ", "")?;
        let mode = if mode.as_deref().map(yes).unwrap_or(false) {
            PlanRunMode::Parallel
        } else {
            PlanRunMode::Sequential
        };
        let launch = execution.launch(&repo.root, mode)?;
        let mut should_execute = true;
        let persisted = crate::observability::with_writable_db(&repo, |conn| {
            if let Some(mut persisted) = load_resumable_plan_run(conn, &launch)? {
                should_execute = prepare_plan_run_for_resume(
                    conn,
                    &mut persisted,
                    DEFAULT_OUTPUT_LINES_PER_STEP,
                )?;
                Ok(persisted)
            } else {
                let persisted = launch.create_run();
                save_plan_run(conn, &persisted)?;
                Ok(persisted)
            }
        })?;
        let run_id = persisted.run.id.clone();
        let scope_path = execution.cwd().to_path_buf();
        self.plan_runs.insert(run_id.clone(), persisted.clone());
        self.active_plan_runs
            .insert(scope_path.clone(), run_id.clone());
        self.selected_plan_step_by_run
            .insert(run_id.clone(), persisted.run.selected_step);
        self.manual_plan_step_selection_by_run.remove(&run_id);

        if should_execute {
            self.spawn_plan_run_executor(repo, config, persisted);
        }
        if self.focused_panel == crate::tui::PanelFocus::Worktrees {
            self.worktree_main_view = crate::view::WorktreeMainView::Plan;
            self.main_focused = false;
        } else {
            self.focus_status();
        }
        if should_execute {
            self.show_message("started plan run")?;
        } else {
            self.show_message("plan run is already running")?;
        }
        Ok(())
    }

    pub(super) fn spawn_plan_run_executor(
        &self,
        repo: crate::repo::Repository,
        config: crate::config::Config,
        mut persisted: crate::plan_run::PersistedPlanRun,
    ) {
        let tx = self.plan_run_tx.clone();
        thread::spawn(move || {
            let run_id = persisted.run.id.clone();
            let mode = persisted.run.mode;
            let scope_path = persisted.run.scope_path.clone();
            let title_prefix = persisted.run.plan_display.clone();
            let server_url =
                crate::opencode::ensure_opencode_server(&repo, &config, "plan", &scope_path)
                    .ok()
                    .map(|runtime| runtime.server_url);
            let mut executor = PlanExecutorConfig::new(
                config.tool("opencode"),
                server_url,
                scope_path.clone(),
                title_prefix,
            );
            if config.opencode_plan_plugin
                && let Ok(plugin) = prepare_plan_plugin_config(&repo.prism_dir())
            {
                executor = executor.with_plugin_config(plugin);
            }
            let result = crate::observability::with_writable_db(&repo, |conn| match mode {
                PlanRunMode::Sequential => {
                    execute_plan_sequential(conn, &mut persisted, &executor, &mut io::sink())
                }
                PlanRunMode::Parallel => {
                    execute_plan_parallel(conn, &mut persisted, &executor, &mut io::sink())
                }
            });
            let _ = tx.send(PlanRunResult {
                repo_root: repo.root,
                run_id,
                result,
            });
        });
    }

    pub(crate) fn open_current_plan_tmux_session(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<bool, String> {
        let Some((repo, plan_run)) = self.current_tmux_plan_run() else {
            return Ok(false);
        };
        let Some(plan_step) = plan_run
            .steps
            .iter()
            .find(|step| {
                matches!(
                    step.status,
                    PlanStepStatus::Starting | PlanStepStatus::Running
                ) && step.opencode_session_id.is_some()
            })
            .or_else(|| {
                plan_run
                    .steps
                    .iter()
                    .find(|step| step.step == plan_run.run.selected_step)
            })
        else {
            self.show_message("selected plan run has no phase session yet")?;
            return Ok(false);
        };
        let Some(session_id) = plan_step.opencode_session_id.as_deref() else {
            self.show_message("selected plan phase has no OpenCode session yet")?;
            return Ok(false);
        };
        let Some(session_index) = self.sessions.iter().position(|session| {
            session.path == plan_run.run.scope_path
                && self
                    .repos
                    .get(session.repo_index)
                    .is_some_and(|managed| managed.repo.root == repo.root)
        }) else {
            self.show_message("plan run worktree is not visible")?;
            return Ok(false);
        };
        let Some(managed) = self.repos.get(self.sessions[session_index].repo_index) else {
            return Ok(true);
        };
        let config = managed.config.clone();
        let session = self.sessions[session_index].background_job_snapshot();
        let mut runtime = crate::opencode::ensure_opencode_server(
            &repo,
            &config,
            &session.branch,
            &session.path,
        )?;
        let changed_session = runtime.opencode_session_id.as_deref() != Some(session_id);
        if changed_session {
            runtime.opencode_session_id = Some(session_id.to_string());
            runtime.generation = runtime.generation.saturating_add(1);
            runtime.updated_unix_ms = crate::auto_flow::unix_ms();
            crate::opencode::save_runtime(&repo, &runtime)?;
        }
        raw.suspend()?;
        let result = self.attach_tmux_window_for_session_index(
            session_index,
            TmuxWindow::Agent,
            changed_session,
        );
        let resume_result = raw.resume();
        self.refresh_sessions()?;
        self.start_tmux_agent_warmup();
        resume_result?;
        result?;
        Ok(true)
    }

    pub(super) fn current_tmux_plan_run(
        &self,
    ) -> Option<(crate::repo::Repository, crate::plan_run::PersistedPlanRun)> {
        if let Some(dashboard) = self
            .current_auto_dashboard()
            .and_then(|dashboard| dashboard.linked_plan_dashboard)
        {
            return Some((
                Repository {
                    root: PathBuf::from(&dashboard.run.run.repo_root),
                },
                dashboard.run,
            ));
        }
        let dashboard = self.current_plan_dashboard()?;
        Some((
            Repository {
                root: PathBuf::from(&dashboard.run.run.repo_root),
            },
            dashboard.run,
        ))
    }

    pub(crate) fn show_plan_actions_dialog(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let Some(dashboard) = self.current_auto_dashboard() else {
            return self.show_standalone_plan_actions_dialog(raw);
        };
        if dashboard.run.run.implementation_source == AutoImplementationSource::Prompt {
            self.show_message("selected Auto Flow run is not using plan mode")?;
            return Ok(());
        }

        self.show_auto_plan_actions_dialog(raw)
    }

    pub(super) fn show_standalone_plan_actions_dialog(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        if self.current_plan_dashboard().is_none() {
            self.show_message("focus an Auto Flow or plan run to show plan actions")?;
            return Ok(());
        }

        let answer = self.prompt_choice_dialog(
            raw,
            Self::plan_action_choices("Plan Actions", "skip phase", true),
        )?;
        let Some(answer) = answer else {
            return Ok(());
        };
        match answer.trim().to_ascii_lowercase().as_str() {
            "" => Ok(()),
            "n" | "next" | "next run" => {
                if !self.move_plan_run_selection(1) {
                    self.show_message("no other plan run")?;
                }
                Ok(())
            }
            "v" | "prev" | "previous" | "previous run" => {
                if !self.move_plan_run_selection(-1) {
                    self.show_message("no other plan run")?;
                }
                Ok(())
            }
            "u" | "pause" | "resume" => {
                let _ = self.toggle_selected_plan_pause()?;
                Ok(())
            }
            "f" | "retry" | "retry failed" => {
                let _ = self.retry_failed_plan_steps()?;
                Ok(())
            }
            "b" | "from" | "retry from" => {
                let _ = self.retry_plan_from_selected_step(raw)?;
                Ok(())
            }
            "s" | "skip" => {
                let _ = self.skip_selected_plan_step()?;
                Ok(())
            }
            "x" | "abort" => {
                let _ = self.abort_selected_plan_run_or_step(raw)?;
                Ok(())
            }
            _ => {
                self.show_message("unknown Plan action")?;
                Ok(())
            }
        }
    }

    pub(super) fn show_auto_plan_actions_dialog(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let answer = self.prompt_choice_dialog(
            raw,
            Self::plan_action_choices("Auto Plan Actions", "skip linked phase", false),
        )?;
        let Some(answer) = answer else {
            return Ok(());
        };
        match answer.trim().to_ascii_lowercase().as_str() {
            "" => Ok(()),
            "u" | "pause" | "resume" => {
                let _ = self.toggle_selected_auto_pause(raw)?;
                Ok(())
            }
            "f" | "retry" | "retry failed" => {
                let _ = self.retry_failed_auto_step()?;
                Ok(())
            }
            "b" | "from" | "retry from" => {
                let _ = self.retry_auto_from_selected_step(raw)?;
                Ok(())
            }
            "s" | "skip" => {
                let _ = self.skip_selected_auto_plan_step()?;
                Ok(())
            }
            "x" | "abort" => {
                let _ = self.abort_selected_auto_run_or_step(raw)?;
                Ok(())
            }
            _ => {
                self.show_message("unknown Auto Plan action")?;
                Ok(())
            }
        }
    }

    pub(super) fn plan_action_choices(
        title: &str,
        skip_label: &str,
        include_run_navigation: bool,
    ) -> crate::view::ChoiceList {
        let mut choices = Vec::new();
        if include_run_navigation {
            choices.extend([("n", "next run"), ("v", "previous run")]);
        }
        choices.extend([
            ("u", "pause/resume"),
            ("f", "retry failed"),
            ("b", "retry from selected"),
            ("s", skip_label),
            ("x", "abort"),
        ]);
        crate::view::ChoiceList {
            title: title.to_string(),
            choices: choices
                .into_iter()
                .map(|(key, label)| crate::view::KeyChoice {
                    key: key.to_string(),
                    label: label.to_string(),
                })
                .collect(),
        }
    }

    pub(crate) fn abort_selected_plan_run_or_step(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<bool, String> {
        let Some(dashboard) = self.current_plan_dashboard() else {
            return Ok(false);
        };
        let repo = Repository {
            root: PathBuf::from(&dashboard.run.run.repo_root),
        };
        let run_id = dashboard.run.run.id.clone();
        let selected_step = dashboard.run.run.selected_step;
        let answer = self.prompt_line_dialog(
            raw,
            "Abort Plan",
            "Abort selected phase? Use 'all' for every running phase. [y/N/all] ",
            "",
        )?;
        let Some(answer) = answer else {
            return Ok(true);
        };
        if answer.trim().eq_ignore_ascii_case("all") {
            crate::observability::with_writable_db(&repo, |conn| {
                let mut run = load_plan_run(conn, &run_id)?
                    .ok_or_else(|| format!("plan run not found: {run_id}"))?;
                abort_plan_run(conn, &mut run)
            })?;
            self.load_plan_run_snapshot(&repo.root, &run_id);
            self.show_message("abort requested for plan run")?;
            return Ok(true);
        }
        if !yes(&answer) {
            return Ok(true);
        }
        crate::observability::with_writable_db(&repo, |conn| {
            let mut run = load_plan_run(conn, &run_id)?
                .ok_or_else(|| format!("plan run not found: {run_id}"))?;
            let step = run
                .steps
                .iter_mut()
                .find(|step| step.step == selected_step)
                .ok_or_else(|| format!("plan phase not found: {selected_step}"))?;
            if !matches!(
                step.status,
                PlanStepStatus::Starting | PlanStepStatus::Running
            ) {
                return Err(format!("plan phase {selected_step} is not running"));
            }
            abort_plan_step(conn, step)?;
            run.run.status = run.aggregate_status();
            save_plan_run(conn, &run)
        })?;
        self.load_plan_run_snapshot(&repo.root, &run_id);
        self.show_message("abort requested for selected plan phase")?;
        Ok(true)
    }

    pub(crate) fn retry_failed_plan_steps(&mut self) -> Result<bool, String> {
        let Some(dashboard) = self.current_plan_dashboard() else {
            return Ok(false);
        };
        let repo = Repository {
            root: PathBuf::from(&dashboard.run.run.repo_root),
        };
        let config = Config::load(&repo);
        let run_id = dashboard.run.run.id.clone();
        let persisted = crate::observability::with_writable_db(&repo, |conn| {
            let mut run = load_plan_run(conn, &run_id)?
                .ok_or_else(|| format!("plan run not found: {run_id}"))?;
            retry_failed_steps(conn, &mut run)?;
            Ok(run)
        })?;
        self.remember_plan_run(persisted.clone());
        self.spawn_plan_run_executor(repo, config, persisted);
        self.show_message("retrying failed plan phases")?;
        Ok(true)
    }

    pub(crate) fn retry_plan_from_selected_step(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<bool, String> {
        let Some(dashboard) = self.current_plan_dashboard() else {
            return Ok(false);
        };
        let selected_step = dashboard.run.run.selected_step;
        let answer = self.prompt_line_dialog(
            raw,
            "Retry Plan",
            &format!("Retry from phase {selected_step}? [y/N] "),
            "",
        )?;
        if !answer.as_deref().map(yes).unwrap_or(false) {
            return Ok(true);
        }
        let repo = Repository {
            root: PathBuf::from(&dashboard.run.run.repo_root),
        };
        let config = Config::load(&repo);
        let run_id = dashboard.run.run.id.clone();
        let persisted = crate::observability::with_writable_db(&repo, |conn| {
            let mut run = load_plan_run(conn, &run_id)?
                .ok_or_else(|| format!("plan run not found: {run_id}"))?;
            retry_from_step(conn, &mut run, selected_step)?;
            Ok(run)
        })?;
        self.remember_plan_run(persisted.clone());
        self.spawn_plan_run_executor(repo, config, persisted);
        self.show_message("retrying plan from selected phase")?;
        Ok(true)
    }

    pub(crate) fn skip_selected_plan_step(&mut self) -> Result<bool, String> {
        let Some(dashboard) = self.current_plan_dashboard() else {
            return Ok(false);
        };
        let repo = Repository {
            root: PathBuf::from(&dashboard.run.run.repo_root),
        };
        let run_id = dashboard.run.run.id.clone();
        let selected_step = dashboard.run.run.selected_step;
        crate::observability::with_writable_db(&repo, |conn| {
            let mut run = load_plan_run(conn, &run_id)?
                .ok_or_else(|| format!("plan run not found: {run_id}"))?;
            skip_plan_step(conn, &mut run, selected_step)
        })?;
        self.load_plan_run_snapshot(&repo.root, &run_id);
        self.show_message("skipped selected plan phase")?;
        Ok(true)
    }

    pub(crate) fn skip_selected_auto_plan_step(&mut self) -> Result<bool, String> {
        let Some(dashboard) = self.current_auto_dashboard() else {
            return Ok(false);
        };
        let Some(plan_dashboard) = dashboard.linked_plan_dashboard else {
            return Ok(false);
        };
        let repo = Repository {
            root: PathBuf::from(&dashboard.run.run.repo_root),
        };
        let config = Config::load(&repo);
        let auto_run_id = dashboard.run.run.id.clone();
        let plan_run_id = plan_dashboard.run.run.id.clone();
        let selected_step = plan_dashboard.run.run.selected_step;
        let mut should_execute = false;
        let (auto_run, plan_run) = crate::observability::with_writable_db(&repo, |conn| {
            let mut plan_run = load_plan_run(conn, &plan_run_id)?
                .ok_or_else(|| format!("plan run not found: {plan_run_id}"))?;
            skip_plan_step(conn, &mut plan_run, selected_step)?;
            let mut auto_run = load_auto_run(conn, &auto_run_id)?
                .ok_or_else(|| format!("auto flow run not found: {auto_run_id}"))?;
            should_execute =
                prepare_auto_run_for_resume(conn, &mut auto_run, DEFAULT_OUTPUT_LINES_PER_STEP)?;
            Ok((auto_run, plan_run))
        })?;
        self.remember_plan_run(plan_run);
        self.remember_auto_run(auto_run.clone());
        if should_execute {
            self.spawn_auto_run_executor(repo, config, auto_run);
            self.show_message("skipped linked plan phase; continuing Auto Flow")?;
        } else {
            self.show_message("skipped linked plan phase")?;
        }
        Ok(true)
    }

    pub(crate) fn toggle_selected_plan_pause(&mut self) -> Result<bool, String> {
        let Some(dashboard) = self.current_plan_dashboard() else {
            return Ok(false);
        };
        let repo = Repository {
            root: PathBuf::from(&dashboard.run.run.repo_root),
        };
        let config = Config::load(&repo);
        let run_id = dashboard.run.run.id.clone();
        let mut should_execute = false;
        let persisted = crate::observability::with_writable_db(&repo, |conn| {
            let mut run = load_plan_run(conn, &run_id)?
                .ok_or_else(|| format!("plan run not found: {run_id}"))?;
            if run.run.pause_requested || run.run.status == PlanRunStatus::Paused {
                resume_paused_plan_run(conn, &mut run)?;
                should_execute =
                    prepare_plan_run_for_resume(conn, &mut run, DEFAULT_OUTPUT_LINES_PER_STEP)?;
            } else {
                request_plan_run_pause(conn, &mut run)?;
            }
            Ok(run)
        })?;
        self.remember_plan_run(persisted.clone());
        if persisted.run.pause_requested || persisted.run.status == PlanRunStatus::Paused {
            self.show_message("plan run will pause before the next phase")?;
            return Ok(true);
        }
        if should_execute {
            self.spawn_plan_run_executor(repo, config, persisted);
            self.show_message("resumed plan run")?;
        } else {
            self.show_message("plan run is already running")?;
        }
        Ok(true)
    }

    pub(crate) fn dismiss_selected_plan_run(&mut self) -> Result<bool, String> {
        let Some(dashboard) = self.current_plan_dashboard() else {
            return Ok(false);
        };
        let repo = Repository {
            root: PathBuf::from(&dashboard.run.run.repo_root),
        };
        let run_id = dashboard.run.run.id.clone();
        crate::observability::with_writable_db(&repo, |conn| {
            let mut run = load_plan_run(conn, &run_id)?
                .ok_or_else(|| format!("plan run not found: {run_id}"))?;
            archive_plan_run(conn, &mut run)
        })?;
        self.plan_runs.remove(&run_id);
        self.active_plan_runs.retain(|_, active| active != &run_id);
        self.selected_plan_step_by_run.remove(&run_id);
        self.manual_plan_step_selection_by_run.remove(&run_id);
        self.plan_output_state_by_run.remove(&run_id);
        if self.focused_panel == crate::tui::PanelFocus::Worktrees
            && self.current_plan_dashboard().is_none()
        {
            self.worktree_main_view = crate::view::WorktreeMainView::Details;
        }
        self.show_message("dismissed plan run")?;
        Ok(true)
    }
}
