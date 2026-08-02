use super::*;
use std::time::Instant;

use crate::session::Session;

const TMUX_ATTACH_RESIZE_SETTLE: Duration = Duration::from_millis(100);

impl Tui {
    #[cfg(test)]
    pub(crate) fn attach_selected_tmux_session(&mut self) -> Result<(), String> {
        let Some(index) = self.selected_worktree_index() else {
            return Ok(());
        };
        self.attach_tmux_session_for_index(index)
    }

    pub(crate) fn prepare_tmux_session_for_attach(
        &mut self,
        session_index: usize,
        terminal_size: (u16, u16),
    ) -> Result<(), String> {
        let started = Instant::now();
        let Some(session) = self.sessions.get(session_index) else {
            return Ok(());
        };
        let Some(managed) = self.repos.get(session.repo_index) else {
            return Ok(());
        };
        let repo = managed.repo.clone();
        let session = self.sessions[session_index].background_job_snapshot();
        let association = crate::session::worktree_harness(&repo, &session)?;
        let config = managed.config.for_harness(&association.harness_id)?;
        let use_ =
            crate::agent_session::session_use(&self.repos, &mut self.tmux_generations, &session);
        let target_session = crate::tmux::TmuxAgentSession::for_worktree_session(
            &repo,
            &session.branch,
            use_.generation,
        )
        .name()
        .to_string();
        self.finish_tmux_warmup_for_key(&use_.warmup_key);
        let (width, height) = terminal_size;
        let result = crate::tmux::resize_agent_pane(
            &repo,
            &config,
            &session.branch,
            use_.generation,
            width.max(1),
            height.max(1),
        );
        if result.is_ok() {
            std::thread::sleep(TMUX_ATTACH_RESIZE_SETTLE);
        }
        crate::flight_recorder::record(
            "attach",
            "prepare",
            Some(started.elapsed()),
            vec![
                crate::flight_recorder::text("target_session", target_session),
                crate::flight_recorder::text("window", "agent"),
                crate::flight_recorder::unsigned("generation", use_.generation),
                crate::flight_recorder::boolean("success", result.is_ok()),
            ],
        );
        result
    }

    pub(crate) fn attach_tmux_session_for_index(
        &mut self,
        session_index: usize,
    ) -> Result<(), String> {
        let Some(session) = self.sessions.get(session_index) else {
            return Ok(());
        };
        let Some(managed) = self.repos.get(session.repo_index) else {
            return Ok(());
        };
        let repo = managed.repo.clone();
        let session = self.sessions[session_index].background_job_snapshot();
        let association = crate::session::worktree_harness(&repo, &session)?;
        let config = managed.config.for_harness(&association.harness_id)?;
        let use_ =
            crate::agent_session::session_use(&self.repos, &mut self.tmux_generations, &session);
        let target_session = crate::tmux::TmuxAgentSession::for_worktree_session(
            &repo,
            &session.branch,
            use_.generation,
        )
        .name()
        .to_string();
        self.finish_tmux_warmup_for_key(&use_.warmup_key);
        let attach_started = Instant::now();
        let attach_result =
            crate::agent_session::attach_session(&repo, &config, &session, use_.generation);
        crate::flight_recorder::record(
            "attach",
            "interactive",
            Some(attach_started.elapsed()),
            vec![
                crate::flight_recorder::text("target_session", target_session),
                crate::flight_recorder::text("window", "agent"),
                crate::flight_recorder::unsigned("generation", use_.generation),
                crate::flight_recorder::boolean("success", attach_result.is_ok()),
            ],
        );
        let running = attach_result?;
        let detach_started = Instant::now();
        self.resize_tmux_portal_after_detach(&repo, &config, &session, &use_);
        crate::flight_recorder::record(
            "attach",
            "detach_resize",
            Some(detach_started.elapsed()),
            vec![
                crate::flight_recorder::text("target", &session.branch),
                crate::flight_recorder::unsigned("generation", use_.generation),
            ],
        );
        let apply_started = Instant::now();
        let generation = use_.generation;
        let previous_agent_state = self.sessions[session_index].agent_state;
        let outcome = crate::agent_session::apply_attach_result(
            &self.repos,
            &mut self.sessions,
            &mut self.tmux_generations,
            crate::agent_session::AgentSessionAttachCompletion {
                repo: &repo,
                config: &config,
                session_use: use_,
                branch: &session.branch,
                running,
            },
        );
        self.accept_external_agent_state_change(session_index, previous_agent_state);
        if let Some(warmup) = outcome.delayed_warmup {
            self.start_tmux_agent_warmup_for_key(warmup.key, warmup.delay);
        }
        crate::flight_recorder::record(
            "attach",
            "apply_result",
            Some(apply_started.elapsed()),
            vec![
                crate::flight_recorder::text("target", &session.branch),
                crate::flight_recorder::unsigned("generation", generation),
                crate::flight_recorder::boolean("running", running),
            ],
        );
        Ok(())
    }

