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
    pub(crate) fn refresh_sessions(&mut self) -> Result<(), String> {
        for managed in &mut self.repos {
            managed.config = crate::config::Config::load(&managed.repo);
        }
        let old = std::mem::take(&mut self.sessions);
        let mut by_path = old
            .into_iter()
            .map(|session| (session.identity_key(), session))
            .collect::<BTreeMap<_, _>>();
        let mut fresh = Vec::new();
        for (repo_index, managed) in self.repos.iter().enumerate() {
            let mut repo_sessions = discover_sessions(&managed.repo, &managed.config)?;
            for session in &mut repo_sessions {
                session.apply_repo_identity(repo_index, managed.label.clone(), managed.key);
                if let Some(previous) = by_path.remove(&session.identity_key()) {
                    session.preserve_refresh_state_from(previous, &managed.config);
                }
            }
            fresh.extend(repo_sessions);
        }
        self.sessions = fresh;
        self.ensure_navigation_valid();
        Ok(())
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
        self.show_loading_dialog(
            raw,
            "Create Session",
            &format!("Creating worktree for {}", branch.trim()),
        )?;
        if let Err(error) = create_worktree_session(&context.repo, &context.config, branch.trim()) {
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
            create_worktree_session(&context.repo, &context.config, branch.trim())?;
        }
        self.refresh_sessions()?;
        self.start_tmux_agent_warmup();
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
        write_task_metadata(&context.repo, &self.sessions[index], &initial_prompt)?;
        self.sessions[index].mark_adopted_with_prompt(&initial_prompt);
        if !initial_prompt.trim().is_empty() {
            self.show_loading_dialog(raw, "Create Session", "Starting agent session")?;
            self.paste_prompt_into_tmux_agent(index, &initial_prompt, false)?;
            self.show_message("pasted initial prompt into agent session")?;
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
            .map(|(worktree, key)| crate::view::KeyChoice {
                key: key.to_string(),
                label: format!(
                    "{}  {}  {}",
                    worktree.branch,
                    worktree.classification.label(),
                    worktree.worktree_path
                ),
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
        create_worktree_session(&context.repo, &context.config, &worktree.branch)?;
        unarchive_worktree_session(&context.repo, &worktree.branch)?;
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
        if !self.confirm_delete_dialog(raw, &branch, &path_display, &warnings)? {
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
        let key = DeleteSessionKey {
            repo_root: repo.root.clone(),
            path: path.clone(),
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
        let tx = self.delete_session_tx.clone();
        let branch_for_job = branch.clone();
        thread::spawn(move || {
            let result = delete_worktree_session(&repo, &config, &path, &branch_for_job);
            let _ = tx.send(DeleteSessionResult { key, result });
        });
        self.show_message(&format!("deleting {branch}..."))
    }

    pub(crate) fn poll_delete_sessions(&mut self) -> bool {
        let mut changed = false;
        while let Ok(result) = self.delete_session_rx.try_recv() {
            self.delete_sessions_in_flight.remove(&result.key);
            changed = true;
            match result.result {
                Ok(()) => {
                    if self.selected_worktree_by_repo.get(&result.key.repo_root)
                        == Some(&result.key.path)
                    {
                        self.selected_worktree_by_repo.remove(&result.key.repo_root);
                    }
                    match self.refresh_sessions() {
                        Ok(()) => {
                            self.start_tmux_agent_warmup();
                            self.start_wt_column_poll();
                            self.start_default_branch_status_poll(true);
                            let _ = self
                                .show_message("deleted local session data, worktree, and branch");
                        }
                        Err(error) => {
                            let _ = self
                                .show_message(&format!("delete complete; refresh failed: {error}"));
                        }
                    }
                }
                Err(error) => {
                    if let Some(session) = self
                        .sessions
                        .iter_mut()
                        .find(|session| session.path == result.key.path)
                    {
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
