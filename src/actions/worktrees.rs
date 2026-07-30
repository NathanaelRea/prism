use super::*;

pub(super) fn archive_choice_keys() -> Vec<String> {
    ('1'..='9')
        .chain('a'..='z')
        .map(|key| key.to_string())
        .collect()
}

pub(super) fn archived_picker_overflow_message(
    archived_count: usize,
    key_count: usize,
) -> Option<String> {
    (archived_count > key_count).then(|| {
        format!(
            "{archived_count} archived worktrees exceeds picker limit {key_count}; create by branch name to restore"
        )
    })
}

impl Tui {
    pub(crate) fn refresh_sessions_after_tmux(&mut self) -> Result<(), String> {
        self.route_tui_job_messages();
        self.poll_session_refresh();
        if self.session_refresh_in_flight {
            self.session_refresh_pending = true;
            return Ok(());
        }
        let base_generation = self.session_inventory_generation;
        let mut repos = self.repos.clone();
        let previous_repository_identities = self.session_repository_identities.clone();
        let known_tmux_slots = self
            .tmux_generations
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut sessions = self
            .sessions
            .iter()
            .map(crate::session::Session::background_job_snapshot)
            .collect::<Vec<_>>();
        self.session_refresh_in_flight = true;
        self.spawn_tui_job(
            TuiJobKind::SessionRefresh,
            TuiJobKey::None,
            base_generation,
            Some(TUI_ACTION_JOB_TIMEOUT),
            "prism-session-refresh".to_string(),
            move |context| {
                for managed in &mut repos {
                    managed.config = crate::config::Config::load(&managed.repo);
                }
                let repositories = repos
                    .iter()
                    .enumerate()
                    .map(
                        |(repo_index, managed)| crate::session::WorktreeSessionRepository {
                            repo_index,
                            repo: &managed.repo,
                            config: &managed.config,
                            label: &managed.label,
                            key: managed.key,
                            identity: &managed.identity,
                        },
                    )
                    .collect::<Vec<_>>();
                let baseline_sessions = sessions
                    .iter()
                    .filter_map(|session| {
                        let managed = repos.get(session.repo_index)?;
                        Some((
                            session.identity_key(&managed.identity),
                            session.background_job_snapshot(),
                        ))
                    })
                    .collect();
                let result = crate::session::refresh_worktree_sessions(
                    &repositories,
                    &previous_repository_identities,
                    &mut sessions,
                )
                .map(|()| {
                    let tmux_generations = sessions
                        .iter()
                        .filter_map(|session| {
                            let managed = repos.get(session.repo_index)?;
                            let slot = AgentSessionSlot::for_repository_session(
                                &managed.identity,
                                session,
                            );
                            if known_tmux_slots.contains(&slot) {
                                return None;
                            }
                            let generation = crate::tmux::latest_agent_session_generation(
                                &managed.repo,
                                &managed.config,
                                &session.branch,
                            )
                            .unwrap_or_default();
                            Some((slot, generation))
                        })
                        .collect();
                    SessionRefreshSnapshot {
                        repository_identities: repos
                            .iter()
                            .enumerate()
                            .map(|(index, repo)| (index, repo.identity.clone()))
                            .collect(),
                        configs: repos
                            .iter()
                            .map(|repo| (repo.identity.clone(), repo.config.clone()))
                            .collect(),
                        baseline_sessions,
                        worktree_harness_configs: crate::tui::load_worktree_harness_configs(
                            &repos, &sessions,
                        ),
                        tmux_generations,
                        sessions,
                    }
                });
                if result.is_err() && context.wait(Duration::from_secs(1)) {
                    return Ok(None);
                }
                Ok(Some(TuiJobPayload::SessionRefresh(SessionRefreshResult {
                    base_generation,
                    result,
                })))
            },
        );
        Ok(())
    }