    pub(crate) fn attach_selected_tmux_window(&mut self, window: TmuxWindow) -> Result<(), String> {
        let Some(context) = self.selected_worktree_context() else {
            return Ok(());
        };
        self.attach_tmux_window_for_session_index(context.session_index, window, false)
    }

    pub(super) fn attach_tmux_window_for_session_index(
        &mut self,
        session_index: usize,
        window: TmuxWindow,
        force_new_generation: bool,
    ) -> Result<(), String> {
        let Some(session) = self.sessions.get(session_index) else {
            return Ok(());
        };
        let Some(managed) = self.repos.get(session.repo_index) else {
            return Ok(());
        };
        let repo = managed.repo.clone();
        let session = self.sessions[session_index].background_job_snapshot();
        let association = crate::session::worktree_harness(&repo, &session)?;
        let config = managed.config.for_harness(&association.harness_id)?;
        let mut use_ =
            crate::agent_session::session_use(&self.repos, &mut self.tmux_generations, &session);
        if force_new_generation {
            use_.generation = crate::agent_session::rotate_generation(
                &self.repos,
                &mut self.tmux_generations,
                use_.slot.clone(),
            );
            use_.warmup_key = crate::agent_session::AgentSessionWarmupKey::new(
                use_.slot.clone(),
                use_.generation,
            );
        }
        let target_session = crate::tmux::TmuxAgentSession::for_worktree_session(
            &repo,
            &session.branch,
            use_.generation,
        )
        .name()
        .to_string();
        self.finish_tmux_warmup_for_key(&use_.warmup_key);
        let attach_started = Instant::now();
        let attach_result =
            crate::agent_session::attach_window(&repo, &config, &session, use_.generation, window);
        crate::flight_recorder::record(
            "attach",
            "interactive",
            Some(attach_started.elapsed()),
            vec![
                crate::flight_recorder::text("target_session", target_session),
                crate::flight_recorder::text("window", window.label()),
                crate::flight_recorder::unsigned("generation", use_.generation),
                crate::flight_recorder::boolean("success", attach_result.is_ok()),
            ],
        );
        let running = attach_result?;
        let detach_started = Instant::now();
        self.resize_tmux_portal_after_detach(&repo, &config, &session, &use_);
        crate::flight_recorder::record(
            "attach",
            "detach_resize",
            Some(detach_started.elapsed()),
            vec![
                crate::flight_recorder::text("window", window.label()),
                crate::flight_recorder::unsigned("generation", use_.generation),
            ],
        );
        let apply_started = Instant::now();
        let previous_agent_state = self.sessions[session_index].agent_state;
        if crate::agent_session::apply_running_result(
            &self.repos,
            &mut self.sessions,
            &use_.slot,
            running,
        ) {
            self.accept_external_agent_state_change(session_index, previous_agent_state);
        }
        crate::flight_recorder::record(
            "attach",
            "apply_result",
            Some(apply_started.elapsed()),
            vec![
                crate::flight_recorder::text("window", window.label()),
                crate::flight_recorder::unsigned("generation", use_.generation),
                crate::flight_recorder::boolean("running", running),
            ],
        );
        self.start_opencode_status_poll(true);
        self.start_opencode_event_listeners();
        Ok(())
    }

