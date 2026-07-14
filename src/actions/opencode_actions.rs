use super::*;

pub(super) const SELECTED_OPENCODE_STATUS_POLL_INTERVAL: Duration = Duration::from_secs(2);
pub(super) const VISIBLE_OPENCODE_STATUS_POLL_INTERVAL: Duration = Duration::from_secs(5);
pub(super) const OPENCODE_SSE_RECONNECT_INITIAL: Duration = Duration::from_millis(500);
pub(super) const OPENCODE_SSE_RECONNECT_MAX: Duration = Duration::from_secs(10);

pub(super) fn current_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

pub(super) fn opencode_poll_key(session: &crate::session::Session) -> OpencodePollKey {
    OpencodePollKey::for_session(session)
}

impl Tui {
    pub(crate) fn start_opencode_status_poll(&mut self, force: bool) {
        let _ = self.poll_opencode_status();
        let selected = self.selected_worktree_index();
        let visible = self.visible_session_indices();
        let now = std::time::Instant::now();
        for session_index in visible {
            let Some(session) = self.sessions.get(session_index) else {
                continue;
            };
            let Some(managed) = self.repos.get(session.repo_index) else {
                continue;
            };
            if managed.config.default_agent != "opencode"
                || !session.is_task_branch(&managed.config)
            {
                continue;
            }
            let key = opencode_poll_key(session);
            if !force && self.opencode_polls_in_flight.contains(&key) {
                continue;
            }
            let interval = if Some(session_index) == selected {
                SELECTED_OPENCODE_STATUS_POLL_INTERVAL
            } else {
                VISIBLE_OPENCODE_STATUS_POLL_INTERVAL
            };
            let due = self
                .opencode_last_polled
                .get(&key)
                .map(|last| now.duration_since(*last) >= interval)
                .unwrap_or(true);
            if !force && !due {
                continue;
            }
            let repo = managed.repo.clone();
            let branch = session.branch.clone();
            let path = session.path.clone();
            let tx = self.opencode_poll_tx.clone();
            self.opencode_polls_in_flight.insert(key.clone());
            self.opencode_last_polled.insert(key.clone(), now);
            std::thread::spawn(move || {
                let status = load_runtime(&repo, &branch, &path).and_then(|runtime| {
                    let Some(runtime) = runtime else {
                        return Err("no OpenCode runtime exists yet".to_string());
                    };
                    let runtime = opencode::refresh_opencode_session(&repo, runtime, &path)?;
                    opencode::poll_status(&runtime)
                });
                let _ = tx.send(OpencodePollResult { key, status });
            });
        }
    }

    pub(crate) fn start_opencode_event_listeners(&mut self) {
        for session_index in self.visible_session_indices() {
            let Some(session) = self.sessions.get(session_index) else {
                continue;
            };
            let Some(managed) = self.repos.get(session.repo_index) else {
                continue;
            };
            if managed.config.default_agent != "opencode"
                || !session.is_task_branch(&managed.config)
            {
                continue;
            }
            let Ok(Some(runtime)) = load_runtime(&managed.repo, &session.branch, &session.path)
            else {
                continue;
            };
            let Some(session_id) = runtime.opencode_session_id.clone() else {
                continue;
            };
            if let Some(session) = self.sessions.get_mut(session_index) {
                let current = session.opencode_status.clone();
                if current
                    .as_ref()
                    .and_then(|status| status.server_url.as_deref())
                    != Some(runtime.server_url.as_str())
                    || current
                        .as_ref()
                        .and_then(|status| status.session_id.as_deref())
                        != Some(session_id.as_str())
                {
                    session.opencode_status = Some(OpencodeStatus {
                        server_url: Some(runtime.server_url.clone()),
                        session_id: Some(session_id.clone()),
                        title: current.as_ref().and_then(|status| status.title.clone()),
                        state: opencode::OpencodeState::Unknown,
                        latest_message: current
                            .as_ref()
                            .and_then(|status| status.latest_message.clone()),
                        latest_user_message: current
                            .as_ref()
                            .and_then(|status| status.latest_user_message.clone()),
                        recent_messages: current
                            .as_ref()
                            .map(|status| status.recent_messages.clone())
                            .unwrap_or_default(),
                        active_tool: current
                            .as_ref()
                            .and_then(|status| status.active_tool.clone()),
                        todos: current
                            .as_ref()
                            .map(|status| status.todos.clone())
                            .unwrap_or_default(),
                        last_updated_unix_ms: None,
                    });
                }
            }
            if !self.opencode_sse_servers.insert(runtime.server_url.clone()) {
                continue;
            }
            let server_url = runtime.server_url;
            let tx = self.opencode_event_tx.clone();
            std::thread::spawn(move || {
                let mut backoff = OPENCODE_SSE_RECONNECT_INITIAL;
                loop {
                    let result = opencode::listen_events(&server_url, |event| {
                        tx.send(OpencodeEventResult {
                            server_url: server_url.clone(),
                            event: Ok(event),
                        })
                        .map_err(|error| error.to_string())
                    });
                    if let Err(error) = result {
                        let _ = tx.send(OpencodeEventResult {
                            server_url: server_url.clone(),
                            event: Err(error),
                        });
                    }
                    std::thread::sleep(backoff);
                    backoff = (backoff * 2).min(OPENCODE_SSE_RECONNECT_MAX);
                }
            });
        }
    }

