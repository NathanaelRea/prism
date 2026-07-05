use super::*;

impl Tui {
    pub(crate) fn attach_selected_tmux_session(&mut self) -> Result<(), String> {
        let Some(context) = self.selected_worktree_context() else {
            return Ok(());
        };
        let session = self.sessions[context.session_index].background_job_snapshot();
        let use_ =
            crate::agent_session::session_use(&self.repos, &mut self.tmux_generations, &session);
        self.finish_tmux_warmup_for_key(&use_.warmup_key);
        let running = crate::agent_session::attach_session(
            &context.repo,
            &context.config,
            &session,
            use_.generation,
        )?;
        let outcome = crate::agent_session::apply_attach_result(
            &self.repos,
            &mut self.sessions,
            &mut self.tmux_generations,
            crate::agent_session::AgentSessionAttachCompletion {
                repo: &context.repo,
                config: &context.config,
                session_use: use_,
                branch: &session.branch,
                running,
            },
        );
        if let Some(warmup) = outcome.delayed_warmup {
            self.start_tmux_agent_warmup_for_key(warmup.key, warmup.delay);
        }
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
        let config = managed.config.clone();
        let session = self.sessions[session_index].background_job_snapshot();
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
        self.finish_tmux_warmup_for_key(&use_.warmup_key);
        let running =
            crate::agent_session::attach_window(&repo, &config, &session, use_.generation, window)?;
        crate::agent_session::apply_running_result(
            &self.repos,
            &mut self.sessions,
            &use_.slot,
            running,
        );
        self.start_opencode_status_poll(true);
        self.start_opencode_event_listeners();
        Ok(())
    }

    pub(crate) fn start_tmux_agent_warmup(&mut self) {
        self.poll_tmux_agent_warmup();
        let jobs = crate::agent_session::warmup_jobs_for_sessions(
            &self.repos,
            &self.sessions,
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
        if let Some(job) = crate::agent_session::warmup_job_for_key(
            &self.repos,
            &self.sessions,
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
        let tx = self.tmux_warmup_tx.clone();
        self.tmux_warmups_in_flight.insert(job.key.clone());
        std::thread::spawn(move || {
            if !job.delay.is_zero() {
                std::thread::sleep(job.delay);
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
            let _ = tx.send(AgentSessionWarmupResult {
                key: job.key,
                running,
                error,
            });
        });
    }

    pub(crate) fn poll_tmux_agent_warmup(&mut self) -> bool {
        let mut changed = false;
        while let Ok(result) = self.tmux_warmup_rx.try_recv() {
            changed |= self.apply_tmux_warmup_result(result);
        }
        changed
    }

    pub(super) fn finish_tmux_warmup_for_key(&mut self, key: &AgentSessionWarmupKey) -> bool {
        let mut changed = self.poll_tmux_agent_warmup();
        while self.tmux_warmups_in_flight.contains(key) {
            let Ok(result) = self.tmux_warmup_rx.recv() else {
                self.tmux_warmups_in_flight.remove(key);
                break;
            };
            changed |= self.apply_tmux_warmup_result(result);
        }
        changed
    }

    pub(super) fn apply_tmux_warmup_result(&mut self, result: AgentSessionWarmupResult) -> bool {
        self.tmux_warmups_in_flight.remove(&result.key);
        crate::agent_session::apply_warmup_result(
            &self.repos,
            &self.repo,
            &mut self.sessions,
            &self.tmux_generations,
            result,
        )
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
        let config = managed.config.clone();
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
            return Ok(());
        }

        self.finish_tmux_warmup_for_key(&use_.warmup_key);
        let running =
            crate::agent_session::submit_prompt(&repo, &config, &session, use_.generation, prompt)?;
        crate::agent_session::apply_running_result(
            &self.repos,
            &mut self.sessions,
            &use_.slot,
            running,
        );
        Ok(())
    }

    pub(super) fn submit_action_prompt_to_agent(
        &mut self,
        index: usize,
        repo: &crate::repo::Repository,
        summary: &str,
        prompt: &str,
    ) -> Result<(), String> {
        self.paste_prompt_into_tmux_agent(index, prompt, true)
            .map_err(|error| format!("send {summary} prompt to agent session: {error}"))?;
        write_task_summary_metadata(repo, &self.sessions[index], summary)?;
        self.sessions[index].mark_adopted_with_summary(summary);
        Ok(())
    }
}