    fn resize_tmux_portal_after_detach(
        &mut self,
        repo: &Repository,
        config: &Config,
        session: &Session,
        use_: &crate::agent_session::AgentSessionUse,
    ) {
        if self.focused_panel != crate::tui::PanelFocus::Worktrees {
            return;
        }
        let Some((width, height)) = self.tmux_portal_size else {
            return;
        };
        if crate::tmux::resize_agent_pane(
            repo,
            config,
            &session.branch,
            use_.generation,
            width,
            height,
        )
        .is_ok()
        {
            self.tmux_portal_resized = Some((use_.warmup_key.clone(), (width, height)));
            self.tmux_portal_last_polled
                .insert(use_.warmup_key.clone(), Instant::now());
        }
    }

    pub(crate) fn start_tmux_agent_warmup(&mut self) {
        self.poll_tmux_agent_warmup();
        self.refresh_worktree_harness_configs();
        let jobs = crate::agent_session::warmup_jobs_for_sessions(
            &self.repos,
            &self.sessions,
            &self.worktree_harness_configs,
            &mut self.tmux_generations,
            &self.tmux_warmups_in_flight,
        );
        for job in jobs {
            self.spawn_tmux_warmup_job(job);
        }
    }

    pub(super) fn start_tmux_agent_warmup_for_key(
        &mut self,
        key: AgentSessionWarmupKey,
        delay: Duration,
    ) {
        self.poll_tmux_agent_warmup();
        self.refresh_worktree_harness_configs();
        if let Some(job) = crate::agent_session::warmup_job_for_key(
            &self.repos,
            &self.sessions,
            &self.worktree_harness_configs,
            &self.tmux_generations,
            &self.tmux_warmups_in_flight,
            key,
            delay,
        ) {
            self.spawn_tmux_warmup_job(job);
        }
    }

    pub(super) fn spawn_tmux_warmup_job(
        &mut self,
        job: crate::agent_session::AgentSessionWarmupJob,
    ) {
        self.tmux_warmups_in_flight.insert(job.key.clone());
        let key = job.key.clone();
        self.spawn_tui_job(
            TuiJobKind::TmuxWarmup,
            TuiJobKey::Tmux(key.clone()),
            key.generation,
            Some(TUI_ACTION_JOB_TIMEOUT),
            format!("prism-tmux-warmup-{}", job.key.slot.worktree.branch),
            move |context| {
                if !job.delay.is_zero() && context.wait(job.delay) {
                    return Ok(None);
                }
                let result = crate::agent_session::ensure_session(
                    &job.repo,
                    &job.config,
                    &job.session,
                    job.key.generation,
                );
                let (running, error) = match result {
                    Ok(running) => (Some(running), None),
                    Err(error) => (None, Some(error)),
                };
                Ok(Some(TuiJobPayload::TmuxWarmup(AgentSessionWarmupResult {
                    key,
                    running,
                    error,
                })))
            },
        );
    }

    pub(crate) fn poll_tmux_agent_warmup(&mut self) -> bool {
        if !self.tui_tick_active && !self.routing_tui_jobs {
            self.route_tui_job_messages();
        }
        let mut changed = false;
        while let Ok(result) = self.tmux_warmup_rx.try_recv() {
            changed |= self.apply_tmux_warmup_result(result);
        }
        changed
    }