    pub(crate) fn poll_opencode_status(&mut self) -> bool {
        let mut changed = false;
        while let Ok(result) = self.opencode_poll_rx.try_recv() {
            self.opencode_polls_in_flight.remove(&result.key);
            match result.status {
                Ok(status) => {
                    if let Some(index) = self
                        .sessions
                        .iter()
                        .position(|session| opencode_poll_key(session) == result.key)
                    {
                        changed |= self.apply_opencode_status(index, status);
                    }
                }
                Err(error) => {
                    if error == "no OpenCode runtime exists yet" {
                        continue;
                    }
                    if let Some(repo) = self.repos.get(result.key.repo_index) {
                        let _ = append_runtime_log(
                            &repo.repo,
                            &format!(
                                "opencode status refresh failed for {}: {error}",
                                result.key.branch
                            ),
                        );
                    }
                }
            }
        }
        changed
    }

    pub(crate) fn poll_opencode_events(&mut self) -> bool {
        let mut changed = false;
        while let Ok(result) = self.opencode_event_rx.try_recv() {
            match result.event {
                Ok(event) => {
                    let Some(session_id) = event.session_id.as_deref() else {
                        continue;
                    };
                    let Some(index) = self.sessions.iter().position(|session| {
                        session
                            .opencode_status
                            .as_ref()
                            .and_then(|status| status.server_url.as_deref())
                            == Some(result.server_url.as_str())
                            && session
                                .opencode_status
                                .as_ref()
                                .and_then(|status| status.session_id.as_deref())
                                == Some(session_id)
                    }) else {
                        continue;
                    };
                    let current = self.sessions[index].opencode_status.clone();
                    let mut status = current.unwrap_or_else(|| OpencodeStatus {
                        server_url: Some(result.server_url.clone()),
                        session_id: Some(session_id.to_string()),
                        title: None,
                        state: opencode::OpencodeState::Unknown,
                        latest_message: None,
                        latest_user_message: None,
                        recent_messages: Vec::new(),
                        active_tool: None,
                        todos: Vec::new(),
                        last_updated_unix_ms: None,
                    });
                    status.server_url = Some(result.server_url.clone());
                    status.session_id = Some(session_id.to_string());
                    if let Some(title) = event.title {
                        status.title = Some(title);
                    }
                    if let Some(state) = event.state {
                        status.state = state;
                        if !matches!(
                            state,
                            opencode::OpencodeState::Busy | opencode::OpencodeState::Retry
                        ) {
                            status.active_tool = None;
                        }
                    }
                    if let Some(message) = event.latest_message {
                        status.latest_message = Some(message.clone());
                        if status.recent_messages.first() != Some(&message) {
                            status.recent_messages.insert(0, message);
                            status.recent_messages.truncate(5);
                        }
                    }
                    if let Some(tool) = event.active_tool {
                        status.active_tool = Some(tool);
                    }
                    if let Some(todos) = event.todos {
                        status.todos = todos;
                    }
                    status.last_updated_unix_ms = Some(current_unix_ms());
                    changed |= self.apply_opencode_status(index, status);
                }
                Err(error) => {
                    if let Some(repo) = self.sessions.iter().find_map(|session| {
                        (session
                            .opencode_status
                            .as_ref()
                            .and_then(|status| status.server_url.as_deref())
                            == Some(result.server_url.as_str()))
                        .then(|| self.repos.get(session.repo_index))
                        .flatten()
                    }) {
                        let _ = append_runtime_log(
                            &repo.repo,
                            &format!(
                                "opencode event stream disconnected for {}: {error}",
                                result.server_url
                            ),
                        );
                    }
                }
            }
        }
        changed
    }