    pub(crate) fn poll_session_refresh(&mut self) -> bool {
        if !self.tui_tick_active && !self.routing_tui_jobs {
            self.route_tui_job_messages();
        }
        let mut changed = false;
        let mut restart = false;
        while let Ok(result) = self.session_refresh_rx.try_recv() {
            if self.session_refresh_pending
                || result.base_generation != self.session_inventory_generation
            {
                restart |= self.session_refresh_pending;
                self.session_refresh_pending = false;
                continue;
            }
            let snapshot = match result.result {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    let _ = append_runtime_message(
                        &self.repo,
                        &format!("background Worktree Session refresh failed: {error}"),
                    );
                    restart = true;
                    continue;
                }
            };
            for managed in &mut self.repos {
                if let Some(config) = snapshot.configs.get(&managed.identity) {
                    managed.config = config.clone();
                }
            }
            let mut previous = self
                .sessions
                .iter()
                .filter_map(|session| {
                    let managed = self.repos.get(session.repo_index)?;
                    Some((
                        session.identity_key(&managed.identity),
                        session.background_job_snapshot(),
                    ))
                })
                .collect::<BTreeMap<_, _>>();
            let mut baseline = snapshot.baseline_sessions;
            self.sessions = snapshot
                .sessions
                .into_iter()
                .filter_map(|mut session| {
                    let repository = snapshot.repository_identities.get(&session.repo_index)?;
                    let repo_index = self
                        .repos
                        .iter()
                        .position(|managed| &managed.identity == repository)?;
                    let managed = &self.repos[repo_index];
                    session.apply_repo_identity(repo_index, managed.label.clone(), managed.key);
                    let identity = session.identity_key(&managed.identity);
                    if let Some(old) = previous.remove(&identity) {
                        if let Some(baseline) = baseline.remove(&identity) {
                            session.preserve_concurrent_refresh_state_from(&old, &baseline);
                        }
                        session.preserve_refresh_state_from(old, &managed.config);
                    }
                    Some(session)
                })
                .collect();
            self.worktree_harness_configs = snapshot.worktree_harness_configs;
            self.reconcile_session_inventory();
            for (slot, generation) in snapshot.tmux_generations {
                self.tmux_generations.entry(slot).or_insert(generation);
            }
            self.session_inventory_generation = self.session_inventory_generation.saturating_add(1);
            self.request_workflow_maintenance();
            changed = true;
        }
        if restart {
            let _ = self.refresh_sessions_after_tmux();
        }
        if changed {
            self.start_tmux_agent_warmup();
            self.start_wt_column_poll();
            self.start_default_branch_status_poll(true);
            self.start_opencode_status_poll(true);
            self.start_opencode_event_listeners();
            self.start_workflow_polls(true);
            self.poll_pull_requests(true);
        }
        changed
    }

    pub(crate) fn refresh_sessions(&mut self) -> Result<(), String> {
        for managed in &mut self.repos {
            managed.config = crate::config::Config::load(&managed.repo);
        }
        let repositories = self
            .repos
            .iter()
            .enumerate()
            .map(
                |(repo_index, managed)| crate::session::WorktreeSessionRepository {
                    repo_index,
                    repo: &managed.repo,
                    config: &managed.config,
                    label: &managed.label,
                    key: managed.key,
                    identity: &managed.identity,
                },
            )
            .collect::<Vec<_>>();
        crate::session::refresh_worktree_sessions(
            &repositories,
            &self.session_repository_identities,
            &mut self.sessions,
        )?;
        self.session_inventory_generation = self.session_inventory_generation.saturating_add(1);
        self.reconcile_session_inventory();
        self.worktree_harness_configs =
            crate::tui::load_worktree_harness_configs(&self.repos, &self.sessions);
        self.request_workflow_maintenance();
        Ok(())
    }

    fn reconcile_session_inventory(&mut self) {
        let live = self
            .sessions
            .iter()
            .filter_map(|session| {
                let repo = self.repos.get(session.repo_index)?;
                Some(session.identity_key(&repo.identity))
            })
            .collect::<BTreeSet<_>>();
        for (identity, generation) in &mut self.worktree_generations {
            if !live.contains(identity) {
                *generation = generation.saturating_add(1);
            }
        }
        for identity in &live {
            self.worktree_generations
                .entry(identity.clone())
                .or_default();
        }
        self.pr_persistence_pending.retain(|key, request| {
            live.contains(&key.worktree) || request.cache.summary().is_none()
        });
        self.pr_persistence_versions.retain(|key, _| {
            live.contains(&key.worktree)
                || self.pr_persistence_pending.contains_key(key)
                || self.pr_persistence_in_flight.contains(key)
        });
        self.retain_agent_state_persistence_for(&live);
        self.session_repository_identities = self
            .repos
            .iter()
            .enumerate()
            .map(|(index, repo)| (index, repo.identity.clone()))
            .collect();
        crate::agent_session::reconcile_worktree_sessions(
            &self.repos,
            &self.sessions,
            &mut self.tmux_generations,
        );
        self.ensure_navigation_valid();
    }

    pub(crate) fn create_session(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<bool, String> {
        let context = self
            .selected_repo_context()
            .ok_or_else(|| "no selected repository".to_string())?;
        self.ensure_default_branch_ready_for_create(raw)?;
        let repo_label = self
            .repos
            .get(context.repo_index)
            .map(|repo| repo.label.clone())
            .unwrap_or_else(|| context.repo.root.display().to_string());
        let branch_prompt = format!("Branch name for {repo_label}: ");
        let Some(branch) = self.prompt_line_dialog(raw, "Create Session", &branch_prompt, "")?
        else {
            return Ok(false);
        };
        if branch.trim().is_empty() {
            return Ok(false);
        }
        let Some(initial_prompt) =
            self.prompt_line_dialog(raw, "Create Session", "Initial prompt (optional): ", "")?
        else {
            return Ok(false);
        };
        if !initial_prompt.trim().is_empty()
            && !context.config.selected_harness()?.describe().initial_prompt
        {
            return Err(format!(
                "harness '{}' does not support an initial prompt; configure a reliable interactive_prompt_transport or create the session without a prompt",
                context.config.default_harness
            ));
        }
        self.show_loading_dialog(
            raw,
            "Create Session",
            &format!("Creating worktree for {}", branch.trim()),
        )?;
        let creation = match create_worktree_session(&context.repo, &context.config, branch.trim())
        {
            Ok(outcome) => outcome,
            Err(error) => {
                if !is_worktrunk_approval_failure(&error)
                    || !self.offer_worktrunk_approval(raw, &context.repo, &context.config)?
                {
                    return Err(error);
                }
                self.show_loading_dialog(
                    raw,
                    "Create Session",
                    &format!("Creating worktree for {}", branch.trim()),
                )?;
                create_worktree_session(&context.repo, &context.config, branch.trim())?
            }
        };
        if let CreateWorktreeOutcome::CreatedMetadataFailed { error } = creation {
            self.refresh_sessions()?;
            self.show_message(&format!(
                "worktree created, but restoring Prism metadata failed: {error}"
            ))?;
            return Ok(true);
        }
        self.refresh_sessions()?;
        self.start_wt_column_poll();
        let index = self
            .sessions
            .iter()
            .position(|session| session.matches_branch(context.repo_index, branch.trim()))
            .ok_or_else(|| {
                format!(
                    "created branch '{}' was not found in git worktree list",
                    branch.trim()
                )
            })?;
        if !self.visible_session_indices().contains(&index) {
            self.worktree_filter.clear();
        }
        self.select_worktree(index);
        crate::session::set_worktree_harness(
            &context.repo,
            &self.sessions[index],
            &context.config.default_harness,
            false,
        )?;
        self.reload_worktree_harness_config(index);
        let adoption = crate::session::adopt_worktree_session(
            &context.repo,
            &mut self.sessions[index],
            &initial_prompt,
        );
        if let crate::session::AdoptWorktreeOutcome::WorktreeCreatedMetadataFailed { error } =
            adoption
        {
            self.show_message(&format!(
                "worktree created, but Prism metadata adoption failed: {error}"
            ))?;
            return Ok(true);
        }
        if !initial_prompt.trim().is_empty() {
            self.show_loading_dialog(raw, "Create Session", "Starting agent session")?;
            self.paste_prompt_into_tmux_agent(index, &initial_prompt, false)?;
            self.show_message("submitted initial prompt to agent session")?;
        } else {
            self.start_tmux_agent_warmup();
        }
        Ok(true)
    }

    pub(super) fn ensure_default_branch_ready_for_create(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let context = self
            .selected_repo_context()
            .ok_or_else(|| "no selected repository".to_string())?;
        let Some(base) = context
            .config
            .default_base
            .as_deref()
            .map(str::trim)
            .filter(|base| !base.is_empty())
            .map(str::to_string)
        else {
            return Ok(());
        };
        let base_path = self.default_branch_path_for_repo(context.repo_index, &base);
        let behind = branch_behind(&base_path, &base, &context.config)?;
        if behind == 0 {
            return Ok(());
        }
        let should_pull = self.confirm_action_dialog(
            raw,
            "Default Branch Behind",
            &format!("{base} is behind origin/{base} by {behind}. Pull first?"),
            true,
        )?;
        if should_pull {
            self.show_loading_dialog(raw, "Pull Default Branch", &format!("Pulling {base}"))?;
            pull_branch(&base_path, &base, &context.config)?;
            self.refresh_sessions()?;
        }
        Ok(())
    }

    pub(crate) fn pull_default_branch(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let context = self
            .selected_repo_context()
            .ok_or_else(|| "no selected repository".to_string())?;
        let Some(base) = context
            .config
            .default_base
            .as_deref()
            .map(str::trim)
            .filter(|base| !base.is_empty())
            .map(str::to_string)
        else {
            self.show_message("no default_base configured")?;
            return Ok(());
        };
        let base_path = self.default_branch_path_for_repo(context.repo_index, &base);
        self.show_loading_dialog(raw, "Pull Default Branch", &format!("Pulling {base}"))?;
        pull_branch(&base_path, &base, &context.config)?;
        self.refresh_sessions()?;
        self.start_wt_column_poll();
        self.show_message(&format!("pulled {base}"))?;
        Ok(())
    }

    pub(super) fn default_branch_path_for_repo(&self, repo_index: usize, base: &str) -> PathBuf {
        self.sessions
            .iter()
            .find(|session| session.matches_branch(repo_index, base))
            .map(|session| session.path.clone())
            .or_else(|| {
                self.repos
                    .get(repo_index)
                    .map(|repo| repo.repo.root.clone())
            })
            .unwrap_or_else(|| self.repo.root.clone())
    }

    pub(crate) fn archive_session(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let Some(context) = self.selected_worktree_context() else {
            return Ok(());
        };
        let selected = context.session_index;
        let branch = self.sessions[selected].branch.clone();
        if self.sessions[selected].is_default_branch(&context.config) {
            self.show_message("default branch worktree cannot be archived from Prism")?;
            return Ok(());
        }
        let archive_key_count = archive_choice_keys().len();
        let archived_count = list_archived_worktrees(&context.repo)?.len();
        if archived_count >= archive_key_count {
            self.show_message(&format!(
                "archived worktree limit {archive_key_count} reached; unarchive one before archiving another"
            ))?;
            return Ok(());
        }
        let path = self.sessions[selected].path.clone();
        let path_display = self.sessions[selected].path_display.clone();
        let warnings = self.sessions[selected].archive_warnings();
        if !self.confirm_archive_dialog(raw, &branch, &path_display, &warnings)? {
            return Ok(());
        }
        archive_worktree_session(&context.repo, &self.sessions[selected])?;
        if self.selected_worktree_by_repo.get(&context.repo.root) == Some(&path) {
            self.selected_worktree_by_repo.remove(&context.repo.root);
        }
        self.refresh_sessions()?;
        self.show_message("archived worktree; files and branch were kept")?;
        Ok(())
    }

    pub(crate) fn unarchive_session(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let context = self
            .selected_repo_context()
            .ok_or_else(|| "no selected repository".to_string())?;
        let archived = list_archived_worktrees(&context.repo)?;
        if archived.is_empty() {
            self.show_message("no archived worktrees for selected repo")?;
            return Ok(());
        }
        let keys = archive_choice_keys();
        if let Some(message) = archived_picker_overflow_message(archived.len(), keys.len()) {
            self.show_message(&message)?;
            return Ok(());
        }
        let choices = archived
            .iter()
            .zip(keys.iter())
            .map(|(worktree, key)| {
                crate::view::KeyChoice::new(
                    key,
                    format!(
                        "{}  {}  {}",
                        worktree.branch,
                        worktree.classification.label(),
                        worktree.worktree_path
                    ),
                )
            })
            .collect::<Vec<_>>();
        let Some(answer) = self.prompt_choice_dialog(
            raw,
            crate::view::ChoiceList {
                title: "Unarchive Worktree".to_string(),
                choices,
            },
        )?
        else {
            return Ok(());
        };
        let Some(index) = keys.iter().position(|key| *key == answer) else {
            return Ok(());
        };
        let Some(worktree) = archived.get(index) else {
            return Ok(());
        };
        self.show_loading_dialog(
            raw,
            "Unarchive Worktree",
            &format!("Restoring {}", worktree.branch),
        )?;
        match create_worktree_session(&context.repo, &context.config, &worktree.branch)? {
            CreateWorktreeOutcome::Created | CreateWorktreeOutcome::Restored => {}
            CreateWorktreeOutcome::CreatedMetadataFailed { error } => {
                self.refresh_sessions()?;
                self.show_message(&format!(
                    "worktree restored, but Prism metadata restoration failed: {error}"
                ))?;
                return Ok(());
            }
        }
        self.refresh_sessions()?;
        self.start_tmux_agent_warmup();
        self.start_wt_column_poll();
        if let Some(index) = self
            .sessions
            .iter()
            .position(|session| session.matches_branch(context.repo_index, &worktree.branch))
        {
            if !self.visible_session_indices().contains(&index) {
                self.worktree_filter.clear();
            }
            self.select_worktree(index);
            self.focused_panel = crate::tui::PanelFocus::Worktrees;
            self.main_focused = false;
        }
        self.show_message("unarchived worktree")?;
        Ok(())
    }

    pub(crate) fn delete_session(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let Some(context) = self.selected_worktree_context() else {
            return Ok(());
        };
        let selected = context.session_index;
        let branch = self.sessions[selected].branch.clone();
        if self.sessions[selected].is_default_branch(&context.config) {
            self.show_message("default branch worktree cannot be deleted from Prism")?;
            return Ok(());
        }
        let path = self.sessions[selected].path.clone();
        let path_display = self.sessions[selected].path_display.clone();
        let warnings = self.sessions[selected].deletion_warnings();
        if !self.confirm_delete_dialog(raw, &branch, &path_display, &warnings, false)? {
            return Ok(());
        }
        self.start_delete_worktree_session(context.repo, context.config, path, branch)?;
        Ok(())
    }

    pub(crate) fn start_delete_worktree_session(
        &mut self,
        repo: Repository,
        config: Config,
        path: PathBuf,
        branch: String,
    ) -> Result<(), String> {
        let repository = self
            .repos
            .iter()
            .find(|managed| managed.repo.root == repo.root)
            .map(|managed| managed.identity.clone())
            .ok_or_else(|| "repository identity was not found".to_string())?;
        let worktree = self
            .sessions
            .iter()
            .find(|session| session.path == path && session.branch == branch)
            .map(|session| session.identity_key(&repository))
            .ok_or_else(|| "worktree session identity was not found".to_string())?;
        let key = DeleteSessionKey {
            generation: self
                .worktree_generations
                .get(&worktree)
                .copied()
                .unwrap_or_default(),
            worktree,
        };
        if !self.delete_sessions_in_flight.insert(key.clone()) {
            self.show_message("delete already in progress")?;
            return Ok(());
        }
        let selected_path = self
            .sessions
            .get(self.selected)
            .map(|session| session.path.clone());
        if let Some(session) = self
            .sessions
            .iter_mut()
            .find(|session| session.path == path)
        {
            session.hidden = true;
        }
        if selected_path.as_ref() == Some(&path) {
            self.ensure_navigation_valid();
        }
        let branch_for_job = branch.clone();
        let job_key = key.clone();
        self.spawn_tui_job(
            TuiJobKind::DeleteSession,
            TuiJobKey::Delete(key.clone()),
            key.generation,
            Some(TUI_ACTION_JOB_TIMEOUT),
            format!("prism-delete-{}", branch),
            move |context| {
                let result = crate::session::delete_worktree_session_if_current(
                    &repo,
                    &config,
                    &path,
                    &branch_for_job,
                    Some(&key.worktree.incarnation),
                );
                Ok(Some(TuiJobPayload::DeleteSession(DeleteSessionResult {
                    key: job_key,
                    delivery_id: context.id(),
                    result,
                })))
            },
        );
        self.show_message(&format!("deleting {branch}..."))
    }

    pub(crate) fn poll_delete_sessions(&mut self) -> bool {
        if !self.tui_tick_active && !self.routing_tui_jobs {
            self.route_tui_job_messages();
        }
        let mut changed = false;
        while let Ok(result) = self.delete_session_rx.try_recv() {
            let Some(current_generation) =
                self.worktree_generations.get(&result.key.worktree).copied()
            else {
                continue;
            };
            if current_generation != result.key.generation {
                continue;
            }
            changed = true;
            if matches!(
                &result.result,
                Ok(DeleteWorktreeOutcome::Deleted)
                    | Ok(DeleteWorktreeOutcome::BranchRetained {
                        owned_state_removed: true,
                        ..
                    })
            ) && let Some(index) = self.sessions.iter().position(|session| {
                self.repos
                    .get(session.repo_index)
                    .is_some_and(|repo| session.identity_key(&repo.identity) == result.key.worktree)
            }) {
                self.queue_pr_cache_removal(index);
                self.queue_agent_state_removal(index);
            }
            match result.result {
                Ok(DeleteWorktreeOutcome::Deleted) => {
                    self.sessions.retain(|session| {
                        session.path != result.key.worktree.path
                            || session.branch != result.key.worktree.branch
                    });
                    if self
                        .selected_worktree_by_repo
                        .get(&result.key.worktree.repository.root)
                        == Some(&result.key.worktree.path)
                    {
                        self.selected_worktree_by_repo
                            .remove(&result.key.worktree.repository.root);
                    }
                    self.ensure_navigation_valid();
                    self.session_inventory_generation =
                        self.session_inventory_generation.saturating_add(1);
                    self.reconcile_session_inventory();
                    let _ = self.refresh_sessions_after_tmux();
                    let _ = self.show_message("deleted local session data, worktree, and branch");
                }
                Ok(DeleteWorktreeOutcome::BranchRetained { error, .. }) => {
                    self.sessions.retain(|session| {
                        session.path != result.key.worktree.path
                            || session.branch != result.key.worktree.branch
                    });
                    if self
                        .selected_worktree_by_repo
                        .get(&result.key.worktree.repository.root)
                        == Some(&result.key.worktree.path)
                    {
                        self.selected_worktree_by_repo
                            .remove(&result.key.worktree.repository.root);
                    }
                    self.ensure_navigation_valid();
                    self.session_inventory_generation =
                        self.session_inventory_generation.saturating_add(1);
                    self.reconcile_session_inventory();
                    let _ = self.refresh_sessions_after_tmux();
                    let _ = self.show_message(&format!(
                        "worktree removed, but branch deletion failed: {error}"
                    ));
                }
                Ok(DeleteWorktreeOutcome::DeletedWithWarnings { errors, .. }) => {
                    self.sessions.retain(|session| {
                        session.path != result.key.worktree.path
                            || session.branch != result.key.worktree.branch
                    });
                    self.ensure_navigation_valid();
                    self.session_inventory_generation =
                        self.session_inventory_generation.saturating_add(1);
                    self.reconcile_session_inventory();
                    let _ = self.refresh_sessions_after_tmux();
                    let _ = self.show_message(&format!(
                        "worktree removed with cleanup warnings: {}",
                        errors.join("; ")
                    ));
                }
                Err(error) => {
                    if let Some(session) = self.sessions.iter_mut().find(|session| {
                        session.path == result.key.worktree.path
                            && session.branch == result.key.worktree.branch
                    }) {
                        session.hidden = false;
                    }
                    self.ensure_navigation_valid();
                    let _ = self.show_message(&format!("delete failed: {error}"));
                }
            }
        }
        changed
    }

    #[cfg(test)]
    pub(crate) fn start_delete_session_for_test(&mut self) -> Result<(), String> {
        let context = self
            .selected_worktree_context()
            .ok_or_else(|| "no selected worktree".to_string())?;
        let session = self
            .sessions
            .get(context.session_index)
            .ok_or_else(|| "no selected worktree".to_string())?;
        self.start_delete_worktree_session(
            context.repo,
            context.config,
            session.path.clone(),
            session.branch.clone(),
        )
    }
}
