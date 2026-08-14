use super::*;
use std::time::Instant;

struct OpencodeListenerTarget {
    session_index: usize,
    stream: OpencodeListenerKey,
}

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
            let session_key = session.identity_key(&managed.identity);
            let Some(harness_config) = self.worktree_harness_configs.get(&session_key) else {
                continue;
            };
            if !harness_config.selected_adapter_is("opencode")
                || !session.is_task_branch(&managed.config)
            {
                continue;
            }
            let generation = self
                .worktree_generations
                .get(&session.identity_key(&managed.identity))
                .copied()
                .unwrap_or_default();
            let key = opencode_poll_key(&managed.identity, session, generation);
            if self.opencode_polls_in_flight.contains(&key) {
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
            let reconciliation_due = self.opencode_reconcile_requested.contains_key(&session_key);
            if !force && !reconciliation_due && !due {
                continue;
            }
            let repo = managed.repo.clone();
            let harness_id = harness_config.default_harness.clone();
            let branch = session.branch.clone();
            let path = session.path.clone();
            self.opencode_polls_in_flight.insert(key.clone());
            self.opencode_last_polled.insert(key.clone(), now);
            let job_key = key.clone();
            self.spawn_tui_job(
                TuiJobKind::OpencodePoll,
                TuiJobKey::Opencode(key.clone()),
                generation,
                Some(TUI_ACTION_JOB_TIMEOUT),
                format!("prism-opencode-poll-{}", session_index),
                move |_| async move {
                    let status =
                        load_runtime(&repo, &harness_id, &branch, &path).and_then(|runtime| {
                            let Some(runtime) = runtime else {
                                return Err("no OpenCode runtime exists yet".to_string());
                            };
                            let runtime =
                                opencode::refresh_opencode_session(&repo, runtime, &path)?;
                            opencode::poll_status(&runtime)
                        });
                    Ok(Some(TuiJobPayload::OpencodePoll(OpencodePollResult {
                        key: job_key,
                        started_at: now,
                        status,
                    })))
                },
            );
        }
    }

    pub(crate) fn start_opencode_event_listeners(&mut self) {
        if !self.tui_tick_active && !self.routing_tui_jobs {
            self.route_tui_job_messages();
        }
        let now = Instant::now();
        if self
            .opencode_listener_last_scanned
            .is_some_and(|last| now.duration_since(last) < Duration::from_secs(1))
        {
            return;
        }
        self.opencode_listener_last_scanned = Some(now);
        let mut targets = Vec::new();
        for session_index in self.visible_session_indices() {
            let Some(session) = self.sessions.get(session_index) else {
                continue;
            };
            let Some(managed) = self.repos.get(session.repo_index) else {
                continue;
            };
            let key = session.identity_key(&managed.identity);
            let Some(harness_config) = self.worktree_harness_configs.get(&key) else {
                continue;
            };
            if !harness_config.selected_adapter_is("opencode")
                || !session.is_task_branch(&managed.config)
            {
                continue;
            }
            let Some(status) = session.opencode_status.as_ref() else {
                continue;
            };
            let Some(session_id) = status.session_id.clone() else {
                continue;
            };
            let Some(server_url) = status.server_url.clone() else {
                continue;
            };
            let generation = self
                .worktree_generations
                .get(&key)
                .copied()
                .unwrap_or_default();
            targets.push(OpencodeListenerTarget {
                session_index,
                stream: OpencodeListenerKey {
                    worktree: key,
                    generation,
                    session_id,
                    server_url,
                },
            });
        }

        let desired = targets
            .iter()
            .map(|target| target.stream.clone())
            .collect::<BTreeSet<_>>();
        let to_start = self.reconcile_opencode_listener_jobs(&desired);
        for target in targets {
            if !to_start.contains(&target.stream) {
                continue;
            }
            self.opencode_listeners.insert(target.stream.clone());
            let stream = target.stream;
            let listener_url = stream.server_url.clone();
            let listener_directory = stream.worktree.path.clone();
            let job_stream = stream.clone();
            self.spawn_tui_job(
                TuiJobKind::OpencodeListener,
                TuiJobKey::OpencodeListener(stream.clone()),
                stream.generation,
                None,
                format!("prism-opencode-sse-{}", target.session_index),
                move |context| async move {
                    let mut backoff = OPENCODE_SSE_RECONNECT_INITIAL;
                    loop {
                        if context.is_canceled() {
                            return Ok(None);
                        }
                        let cancellation_context = context.clone();
                        let event_context = context.clone();
                        let event_stream = job_stream.clone();
                        let result = opencode::listen_classified_events_until_async(
                            listener_url.clone(),
                            listener_directory.clone(),
                            move || cancellation_context.is_canceled(),
                            move |event, snapshot_facet| {
                                let facet = match snapshot_facet {
                                    Some(opencode::OpencodeSnapshotFacet::Status) => {
                                        Some(CoalescedFacet::Status)
                                    }
                                    Some(opencode::OpencodeSnapshotFacet::Message) => {
                                        Some(CoalescedFacet::Message)
                                    }
                                    None => None,
                                };
                                let payload = TuiJobPayload::OpencodeEvent(OpencodeEventResult {
                                    stream: event_stream.clone(),
                                    received_at: Instant::now(),
                                    event: Ok(event),
                                });
                                if let Some(facet) = facet {
                                    event_context.send_coalesced(facet, payload)
                                } else {
                                    event_context.send(payload)
                                }
                            },
                        )
                        .await;
                        if context.is_canceled() {
                            return Ok(None);
                        }
                        if let Err(error) = result
                            && context
                                .send(TuiJobPayload::OpencodeEvent(OpencodeEventResult {
                                    stream: job_stream.clone(),
                                    received_at: Instant::now(),
                                    event: Err(error),
                                }))
                                .is_err()
                        {
                            return Ok(None);
                        }
                        if context.wait(backoff).await {
                            return Ok(None);
                        }
                        backoff = (backoff * 2).min(OPENCODE_SSE_RECONNECT_MAX);
                    }
                },
            );
        }
    }

    pub(crate) fn reconcile_opencode_listener_jobs(
        &mut self,
        desired: &BTreeSet<OpencodeListenerKey>,
    ) -> BTreeSet<OpencodeListenerKey> {
        for metadata in self.jobs.active_metadata() {
            if metadata.kind == TuiJobKind::OpencodeListener
                && let TuiJobKey::OpencodeListener(stream) = &metadata.key
                && !desired.contains(stream)
            {
                self.jobs.cancel(metadata.id);
            }
        }
        desired
            .iter()
            .filter(|stream| !self.opencode_listeners.contains(*stream))
            .cloned()
            .collect()
    }

    pub(crate) fn poll_opencode_status(&mut self) -> bool {
        if !self.tui_tick_active && !self.routing_tui_jobs {
            self.route_tui_job_messages();
        }
        let mut changed = false;
        while let Ok(result) = self.opencode_poll_rx.try_recv() {
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
                        let worktree = self.sessions[index]
                            .identity_key(&self.repos[self.sessions[index].repo_index].identity);
                        let reconciliation = self
                            .opencode_reconcile_requested
                            .get(&worktree)
                            .copied()
                            .filter(|requested_at| result.started_at >= *requested_at);
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
                        if let Some(requested_at) = reconciliation {
                            self.opencode_reconcile_requested.remove(&worktree);
                            self.opencode_event_watermarks
                                .insert(worktree, requested_at);
                        }
                    }
                }
                Err(error) => {
                    if error == "no OpenCode runtime exists yet" {
                        continue;
                    }
                    if let Some(repo) = self
                        .repos
                        .iter()
                        .find(|repo| repo.identity == result.key.worktree.repository)
                    {
                        let _ = append_runtime_message(
                            &repo.repo,
                            &format!(
                                "opencode status refresh failed for {}: {error}",
                                result.key.worktree.branch
                            ),
                        );
                    }
                }
            }
        }
        changed
    }

    pub(crate) fn poll_opencode_events(&mut self) -> bool {
        #[cfg(test)]
        while let Ok(result) = self.opencode_event_rx.try_recv() {
            self.opencode_events_changed |= self.apply_opencode_event_result(result);
        }
        std::mem::take(&mut self.opencode_events_changed)
    }

    pub(crate) fn apply_opencode_event_result(&mut self, result: OpencodeEventResult) -> bool {
        if self
            .worktree_generations
            .get(&result.stream.worktree)
            .copied()
            != Some(result.stream.generation)
        {
            return false;
        }
        if self
            .opencode_event_watermarks
            .get(&result.stream.worktree)
            .is_some_and(|watermark| result.received_at <= *watermark)
        {
            return false;
        }
        let mut changed = false;
        match result.event {
            Ok(event) => {
                let Some(session_id) = event.session_id.as_deref() else {
                    return false;
                };
                if session_id != result.stream.session_id {
                    return false;
                }
                let Some(index) = self.sessions.iter().position(|session| {
                    self.repos.get(session.repo_index).is_some_and(|managed| {
                        session.identity_key(&managed.identity) == result.stream.worktree
                    }) && session
                        .opencode_status
                        .as_ref()
                        .and_then(|status| status.server_url.as_deref())
                        == Some(result.stream.server_url.as_str())
                        && session
                            .opencode_status
                            .as_ref()
                            .and_then(|status| status.session_id.as_deref())
                            == Some(result.stream.session_id.as_str())
                }) else {
                    return false;
                };
                let current = self.sessions[index].opencode_status.clone();
                let mut status = current.unwrap_or_else(|| OpencodeStatus {
                    server_url: Some(result.stream.server_url.clone()),
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
                status.server_url = Some(result.stream.server_url.clone());
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
                        result.received_at,
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
                        session.identity_key(&managed.identity) == result.stream.worktree
                    }) && session
                        .opencode_status
                        .as_ref()
                        .and_then(|status| status.server_url.as_deref())
                        == Some(result.stream.server_url.as_str())
                        && session
                            .opencode_status
                            .as_ref()
                            .and_then(|status| status.session_id.as_deref())
                            == Some(result.stream.session_id.as_str()))
                    .then(|| self.repos.get(session.repo_index))
                    .flatten()
                }) {
                    let _ = append_runtime_message(
                        &repo.repo,
                        &format!(
                            "opencode event stream disconnected for {}: {error}",
                            result.stream.server_url
                        ),
                    );
                }
            }
        }
        changed
    }

    pub(crate) fn request_opencode_reconciliation_for(
        &mut self,
        worktree: crate::session::WorktreeSessionKey,
    ) {
        let requested_at = Instant::now();
        self.opencode_reconcile_requested
            .insert(worktree, requested_at);
        self.start_opencode_status_poll(true);
    }

    pub(super) fn apply_opencode_status(&mut self, index: usize, status: OpencodeStatus) -> bool {
        let notify = status.detail.as_deref() != Some("MessageAbortedError");
        let (changed, agent_state) = {
            let Some(session) = self.sessions.get_mut(index) else {
                return false;
            };
            let mut changed = false;
            let agent_state = status.state.agent_state();
            if session.opencode_status.as_ref() != Some(&status) {
                session.opencode_status = Some(status);
                changed = true;
            }
            (changed, agent_state)
        };
        self.apply_agent_state(index, agent_state, notify) || changed
    }

    pub(crate) async fn abort_selected_opencode_session(
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
            )
            .await?
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
        self.apply_agent_state(selected, AgentState::NeedsInput, false);
        self.start_opencode_status_poll(true);
        self.show_message("agent session abort requested")?;
        Ok(())
    }

    pub(crate) async fn shutdown_owned_opencode_servers(&mut self) -> Result<(), String> {
        let mut seen = BTreeSet::new();
        let mut errors = Vec::new();
        for session in &self.sessions {
            let Some(managed) = self.repos.get(session.repo_index) else {
                continue;
            };
            if !managed.config.opencode_shutdown_owned_servers {
                continue;
            }
            let key = session.identity_key(&managed.identity);
            let Some(harness_config) = self.worktree_harness_configs.get(&key) else {
                continue;
            };
            if !harness_config.selected_adapter_is("opencode") {
                continue;
            }
            let harness_id = harness_config.default_harness.clone();
            let runtime =
                match load_runtime(&managed.repo, &harness_id, &session.branch, &session.path) {
                    Ok(Some(runtime)) => runtime,
                    Ok(None) => continue,
                    Err(error) => {
                        errors.push(format!(
                            "load OpenCode runtime for {}: {error}",
                            session.branch
                        ));
                        continue;
                    }
                };
            let Some(pid) = runtime.server_pid else {
                continue;
            };
            if !seen.insert(pid) {
                continue;
            }
            if let Err(error) = opencode::shutdown_owned_server(&runtime).await {
                errors.push(format!("stop OpenCode server pid {pid}: {error}"));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}