    pub(super) fn apply_opencode_status(&mut self, index: usize, status: OpencodeStatus) -> bool {
        let Some(session) = self.sessions.get_mut(index) else {
            return false;
        };
        let mut changed = false;
        let agent_state = status.state.agent_state();
        if session.opencode_status.as_ref() != Some(&status) {
            session.opencode_status = Some(status);
            changed = true;
        }
        if session.agent_state != agent_state {
            session.agent_state = agent_state;
            if let Some(repo) = self.repos.get(session.repo_index) {
                let _ = save_agent_state(&repo.repo, &session.branch, agent_state);
            }
            changed = true;
        }
        changed
    }

    pub(crate) fn abort_selected_opencode_session(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let Some(context) = self.selected_worktree_context() else {
            return Ok(());
        };
        let selected = context.session_index;
        if self.sessions[selected].is_default_branch(&context.config) {
            self.show_message("default branch does not have an OpenCode session")?;
            return Ok(());
        }
        if context.config.default_agent != "opencode" {
            self.show_message("selected worktree is not using OpenCode")?;
            return Ok(());
        }
        let should_abort = self.confirm_action_dialog(
            raw,
            "Abort OpenCode",
            &format!("Abort {}?", self.sessions[selected].branch),
            "Abort",
            false,
        )?;
        if !should_abort {
            return Ok(());
        }
        let runtime = opencode::ensure_opencode_session(
            &context.repo,
            &context.config,
            &self.sessions[selected].branch,
            &self.sessions[selected].path,
        )?;
        let Some(session_id) = runtime.opencode_session_id.clone() else {
            return Err("OpenCode session ID is not available".to_string());
        };
        opencode::abort_session(&runtime.server_url, &session_id)?;
        self.sessions[selected].opencode_status = Some(OpencodeStatus {
            server_url: Some(runtime.server_url.clone()),
            session_id: Some(session_id.to_string()),
            title: self.sessions[selected]
                .opencode_status
                .as_ref()
                .and_then(|status| status.title.clone()),
            state: opencode::OpencodeState::Idle,
            latest_message: self.sessions[selected]
                .opencode_status
                .as_ref()
                .and_then(|status| status.latest_message.clone()),
            latest_user_message: self.sessions[selected]
                .opencode_status
                .as_ref()
                .and_then(|status| status.latest_user_message.clone()),
            recent_messages: self.sessions[selected]
                .opencode_status
                .as_ref()
                .map(|status| status.recent_messages.clone())
                .unwrap_or_default(),
            active_tool: None,
            todos: self.sessions[selected]
                .opencode_status
                .as_ref()
                .map(|status| status.todos.clone())
                .unwrap_or_default(),
            last_updated_unix_ms: Some(current_unix_ms()),
        });
        self.sessions[selected].agent_state = AgentState::NeedsInput;
        let _ = save_agent_state(
            &context.repo,
            &self.sessions[selected].branch,
            AgentState::NeedsInput,
        );
        self.start_opencode_status_poll(true);
        self.show_message("abort requested for OpenCode session")?;
        Ok(())
    }

    pub(crate) fn shutdown_owned_opencode_servers(&mut self) {
        let mut seen = BTreeSet::new();
        for session in &self.sessions {
            let Some(managed) = self.repos.get(session.repo_index) else {
                continue;
            };
            if !managed.config.opencode_shutdown_owned_servers
                || managed.config.default_agent != "opencode"
            {
                continue;
            }
            let Ok(Some(runtime)) = load_runtime(&managed.repo, &session.branch, &session.path)
            else {
                continue;
            };
            let Some(pid) = runtime.server_pid else {
                continue;
            };
            if !seen.insert(pid) {
                continue;
            }
            if let Err(error) = opencode::shutdown_owned_server(&runtime) {
                let _ = append_runtime_log(
                    &managed.repo,
                    &format!("opencode server shutdown failed for pid {pid}: {error}"),
                );
            }
        }
    }
}
