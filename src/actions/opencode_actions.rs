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

pub(super) fn opencode_poll_key(
    repository: &crate::session::WorktreeRepositoryKey,
    session: &crate::session::Session,
    generation: u64,
) -> OpencodePollKey {
    OpencodePollKey::for_repository_session_generation(repository, session, generation)
}

fn session_uses_opencode(managed: &ManagedRepo, session: &crate::session::Session) -> bool {
    crate::session::worktree_harness(&managed.repo, session)
        .ok()
        .and_then(|association| managed.config.harness_adapter(&association.harness_id).ok())
        .is_some_and(|adapter| adapter == "opencode")
}

fn opencode_harness_id(managed: &ManagedRepo, session: &crate::session::Session) -> Option<String> {
    let association = crate::session::worktree_harness(&managed.repo, session).ok()?;
    (managed
        .config
        .harness_adapter(&association.harness_id)
        .ok()?
        .as_str()
        == "opencode")
        .then_some(association.harness_id)
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
            if !session_uses_opencode(managed, session) || !session.is_task_branch(&managed.config)
            {
                continue;
            }
            let generation = self
                .worktree_generations
                .get(&session.identity_key(&managed.identity))
                .copied()
                .unwrap_or_default();
            let key = opencode_poll_key(&managed.identity, session, generation);
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
            let Some(harness_id) = opencode_harness_id(managed, session) else {
                continue;
            };
            let branch = session.branch.clone();
            let path = session.path.clone();
            let tx = self.opencode_poll_tx.clone();
            self.opencode_polls_in_flight.insert(key.clone());
            self.opencode_last_polled.insert(key.clone(), now);
            std::thread::spawn(move || {
                let status = load_runtime(&repo, &harness_id, &branch, &path).and_then(|runtime| {
                    let Some(runtime) = runtime else {
                        return Err("no OpenCode runtime exists yet".to_string());
                    };
                    let runtime = opencode::refresh_opencode_session(&repo, runtime, &path)?;
                    opencode::poll_status(&runtime)
                });
                let _ = tx.send(OpencodePollResult {
                    key,
                    started_at: now,
                    status,
                });
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
            if !session_uses_opencode(managed, session) || !session.is_task_branch(&managed.config)
            {
                continue;
            }
            let Some(harness_id) = opencode_harness_id(managed, session) else {
                continue;
            };
            let Ok(Some(runtime)) =
                load_runtime(&managed.repo, &harness_id, &session.branch, &session.path)
            else {
                continue;
            };
            let Some(session_id) = runtime.opencode_session_id.clone() else {
                continue;
            };
            let key = session.identity_key(&managed.identity);
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
                        detail: current.as_ref().and_then(|status| status.detail.clone()),
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
                            key: key.clone(),
                            server_url: server_url.clone(),
                            event: Ok(event),
                        })
                        .map_err(|error| error.to_string())
                    });
                    if let Err(error) = result {
                        let _ = tx.send(OpencodeEventResult {
                            key: key.clone(),
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
                Ok(mut status) => {
                    if let Some(index) = self.sessions.iter().position(|session| {
                        self.repos.get(session.repo_index).is_some_and(|repo| {
                            let generation = self
                                .worktree_generations
                                .get(&session.identity_key(&repo.identity))
                                .copied()
                                .unwrap_or_default();
                            opencode_poll_key(&repo.identity, session, generation) == result.key
                        })
                    }) {
                        let state_event_is_newer = self
                            .opencode_last_state_event
                            .get(&result.key)
                            .is_some_and(|event_at| *event_at >= result.started_at);
                        let current = self.sessions[index].opencode_status.as_ref();
                        let preserve_active_from_idle = status.state
                            == opencode::OpencodeState::Idle
                            && (self.sessions[index].agent_state == AgentState::Running
                                || current.is_some_and(|current| {
                                    !matches!(
                                        current.state,
                                        opencode::OpencodeState::Unknown
                                            | opencode::OpencodeState::Idle
                                            | opencode::OpencodeState::Offline
                                    )
                                }));
                        if state_event_is_newer && let Some(current) = current {
                            status.state = current.state;
                        } else if preserve_active_from_idle {
                            // Idle sessions are omitted from /session/status. Preserve active
                            // work until message history reports a completed assistant turn.
                            status.state = current
                                .map(|current| current.state)
                                .filter(|state| {
                                    !matches!(
                                        state,
                                        opencode::OpencodeState::Unknown
                                            | opencode::OpencodeState::Idle
                                            | opencode::OpencodeState::Offline
                                    )
                                })
                                .unwrap_or(opencode::OpencodeState::Busy);
                        }
                        changed |= self.apply_opencode_status(index, status);
                    }
                }
                Err(error) => {
                    if error == "no OpenCode runtime exists yet" {
                        continue;
                    }
                    if let Some(repo) = self
                        .repos
                        .iter()
                        .find(|repo| repo.identity == result.key.repository)
                    {
                        let _ = append_runtime_message(
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
                        self.repos.get(session.repo_index).is_some_and(|managed| {
                            session.identity_key(&managed.identity) == result.key
                        }) && session
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
                        detail: None,
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
                        self.opencode_last_state_event.insert(
                            opencode_poll_key(
                                &self.repos[self.sessions[index].repo_index].identity,
                                &self.sessions[index],
                                self.worktree_generations
                                    .get(&self.sessions[index].identity_key(
                                        &self.repos[self.sessions[index].repo_index].identity,
                                    ))
                                    .copied()
                                    .unwrap_or_default(),
                            ),
                            std::time::Instant::now(),
                        );
                        if state != opencode::OpencodeState::Idle
                            || status.state != opencode::OpencodeState::Done
                        {
                            status.state = state;
                        }
                        if !matches!(
                            state,
                            opencode::OpencodeState::Busy | opencode::OpencodeState::Retry
                        ) {
                            status.active_tool = None;
                        }
                    }
                    if let Some(detail) = event.detail {
                        status.detail = Some(detail);
                    } else if event.state == Some(opencode::OpencodeState::Busy) {
                        status.detail = None;
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
                        (self.repos.get(session.repo_index).is_some_and(|managed| {
                            session.identity_key(&managed.identity) == result.key
                        }) && session
                            .opencode_status
                            .as_ref()
                            .and_then(|status| status.server_url.as_deref())
                            == Some(result.server_url.as_str()))
                        .then(|| self.repos.get(session.repo_index))
                        .flatten()
                    }) {
                        let _ = append_runtime_message(
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
            self.show_message("default branch does not have an agent session")?;
            return Ok(());
        }
        let association =
            crate::session::worktree_harness(&context.repo, &self.sessions[selected])?;
        let session_config = context.config.for_harness(&association.harness_id)?;
        if !session_config.selected_adapter_is("opencode") {
            self.show_message("selected harness does not support native session cancellation")?;
            return Ok(());
        }
        let should_abort = self.confirm_action_dialog(
            raw,
            "Abort Agent Session",
            &format!("Abort {}?", self.sessions[selected].branch),
            false,
        )?;
        if !should_abort {
            return Ok(());
        }
        let harness_config = session_config.harness_config(&association.harness_id)?;
        let runtime = crate::harness::Harness::new(&association.harness_id, &harness_config)
            .prepare_session(
                &context.repo,
                &session_config,
                &self.sessions[selected].branch,
                &self.sessions[selected].path,
            )?
            .ok_or_else(|| "selected harness has no native session protocol".to_string())?;
        let Some(session_id) = runtime.opencode_session_id.clone() else {
            return Err("OpenCode session ID is not available".to_string());
        };
        crate::harness::cancel_native_session(&crate::harness::SessionRef {
            adapter_id: Some("opencode".to_string()),
            endpoint: Some(runtime.server_url.clone()),
            id: Some(session_id.clone()),
        })?;
        self.sessions[selected].opencode_status = Some(OpencodeStatus {
            server_url: Some(runtime.server_url.clone()),
            session_id: Some(session_id.to_string()),
            title: self.sessions[selected]
                .opencode_status
                .as_ref()
                .and_then(|status| status.title.clone()),
            state: opencode::OpencodeState::Done,
            detail: Some("aborted".to_string()),
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
        self.show_message("agent session abort requested")?;
        Ok(())
    }

    pub(crate) fn shutdown_owned_opencode_servers(&mut self) {
        let mut seen = BTreeSet::new();
        for session in &self.sessions {
            let Some(managed) = self.repos.get(session.repo_index) else {
                continue;
            };
            if !managed.config.opencode_shutdown_owned_servers {
                continue;
            }
            let Some(harness_id) = opencode_harness_id(managed, session) else {
                continue;
            };
            let Ok(Some(runtime)) =
                load_runtime(&managed.repo, &harness_id, &session.branch, &session.path)
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
                let _ = append_runtime_message(
                    &managed.repo,
                    &format!("opencode server shutdown failed for pid {pid}: {error}"),
                );
            }
        }
    }
}