    pub(crate) fn finish_tmux_warmup_for_key(&mut self, key: &AgentSessionWarmupKey) -> bool {
        let mut changed = self.poll_tmux_agent_warmup();
        while self.tmux_warmups_in_flight.contains(key) {
            self.route_tui_job_messages();
            while let Ok(result) = self.tmux_warmup_rx.try_recv() {
                changed |= self.apply_tmux_warmup_result(result);
            }
            if self.tmux_warmups_in_flight.contains(key) {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        changed
    }

    pub(super) fn apply_tmux_warmup_result(&mut self, result: AgentSessionWarmupResult) -> bool {
        self.tmux_warmups_in_flight.remove(&result.key);
        let worktree = result.key.slot.worktree.clone();
        let previous_agent_state = self
            .sessions
            .iter()
            .find(|session| {
                self.repos
                    .get(session.repo_index)
                    .is_some_and(|repo| session.identity_key(&repo.identity) == worktree)
            })
            .map(|session| session.agent_state);
        let changed = crate::agent_session::apply_warmup_result(
            &self.repos,
            &self.repo,
            &mut self.sessions,
            &self.tmux_generations,
            result,
        );
        if changed
            && let Some(index) = self.sessions.iter().position(|session| {
                self.repos
                    .get(session.repo_index)
                    .is_some_and(|repo| session.identity_key(&repo.identity) == worktree)
            })
            && let Some(previous) = previous_agent_state
        {
            self.accept_external_agent_state_change(index, previous);
        }
        changed
    }

    pub(super) fn paste_prompt_into_tmux_agent(
        &mut self,
        index: usize,
        prompt: &str,
        force_new_generation: bool,
    ) -> Result<(), String> {
        let session = self
            .sessions
            .get(index)
            .map(crate::session::Session::background_job_snapshot)
            .ok_or_else(|| "no selected session".to_string())?;
        let managed = self
            .repos
            .get(session.repo_index)
            .ok_or_else(|| "selected session repository no longer exists".to_string())?;
        let repo = managed.repo.clone();
        let association = crate::session::worktree_harness(&repo, &session)?;
        let config = managed.config.for_harness(&association.harness_id)?;
        let mut use_ =
            crate::agent_session::session_use(&self.repos, &mut self.tmux_generations, &session);
        if force_new_generation {
            use_.generation = crate::agent_session::rotate_generation(
                &self.repos,
                &mut self.tmux_generations,
                use_.slot.clone(),
            );
            use_.warmup_key = crate::agent_session::AgentSessionWarmupKey::new(
                use_.slot.clone(),
                use_.generation,
            );
        }

        #[cfg(test)]
        if let Some(submissions) = &mut self.prompt_submissions {
            submissions.push((index, prompt.to_string(), use_.generation));
            self.mark_opencode_prompt_submitted(index, &config);
            return Ok(());
        }

        self.finish_tmux_warmup_for_key(&use_.warmup_key);
        let running =
            crate::agent_session::submit_prompt(&repo, &config, &session, use_.generation, prompt)?;
        let previous_agent_state = self.sessions[index].agent_state;
        if crate::agent_session::apply_running_result(
            &self.repos,
            &mut self.sessions,
            &use_.slot,
            running,
        ) {
            self.accept_external_agent_state_change(index, previous_agent_state);
        }
        self.mark_opencode_prompt_submitted(index, &config);
        Ok(())
    }

    fn mark_opencode_prompt_submitted(&mut self, index: usize, config: &crate::config::Config) {
        if !config.selected_adapter_is("opencode")
            || config.is_default_branch(&self.sessions[index].branch)
        {
            return;
        }
        if let Some(status) = self.sessions[index].opencode_status.as_mut() {
            status.state = crate::opencode::OpencodeState::Busy;
            status.detail = None;
            status.active_tool = None;
            status.last_updated_unix_ms = Some(super::opencode_actions::current_unix_ms());
        }
        self.apply_agent_state(index, AgentState::Running, true);
    }
}
