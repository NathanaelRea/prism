use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::agent::{AgentAdapter, AgentProcess, AgentState};
use crate::git::{
    branch_behind, git_status_label, has_upstream, pull_branch, selected_dirty, worktree_dirty,
};
use crate::github::{
    PR_SUMMARY_POLL_INTERVAL, PrCache, fetch_pr_summary_index, pr_details_due, refresh_pr_cache,
    refresh_pr_details_cache, refresh_pr_summary_index, remove_pr_cache, save_pr_details_cache,
};
use crate::json::{json_bool_field, json_object_field, json_string_field, json_top_level_objects};
use crate::process::command_exists;
use crate::process::{run_capture, run_configured_commands, run_status};
use crate::review::build_review_fix_prompt;
use crate::session::{
    append_agent_log, append_runtime_log, clear_hidden, discover_sessions, remove_logs,
    remove_process_state, remove_task_metadata, save_agent_state, write_task_metadata,
};
use crate::tmux::{
    TmuxWindow, agent_session_running, attach_or_create_agent, attach_or_create_window,
    ensure_agent_session, kill_agent_session, latest_agent_session_generation, paste_agent_prompt,
};
use crate::tui::{
    DefaultBranchPollResult, PrPollKey, PrPollResult, TmuxSlotKey, TmuxWarmupKey, TmuxWarmupResult,
    Tui, WtPollResult,
};
use crate::util::{status_count, truncate, yes};

const DEFAULT_BRANCH_STATUS_POLL_INTERVAL: Duration = Duration::from_secs(60);

impl Tui {
    pub(crate) fn refresh_sessions(&mut self) -> Result<(), String> {
        let old = std::mem::take(&mut self.sessions);
        let mut by_path = old
            .into_iter()
            .map(|session| (session.path.clone(), session))
            .collect::<BTreeMap<_, _>>();
        let mut fresh = discover_sessions(&self.repo, &self.config)?;
        for session in &mut fresh {
            if let Some(mut previous) = by_path.remove(&session.path) {
                session.agent = previous.agent.take();
                session.agent_output = previous.agent_output;
                session.agent_state = previous.agent_state;
                session.pr = previous.pr;
                session.wt_columns = previous.wt_columns;
                session.unseen_comments = previous.unseen_comments;
            }
        }
        self.sessions = fresh;
        if self.selected >= self.sessions.len() {
            self.selected = self.sessions.len().saturating_sub(1);
        }
        if !self.session_filter.trim().is_empty()
            && !self.visible_session_indices().contains(&self.selected)
        {
            self.select_top_visible();
        }
        Ok(())
    }

    pub(crate) fn create_session(&mut self) -> Result<bool, String> {
        if !self.allow_dirty && worktree_dirty(&self.repo, &self.config)? {
            self.show_message(
                "current worktree is dirty; restart Prism with --allow-dirty to create anyway",
            )?;
            return Ok(false);
        }
        self.ensure_default_branch_ready_for_create()?;
        let Some(branch) = self.prompt_line_dialog("Create Session", "Branch name: ", "")? else {
            return Ok(false);
        };
        if branch.trim().is_empty() {
            return Ok(false);
        }
        let Some(initial_prompt) =
            self.prompt_line_dialog("Create Session", "Initial prompt (optional): ", "")?
        else {
            return Ok(false);
        };
        self.show_loading_dialog(
            "Create Session",
            &format!("Creating worktree for {}", branch.trim()),
        )?;
        create_worktree_session(&self.repo, &self.config, branch.trim())?;
        clear_hidden(&self.repo, branch.trim())?;
        self.refresh_sessions()?;
        self.start_tmux_agent_warmup();
        self.start_wt_column_poll();
        let index = self
            .sessions
            .iter()
            .position(|session| session.branch == branch.trim())
            .ok_or_else(|| {
                format!(
                    "created branch '{}' was not found in git worktree list",
                    branch.trim()
                )
            })?;
        self.selected = index;
        if !initial_prompt.trim().is_empty() {
            self.show_loading_dialog("Create Session", "Starting agent session")?;
            write_task_metadata(&self.repo, &self.sessions[index], &initial_prompt)?;
            self.sessions[index].prompt_summary = truncate(&initial_prompt.replace('\n', " "), 50);
            self.sessions[index].adopted = true;
            self.paste_prompt_into_tmux_agent(index, initial_prompt.trim())?;
            self.show_message("pasted initial prompt into agent session")?;
        }
        Ok(true)
    }

    fn ensure_default_branch_ready_for_create(&mut self) -> Result<(), String> {
        let Some(base) = self
            .config
            .default_base
            .as_deref()
            .map(str::trim)
            .filter(|base| !base.is_empty())
            .map(str::to_string)
        else {
            return Ok(());
        };
        let base_path = self.default_branch_path(&base);
        let behind = branch_behind(&base_path, &base, &self.config)?;
        if behind == 0 {
            return Ok(());
        }
        let answer = self.prompt_line_dialog(
            "Default Branch Behind",
            &format!("{base} is behind origin/{base} by {behind}. Pull first? [Y/n] "),
            "",
        )?;
        if answer.as_deref().map(yes_default).unwrap_or(false) {
            self.show_loading_dialog("Pull Default Branch", &format!("Pulling {base}"))?;
            pull_branch(&base_path, &base, &self.config)?;
            self.refresh_sessions()?;
        }
        Ok(())
    }

    pub(crate) fn pull_default_branch(&mut self) -> Result<(), String> {
        let Some(base) = self
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
        let base_path = self.default_branch_path(&base);
        self.show_loading_dialog("Pull Default Branch", &format!("Pulling {base}"))?;
        pull_branch(&base_path, &base, &self.config)?;
        self.refresh_sessions()?;
        self.start_wt_column_poll();
        self.show_message(&format!("pulled {base}"))?;
        Ok(())
    }

    fn default_branch_path(&self, base: &str) -> PathBuf {
        self.sessions
            .iter()
            .find(|session| session.branch == base)
            .map(|session| session.path.clone())
            .unwrap_or_else(|| self.repo.root.clone())
    }

    pub(crate) fn edit_config(
        &mut self,
        raw: &mut crate::terminal::RawTerminal,
    ) -> Result<(), String> {
        if let Some(parent) = self.config.repo_config_path.parent() {
            fs::create_dir_all(parent).map_err(|error| format!("create config dir: {error}"))?;
        }
        if !self.config.repo_config_path.exists() {
            fs::write(
                &self.config.repo_config_path,
                "# Prism repository config\n# Example:\n# [worktrees]\n# columns = [\"url\", \"vars.localdev\"]\n#\n# [prompt_templates]\n# review_fix = \"Here are review comments on PR {pr_number}.\\n\\nIf they are applicable, fix them. Otherwise, say why not.\\n\\n---\\n\\n{comments}\"\n",
            )
            .map_err(|error| format!("create config file: {error}"))?;
        }
        let editor =
            editor_command().ok_or_else(|| "no editor found; set VISUAL or EDITOR".to_string())?;
        raw.suspend()?;
        let result = Command::new(&editor)
            .arg(&self.config.repo_config_path)
            .status();
        let resume_result = raw.resume();
        resume_result?;
        let status = result.map_err(|error| format!("{editor}: {error}"))?;
        if !status.success() {
            return Err(format!("{editor} exited with {status}"));
        }
        self.config = crate::config::Config::load(&self.repo);
        self.refresh_sessions()?;
        self.start_tmux_agent_warmup();
        self.start_wt_column_poll();
        self.show_message("config reloaded")?;
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn launch_agent(
        &mut self,
        index: usize,
        initial_prompt: &str,
    ) -> Result<(), String> {
        let session = self
            .sessions
            .get_mut(index)
            .ok_or_else(|| "no selected session".to_string())?;
        let adapter = AgentAdapter::from_config(&self.config, &self.config.default_agent);
        let prompt = initial_prompt.trim();
        let launch = adapter.prepare_launch(prompt)?;
        let argv = launch.argv;
        if argv.is_empty() {
            return Err(format!(
                "agent '{}' has an empty command",
                self.config.default_agent
            ));
        }
        let mut agent = AgentProcess::spawn(&argv, &session.path, launch.prompt_file)?;
        if let Some(stdin_prompt) = launch.stdin_prompt {
            agent.write_all(format!("{stdin_prompt}\n").as_bytes())?;
        }
        session.agent = Some(agent);
        session.agent_state = AgentState::Running;
        let _ = save_agent_state(&self.repo, &session.branch, session.agent_state);
        session.agent_output.clear();
        session.agent_output.push_back(format!(
            "started {} ({})",
            self.config.default_agent,
            adapter.prompt_mode.label()
        ));
        Ok(())
    }

    pub(crate) fn poll_agents(&mut self) -> bool {
        let mut changed = false;
        for session in &mut self.sessions {
            if let Some(agent) = &mut session.agent {
                for chunk in agent.drain_output() {
                    let _ = append_agent_log(&self.repo, &session.branch, &chunk);
                    session.agent_output.push_back(chunk);
                    changed = true;
                }
                while session.agent_output.len() > 200 {
                    session.agent_output.pop_front();
                }
                if session.agent_state == AgentState::Running
                    && let Some(state) = agent.try_wait()
                {
                    session.agent_state = state;
                    let _ = save_agent_state(&self.repo, &session.branch, state);
                    changed = true;
                }
            }
        }
        changed
    }

    pub(crate) fn poll_pull_requests(&mut self, force: bool) -> bool {
        let changed = self.drain_pr_poll_results();
        let summaries_due = self
            .pr_summary_last_polled
            .map(|last| last.elapsed() >= PR_SUMMARY_POLL_INTERVAL)
            .unwrap_or(true);
        let has_pr_branches = self
            .sessions
            .iter()
            .any(|session| pr_pollable(&self.config, session));
        if has_pr_branches && (force || summaries_due) && !self.pr_summary_poll_in_flight {
            let path = self.repo.root.clone();
            let config = self.config.clone();
            let tx = self.pr_poll_tx.clone();
            self.pr_summary_last_polled = Some(std::time::Instant::now());
            self.pr_summary_poll_in_flight = true;
            std::thread::spawn(move || {
                let summaries = fetch_pr_summary_index(&path, &config);
                let _ = tx.send(PrPollResult::Summary { summaries });
            });
        }

        if let Some(session) = self.sessions.get_mut(self.selected) {
            let key = pr_poll_key(session);
            if pr_pollable(&self.config, session)
                && !session
                    .pr
                    .summary
                    .as_ref()
                    .is_some_and(|summary| summary.merged)
                && pr_details_due(&session.pr)
                && !self.pr_polls_in_flight.contains(&key)
            {
                let config = self.config.clone();
                let branch = session.branch.clone();
                let path = session.path.clone();
                let mut cache = session.pr.clone();
                let tx = self.pr_poll_tx.clone();
                session.pr.details_last_polled = Some(std::time::Instant::now());
                cache.details_last_polled = session.pr.details_last_polled;
                self.pr_polls_in_flight.insert(key.clone());
                std::thread::spawn(move || {
                    refresh_pr_details_cache(&branch, &mut cache, &path, &config);
                    let _ = tx.send(PrPollResult::Details {
                        key,
                        cache: Box::new(cache),
                    });
                });
            }
        }
        changed
    }

    fn drain_pr_poll_results(&mut self) -> bool {
        let mut changed = false;
        while let Ok(result) = self.pr_poll_rx.try_recv() {
            match result {
                PrPollResult::Summary { summaries } => {
                    self.pr_summary_poll_in_flight = false;
                    let before = self
                        .sessions
                        .iter()
                        .map(|session| pr_render_signature(&session.pr))
                        .collect::<Vec<_>>();
                    let before_comments = self
                        .sessions
                        .iter()
                        .map(session_comment_count)
                        .collect::<Vec<_>>();
                    match summaries {
                        Ok(summaries) => {
                            refresh_pr_summary_index(
                                &self.repo,
                                &mut self.sessions,
                                summaries,
                                &self.config,
                            );
                        }
                        Err(error) => {
                            for session in &mut self.sessions {
                                session.pr.error = Some(error.clone());
                            }
                        }
                    }
                    let after = self
                        .sessions
                        .iter()
                        .map(|session| pr_render_signature(&session.pr))
                        .collect::<Vec<_>>();
                    for (index, session) in self.sessions.iter_mut().enumerate() {
                        let before = before_comments.get(index).copied().unwrap_or(0);
                        let after = session_comment_count(session);
                        if after > before && index != self.selected {
                            session.unseen_comments = true;
                        }
                    }
                    changed |= before != after;
                }
                PrPollResult::Details { key, cache } => {
                    self.pr_polls_in_flight.remove(&key);
                    let selected_key = self.sessions.get(self.selected).map(pr_poll_key);
                    if let Some(session) = self
                        .sessions
                        .iter_mut()
                        .find(|session| pr_poll_key(session) == key)
                    {
                        let before = pr_render_signature(&session.pr);
                        let current_pr = session.pr.summary.as_ref().map(|summary| summary.number);
                        let result_pr = cache.summary.as_ref().map(|summary| summary.number);
                        if current_pr == result_pr {
                            let before_comments = session_comment_count(session);
                            session.pr.details = cache.details;
                            session.pr.details_last_polled = cache.details_last_polled;
                            session.pr.error = cache.error;
                            if let Some(details) = &session.pr.details {
                                let _ = save_pr_details_cache(&self.repo, &session.branch, details);
                            }
                            if session_comment_count(session) > before_comments
                                && selected_key.as_ref() != Some(&key)
                            {
                                session.unseen_comments = true;
                            }
                        }
                        changed |= before != pr_render_signature(&session.pr);
                    }
                }
            }
        }
        changed
    }

    pub(crate) fn start_wt_column_poll(&mut self) {
        self.poll_wt_columns();
        if self.wt_poll_in_flight || self.config.worktree_columns.is_empty() {
            return;
        }
        let repo = self.repo.clone();
        let config = self.config.clone();
        let tx = self.wt_poll_tx.clone();
        self.wt_poll_in_flight = true;
        std::thread::spawn(move || {
            let columns = fetch_wt_columns(&repo, &config);
            let _ = tx.send(WtPollResult { columns });
        });
    }

    pub(crate) fn poll_wt_columns(&mut self) -> bool {
        let mut changed = false;
        while let Ok(result) = self.wt_poll_rx.try_recv() {
            self.wt_poll_in_flight = false;
            match result.columns {
                Ok(columns_by_path) => {
                    for session in &mut self.sessions {
                        let next = columns_by_path
                            .get(&session.path)
                            .cloned()
                            .unwrap_or_default();
                        if session.wt_columns != next {
                            session.wt_columns = next;
                            changed = true;
                        }
                    }
                }
                Err(error) => {
                    let _ = append_runtime_log(
                        &self.repo,
                        &format!("wt column refresh failed: {error}"),
                    );
                }
            }
        }
        changed
    }

    pub(crate) fn start_default_branch_status_poll(&mut self, force: bool) {
        self.poll_default_branch_status();
        if self.default_branch_poll_in_flight {
            return;
        }
        let due = self
            .default_branch_last_polled
            .map(|last| last.elapsed() >= DEFAULT_BRANCH_STATUS_POLL_INTERVAL)
            .unwrap_or(true);
        if !force && !due {
            return;
        }
        let Some(branch) = self
            .config
            .default_base
            .as_deref()
            .map(str::trim)
            .filter(|branch| !branch.is_empty())
            .map(str::to_string)
        else {
            return;
        };
        let path = self.default_branch_path(&branch);
        let config = self.config.clone();
        let tx = self.default_branch_poll_tx.clone();
        self.default_branch_poll_in_flight = true;
        self.default_branch_last_polled = Some(std::time::Instant::now());
        std::thread::spawn(move || {
            let status_label = default_branch_status_label(&path, &branch, &config);
            let _ = tx.send(DefaultBranchPollResult {
                branch,
                path,
                status_label,
            });
        });
    }

    pub(crate) fn poll_default_branch_status(&mut self) -> bool {
        let mut changed = false;
        while let Ok(result) = self.default_branch_poll_rx.try_recv() {
            self.default_branch_poll_in_flight = false;
            match result.status_label {
                Ok(status_label) => {
                    if let Some(session) = self.sessions.iter_mut().find(|session| {
                        session.branch == result.branch && session.path == result.path
                    }) && session.status_label != status_label
                    {
                        session.status_label = status_label;
                        changed = true;
                    }
                }
                Err(error) => {
                    let _ = append_runtime_log(
                        &self.repo,
                        &format!("default branch status refresh failed: {error}"),
                    );
                }
            }
        }
        changed
    }

    pub(crate) fn attach_selected_agent_terminal(&mut self) -> Result<(), String> {
        if self.selected >= self.sessions.len() {
            return Ok(());
        }
        let session = session_for_tmux_warmup(&self.sessions[self.selected]);
        let slot = tmux_slot_key(&session);
        let generation = self.tmux_generation_for_slot(&slot);
        let key = tmux_warmup_key(slot.clone(), generation);
        self.finish_tmux_warmup_for_key(&key);
        attach_or_create_agent(&self.repo, &self.config, &session, generation)?;
        let running = agent_session_running(&self.repo, &self.config, &session, generation);
        self.update_tmux_agent_state_for_slot(&slot, running);
        if !running {
            let _ = kill_agent_session(&self.repo, &self.config, &session.branch, generation);
            let next_generation = self.rotate_tmux_generation(slot.clone());
            let next_key = tmux_warmup_key(slot, next_generation);
            self.start_tmux_agent_warmup_for_key(next_key, Duration::from_millis(250));
        }
        Ok(())
    }

    pub(crate) fn attach_selected_tmux_window(&mut self, window: TmuxWindow) -> Result<(), String> {
        if self.selected >= self.sessions.len() {
            return Ok(());
        }
        let session = session_for_tmux_warmup(&self.sessions[self.selected]);
        let slot = tmux_slot_key(&session);
        let generation = self.tmux_generation_for_slot(&slot);
        let key = tmux_warmup_key(slot.clone(), generation);
        self.finish_tmux_warmup_for_key(&key);
        attach_or_create_window(&self.repo, &self.config, &session, generation, window)?;
        let running = agent_session_running(&self.repo, &self.config, &session, generation);
        self.update_tmux_agent_state_for_slot(&slot, running);
        Ok(())
    }

    pub(crate) fn start_tmux_agent_warmup(&mut self) {
        self.poll_tmux_agent_warmup();
        let sessions = self
            .sessions
            .iter()
            .filter(|session| session.agent.is_none())
            .map(session_for_tmux_warmup)
            .collect::<Vec<_>>();
        let jobs = sessions
            .into_iter()
            .filter_map(|session| {
                let slot = tmux_slot_key(&session);
                let generation = self.tmux_generation_for_slot(&slot);
                let key = tmux_warmup_key(slot, generation);
                (!self.tmux_warmups_in_flight.contains(&key))
                    .then(|| (key, self.repo.clone(), self.config.clone(), session))
            })
            .collect::<Vec<_>>();

        for (key, repo, config, session) in jobs {
            self.spawn_tmux_warmup_job(key, repo, config, session, Duration::ZERO);
        }
    }

    fn start_tmux_agent_warmup_for_key(&mut self, key: TmuxWarmupKey, delay: Duration) {
        self.poll_tmux_agent_warmup();
        if self.tmux_warmups_in_flight.contains(&key) {
            return;
        }
        if !self.tmux_warmup_key_is_current(&key) {
            return;
        }
        let Some(session) = self
            .sessions
            .iter()
            .find(|session| tmux_slot_key(session) == key.slot)
        else {
            return;
        };
        self.spawn_tmux_warmup_job(
            key,
            self.repo.clone(),
            self.config.clone(),
            session_for_tmux_warmup(session),
            delay,
        );
    }

    fn spawn_tmux_warmup_job(
        &mut self,
        key: TmuxWarmupKey,
        repo: crate::repo::Repository,
        config: crate::config::Config,
        session: crate::session::Session,
        delay: Duration,
    ) {
        let tx = self.tmux_warmup_tx.clone();
        self.tmux_warmups_in_flight.insert(key.clone());
        std::thread::spawn(move || {
            if !delay.is_zero() {
                std::thread::sleep(delay);
            }
            let result = ensure_agent_session(&repo, &config, &session, key.generation);
            let (running, error) = match result {
                Ok(running) => (Some(running), None),
                Err(error) => (None, Some(error)),
            };
            let _ = tx.send(TmuxWarmupResult {
                key,
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

    fn finish_tmux_warmup_for_key(&mut self, key: &TmuxWarmupKey) -> bool {
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

    fn apply_tmux_warmup_result(&mut self, result: TmuxWarmupResult) -> bool {
        self.tmux_warmups_in_flight.remove(&result.key);
        if !self.tmux_warmup_key_is_current(&result.key) {
            return false;
        }
        if let Some(error) = result.error {
            let _ = append_runtime_log(
                &self.repo,
                &format!(
                    "tmux warm-up failed for {}#{}: {error}",
                    result.key.slot.branch, result.key.generation
                ),
            );
            return false;
        }
        let Some(running) = result.running else {
            return false;
        };
        let Some(session) = self
            .sessions
            .iter_mut()
            .find(|session| tmux_slot_key(session) == result.key.slot)
        else {
            return false;
        };
        let Some(state) = tmux_agent_state(session.agent_state, session.agent.is_some(), running)
        else {
            return false;
        };
        if session.agent_state == state {
            return false;
        }
        session.agent_state = state;
        let _ = save_agent_state(&self.repo, &session.branch, state);
        true
    }

    fn tmux_generation_for_slot(&mut self, slot: &TmuxSlotKey) -> u64 {
        if let Some(generation) = self.tmux_generations.get(slot).copied() {
            return generation;
        }
        let generation =
            latest_agent_session_generation(&self.repo, &self.config, &slot.branch).unwrap_or(0);
        self.tmux_generations.insert(slot.clone(), generation);
        generation
    }

    fn rotate_tmux_generation(&mut self, slot: TmuxSlotKey) -> u64 {
        let generation = self.tmux_generation_for_slot(&slot).saturating_add(1);
        self.tmux_generations.insert(slot, generation);
        generation
    }

    fn tmux_warmup_key_is_current(&self, key: &TmuxWarmupKey) -> bool {
        self.tmux_generations.get(&key.slot).copied().unwrap_or(0) == key.generation
    }

    fn update_tmux_agent_state_for_slot(&mut self, slot: &TmuxSlotKey, running: bool) -> bool {
        let Some(session) = self
            .sessions
            .iter_mut()
            .find(|session| tmux_slot_key(session) == *slot)
        else {
            return false;
        };
        let Some(state) = tmux_agent_state(session.agent_state, session.agent.is_some(), running)
        else {
            return false;
        };
        if session.agent_state == state {
            return false;
        }
        session.agent_state = state;
        let _ = save_agent_state(&self.repo, &session.branch, state);
        true
    }

    pub(crate) fn start_review_fix(&mut self) -> Result<(), String> {
        if self.selected >= self.sessions.len() {
            return Ok(());
        }
        if self
            .config
            .is_default_branch(&self.sessions[self.selected].branch)
        {
            self.show_message("default branch has no PR review comments")?;
            return Ok(());
        }
        let prompt = build_review_fix_prompt(&self.sessions[self.selected], &self.config)?;
        copy_to_clipboard(&self.config, &prompt)?;
        self.show_message("review-fix prompt copied to clipboard")?;
        Ok(())
    }

    fn paste_prompt_into_tmux_agent(&mut self, index: usize, prompt: &str) -> Result<(), String> {
        let session = self
            .sessions
            .get(index)
            .map(session_for_tmux_warmup)
            .ok_or_else(|| "no selected session".to_string())?;
        let slot = tmux_slot_key(&session);
        let generation = self.tmux_generation_for_slot(&slot);
        let key = tmux_warmup_key(slot.clone(), generation);
        self.finish_tmux_warmup_for_key(&key);
        paste_agent_prompt(&self.repo, &self.config, &session, generation, prompt)?;
        let running = agent_session_running(&self.repo, &self.config, &session, generation);
        self.update_tmux_agent_state_for_slot(&slot, running);
        Ok(())
    }

    pub(crate) fn push_selected_branch(&mut self) -> Result<(), String> {
        if self.selected >= self.sessions.len() {
            return Ok(());
        }
        let path = self.sessions[self.selected].path.clone();
        let branch = self.sessions[self.selected].branch.clone();
        if branch == "(detached)" {
            self.show_message("cannot push a detached worktree")?;
            return Ok(());
        }
        run_configured_commands(&self.config.checks.pre_push, &path, "pre_push")?;
        let args = if has_upstream(&path, &self.config)? {
            vec!["push".to_string()]
        } else {
            let Some(answer) = self.prompt_line_dialog(
                "Push Branch",
                &format!("No upstream. Push -u origin {branch}? [y/N] "),
                "",
            )?
            else {
                return Ok(());
            };
            if !yes(&answer) {
                return Ok(());
            }
            vec![
                "push".to_string(),
                "-u".to_string(),
                "origin".to_string(),
                branch,
            ]
        };
        self.show_loading_dialog("Push Branch", "Pushing selected branch")?;
        run_capture(
            Command::new(self.config.tool("git"))
                .arg("-C")
                .arg(&path)
                .args(args),
        )?;
        {
            let session = &mut self.sessions[self.selected];
            refresh_pr_cache(
                &self.repo,
                &session.branch,
                &mut session.pr,
                &session.path,
                &self.config,
                true,
            );
        }
        if self.sessions[self.selected].pr.summary.is_none()
            && !self
                .config
                .is_default_branch(&self.sessions[self.selected].branch)
        {
            run_configured_commands(&self.config.checks.pre_pr, &path, "pre_pr")?;
            let Some(pr_body) = self.prompt_pr_description()? else {
                return Ok(());
            };
            self.show_loading_dialog("Create Pull Request", "Creating pull request")?;
            run_capture(
                Command::new(self.config.tool("gh"))
                    .args(create_pr_args(
                        self.config.default_base.as_deref(),
                        &pr_body,
                    ))
                    .current_dir(&path),
            )?;
            let session = &mut self.sessions[self.selected];
            refresh_pr_cache(
                &self.repo,
                &session.branch,
                &mut session.pr,
                &session.path,
                &self.config,
                true,
            );
            self.show_message("push complete; pull request created")?;
        } else {
            self.show_message("push complete")?;
        }
        Ok(())
    }

    fn prompt_pr_description(&self) -> Result<Option<String>, String> {
        let Some(answer) =
            self.prompt_line_dialog("Create Pull Request", "Add description? [y/N] ", "")?
        else {
            return Ok(None);
        };
        if !yes(&answer) {
            return Ok(Some(String::new()));
        }
        self.prompt_line_dialog("Create Pull Request", "Description (empty for none): ", "")
    }

    pub(crate) fn merge_selected_pr(&mut self) -> Result<(), String> {
        if self.selected >= self.sessions.len() {
            return Ok(());
        }
        if self
            .config
            .is_default_branch(&self.sessions[self.selected].branch)
        {
            self.show_message("default branch is not treated as a PR branch")?;
            return Ok(());
        }
        let path = self.sessions[self.selected].path.clone();
        let branch = self.sessions[self.selected].branch.clone();
        {
            let session = &mut self.sessions[self.selected];
            refresh_pr_cache(
                &self.repo,
                &session.branch,
                &mut session.pr,
                &session.path,
                &self.config,
                false,
            );
        }
        let Some(summary) = self.sessions[self.selected].pr.summary.clone() else {
            self.show_message("no pull request found for selected branch")?;
            return Ok(());
        };
        if summary.merged {
            self.show_message("pull request is already merged")?;
            return Ok(());
        }
        if selected_dirty(&path, &self.config)? {
            self.show_message("working tree is dirty; commit or stash before merging")?;
            return Ok(());
        }
        run_configured_commands(&self.config.checks.pre_push, &path, "pre_push")?;
        self.show_message(&format!("merging PR #{}", summary.number))?;
        let pr_number = summary.number.to_string();
        run_status(
            Command::new(self.config.tool("gh"))
                .args(["pr", "merge", &pr_number, "--merge", "--delete-branch"])
                .current_dir(&path),
        )?;
        if branch != "(detached)" {
            let _ = run_status(
                Command::new(self.config.tool("git"))
                    .arg("-C")
                    .arg(&self.repo.root)
                    .args(["branch", "-D", &branch]),
            );
        }
        self.refresh_sessions()?;
        self.show_message("merge complete")?;
        Ok(())
    }

    pub(crate) fn delete_session(&mut self) -> Result<(), String> {
        if self.selected >= self.sessions.len() {
            return Ok(());
        }
        let branch = self.sessions[self.selected].branch.clone();
        let path = self.sessions[self.selected].path.clone();
        let path_display = self.sessions[self.selected].path_display.clone();
        let warnings = delete_warnings(&self.sessions[self.selected]);
        if !self.confirm_delete_dialog(&branch, &path_display, &warnings)? {
            return Ok(());
        }
        self.delete_local_data(&branch)?;
        run_status(
            Command::new(self.config.tool("git"))
                .arg("-C")
                .arg(&self.repo.root)
                .args(["worktree", "remove", "--force"])
                .arg(&path),
        )?;
        if branch != "(detached)" {
            run_status(
                Command::new(self.config.tool("git"))
                    .arg("-C")
                    .arg(&self.repo.root)
                    .args(["branch", "-D", &branch]),
            )?;
        }
        self.refresh_sessions()?;
        self.show_message("deleted local session data, worktree, and branch")?;
        Ok(())
    }

    fn delete_local_data(&self, branch: &str) -> Result<(), String> {
        remove_task_metadata(&self.repo, branch)?;
        remove_pr_cache(&self.repo, branch)?;
        remove_logs(&self.repo, branch)?;
        remove_process_state(&self.repo, branch)?;
        clear_hidden(&self.repo, branch)?;
        Ok(())
    }
}

fn copy_to_clipboard(config: &crate::config::Config, text: &str) -> Result<(), String> {
    let candidates: [(&str, &[&str]); 4] = [
        (&config.tool("wl-copy"), &[]),
        (&config.tool("xclip"), &["-selection", "clipboard"]),
        (&config.tool("xsel"), &["--clipboard", "--input"]),
        (&config.tool("pbcopy"), &[]),
    ];
    let mut errors = Vec::new();
    for (program, args) in candidates {
        if !clipboard_command_exists(program) {
            continue;
        }
        match write_clipboard_command(program, args, text) {
            Ok(()) => return Ok(()),
            Err(error) => errors.push(error),
        }
    }
    if errors.is_empty() {
        Err("no clipboard tool found; install wl-copy, xclip, xsel, or pbcopy".to_string())
    } else {
        Err(format!("clipboard copy failed: {}", errors.join("; ")))
    }
}

fn clipboard_command_exists(program: &str) -> bool {
    let program = program.split_whitespace().next().unwrap_or(program);
    !program.is_empty() && command_exists(program)
}

fn write_clipboard_command(program: &str, args: &[&str], text: &str) -> Result<(), String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("{program}: {error}"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| format!("{program}: stdin unavailable"))?;
    stdin
        .write_all(text.as_bytes())
        .map_err(|error| format!("{program}: {error}"))?;
    drop(stdin);
    Ok(())
}

fn tmux_agent_state(
    current: AgentState,
    has_embedded_agent: bool,
    tmux_agent_running: bool,
) -> Option<AgentState> {
    if tmux_agent_running && !has_embedded_agent {
        return Some(AgentState::NeedsInput);
    }
    if !has_embedded_agent && matches!(current, AgentState::Running | AgentState::NeedsRestart) {
        return Some(AgentState::ExitedOk);
    }
    None
}

fn create_worktree_session(
    repo: &crate::repo::Repository,
    config: &crate::config::Config,
    branch: &str,
) -> Result<(), String> {
    run_capture(
        Command::new(config.tool(&config.worktree_command)).args(create_worktree_args(
            &repo.root,
            branch,
            config.default_base.as_deref(),
        )),
    )?;
    Ok(())
}

fn create_worktree_args(repo_root: &Path, branch: &str, default_base: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "-C".to_string(),
        repo_root.display().to_string(),
        "switch".to_string(),
        "--create".to_string(),
        "--no-cd".to_string(),
        "--format".to_string(),
        "json".to_string(),
    ];
    if let Some(base) = default_base.map(str::trim).filter(|base| !base.is_empty()) {
        args.push("--base".to_string());
        args.push(base.to_string());
    }
    args.push(branch.to_string());
    args
}

fn create_pr_args(default_base: Option<&str>, body: &str) -> Vec<String> {
    let mut args = vec![
        "pr".to_string(),
        "create".to_string(),
        "--fill".to_string(),
        "--body".to_string(),
        body.to_string(),
    ];
    if let Some(base) = default_base.map(str::trim).filter(|base| !base.is_empty()) {
        args.push("--base".to_string());
        args.push(base.to_string());
    }
    args
}

fn pr_render_signature(cache: &PrCache) -> String {
    format!(
        "{:?}|{:?}|{:?}|{:?}",
        cache.summary, cache.details, cache.last_refreshed, cache.error
    )
}

fn pr_poll_key(session: &crate::session::Session) -> PrPollKey {
    PrPollKey {
        branch: session.branch.clone(),
        path: session.path.clone(),
    }
}

fn pr_pollable(config: &crate::config::Config, session: &crate::session::Session) -> bool {
    session.branch != "(detached)"
        && !config.is_default_branch(&session.branch)
        && !session
            .pr
            .summary
            .as_ref()
            .is_some_and(|summary| summary.merged)
}

fn session_comment_count(session: &crate::session::Session) -> usize {
    session
        .pr
        .details
        .as_ref()
        .map(|details| details.comments.len() + details.review_comments.len())
        .or_else(|| {
            session
                .pr
                .summary
                .as_ref()
                .map(|summary| summary.comment_count as usize)
        })
        .unwrap_or(0)
}

fn fetch_wt_columns(
    repo: &crate::repo::Repository,
    config: &crate::config::Config,
) -> Result<BTreeMap<PathBuf, BTreeMap<String, String>>, String> {
    let raw = run_capture(
        Command::new(config.tool(&config.worktree_command))
            .arg("-C")
            .arg(&repo.root)
            .args(["list", "--format=json"]),
    )?;
    let mut by_path = BTreeMap::new();
    for object in json_top_level_objects(&raw) {
        let Some(path) = json_string_field(object, "path") else {
            continue;
        };
        let mut columns = BTreeMap::new();
        for column in &config.worktree_columns {
            if let Some(value) = wt_column_value(object, column) {
                columns.insert(column.clone(), value);
            }
        }
        by_path.insert(PathBuf::from(path), columns);
    }
    Ok(by_path)
}

fn default_branch_status_label(
    path: &Path,
    branch: &str,
    config: &crate::config::Config,
) -> Result<String, String> {
    let behind = branch_behind(path, branch, config)?;
    Ok(status_label_with_behind(
        &git_status_label(path, config),
        behind,
    ))
}

fn status_label_with_behind(label: &str, behind: usize) -> String {
    let dirty = status_count(label, "dirty");
    let ahead = status_count(label, "ahead");
    let mut parts = Vec::new();
    if let Some(count) = dirty {
        parts.push(format!("dirty {count}"));
    }
    if let Some(count) = ahead {
        parts.push(format!("ahead {count}"));
    }
    if behind > 0 {
        parts.push(format!("behind {behind}"));
    }
    if !parts.is_empty() {
        return parts.join(" ");
    }
    if label == "clean" || status_count(label, "behind").is_some() {
        "clean".to_string()
    } else {
        label.to_string()
    }
}

fn wt_column_value(object: &str, column: &str) -> Option<String> {
    if let Some(key) = column.strip_prefix("vars.") {
        return json_object_field(object, "vars").and_then(|vars| json_string_field(vars, key));
    }
    if let Some((object_key, field_key)) = column.split_once('.') {
        return json_object_field(object, object_key)
            .and_then(|inner| json_string_field(inner, field_key));
    }
    json_string_field(object, column)
        .or_else(|| json_bool_field(object, column).map(|value| value.to_string()))
        .or_else(|| {
            if column == "ci" {
                json_object_field(object, "ci").map(|ci| {
                    let status = json_string_field(ci, "status").unwrap_or_default();
                    let number = crate::json::json_u64_field(ci, "number")
                        .map(|number| format!("#{number}"))
                        .unwrap_or_else(|| "ci".to_string());
                    if status.is_empty() {
                        number
                    } else {
                        format!("{number}:{status}")
                    }
                })
            } else {
                None
            }
        })
}

fn yes_default(answer: &str) -> bool {
    !matches!(answer.trim(), "n" | "N" | "no" | "NO")
}

fn editor_command() -> Option<String> {
    std::env::var("VISUAL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var("EDITOR")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .or_else(|| {
            ["nvim", "vim", "vi"]
                .into_iter()
                .find(|editor| command_exists(editor))
                .map(str::to_string)
        })
}

fn tmux_slot_key(session: &crate::session::Session) -> TmuxSlotKey {
    TmuxSlotKey {
        branch: session.branch.clone(),
        path: session.path.clone(),
    }
}

fn tmux_warmup_key(slot: TmuxSlotKey, generation: u64) -> TmuxWarmupKey {
    TmuxWarmupKey { slot, generation }
}

fn session_for_tmux_warmup(session: &crate::session::Session) -> crate::session::Session {
    crate::session::Session {
        path: session.path.clone(),
        path_display: session.path_display.clone(),
        branch: session.branch.clone(),
        prompt_summary: session.prompt_summary.clone(),
        adopted: session.adopted,
        hidden: session.hidden,
        status_label: session.status_label.clone(),
        agent: None,
        agent_output: std::collections::VecDeque::new(),
        agent_state: session.agent_state,
        pr: session.pr.clone(),
        wt_columns: session.wt_columns.clone(),
        unseen_comments: session.unseen_comments,
    }
}

fn delete_warnings(session: &crate::session::Session) -> Vec<String> {
    let mut warnings = Vec::new();
    if status_count(&session.status_label, "dirty").is_some() {
        warnings.push("dirty worktree: uncommitted changes will be deleted".to_string());
    }
    if status_count(&session.status_label, "ahead").is_some() {
        warnings.push("branch is ahead of upstream: unpushed commits may be lost".to_string());
    }
    if status_count(&session.status_label, "behind").is_some() {
        warnings.push("branch is behind upstream".to_string());
    }
    if !session.adopted {
        warnings.push("session was not created by Prism".to_string());
    }
    if session.branch == "(detached)" {
        warnings.push("detached worktree: no local branch will be deleted".to_string());
    }
    if session.agent_state == AgentState::Running {
        warnings.push("agent is still running".to_string());
    }
    if let Some(summary) = &session.pr.summary
        && !summary.merged
    {
        warnings.push(format!("open PR #{} still exists", summary.number));
    }
    warnings
}

#[cfg(test)]
mod tests {
    use crate::agent::AgentState;
    use crate::config::{Checks, Config, EscapeKey};
    use crate::github::PrCache;
    use crate::repo::Repository;
    use crate::session::Session;
    use crate::tui::{TmuxWarmupResult, Tui};

    use super::{
        create_pr_args, create_worktree_args, status_label_with_behind, tmux_agent_state,
        tmux_slot_key, tmux_warmup_key,
    };
    use std::collections::{BTreeMap, VecDeque};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    #[test]
    fn idle_tmux_opencode_session_does_not_count_as_running_agent() {
        let state = tmux_agent_state(AgentState::Idle, false, true);

        assert_eq!(state, Some(AgentState::NeedsInput));
    }

    #[test]
    fn stale_running_state_without_process_is_cleared() {
        let state = tmux_agent_state(AgentState::Running, false, false);

        assert_eq!(state, Some(AgentState::ExitedOk));
    }

    #[test]
    fn embedded_agent_process_owns_running_state() {
        let state = tmux_agent_state(AgentState::Running, true, true);

        assert_eq!(state, None);
    }

    #[test]
    fn create_worktree_uses_worktrunk_without_changing_directory() {
        let args = create_worktree_args(
            PathBuf::from("/repo/prism").as_path(),
            "feat/test",
            Some("main"),
        );

        assert_eq!(
            args,
            vec![
                "-C",
                "/repo/prism",
                "switch",
                "--create",
                "--no-cd",
                "--format",
                "json",
                "--base",
                "main",
                "feat/test",
            ]
        );
    }

    #[test]
    fn create_pr_uses_fill_with_explicit_empty_body_and_default_base_when_configured() {
        assert_eq!(
            create_pr_args(Some("main"), ""),
            vec!["pr", "create", "--fill", "--body", "", "--base", "main"]
        );
        assert_eq!(
            create_pr_args(None, "manual description"),
            vec!["pr", "create", "--fill", "--body", "manual description"]
        );
    }

    #[test]
    fn default_branch_status_replaces_stale_behind_count() {
        assert_eq!(status_label_with_behind("clean", 2), "behind 2");
        assert_eq!(status_label_with_behind("dirty 1 behind 9", 0), "dirty 1");
        assert_eq!(
            status_label_with_behind("dirty 1 ahead 3 behind 9", 2),
            "dirty 1 ahead 3 behind 2"
        );
    }

    #[test]
    fn automatic_pr_polling_does_not_block_input_loop() {
        let temp = unique_temp_dir("prism-pr-poll-test");
        fs::create_dir_all(&temp).unwrap();
        let gh = temp.join("gh");
        fs::write(
            &gh,
            r#"#!/bin/sh
sleep 1
echo 'no pull requests found' >&2
exit 1
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&gh).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&gh, permissions).unwrap();

        let mut config = test_config();
        config
            .tools
            .insert("gh".to_string(), gh.display().to_string());
        let repo = Repository { root: temp.clone() };
        let session = test_session(temp.join("worktree"), "feature");
        let mut tui = Tui::new(repo, config, vec![session], false);

        let started = Instant::now();
        let changed = tui.poll_pull_requests(false);

        assert!(!changed);
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "automatic PR polling blocked for {:?}",
            started.elapsed()
        );

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn default_branch_does_not_start_pr_polling() {
        let temp = unique_temp_dir("prism-default-branch-pr-poll-test");
        fs::create_dir_all(&temp).unwrap();

        let mut config = test_config();
        config.default_base = Some("main".to_string());
        let repo = Repository { root: temp.clone() };
        let session = test_session(temp.join("worktree"), "main");
        let mut tui = Tui::new(repo, config, vec![session], false);

        let changed = tui.poll_pull_requests(false);

        assert!(!changed);
        assert!(!tui.pr_summary_poll_in_flight);
        assert!(tui.pr_polls_in_flight.is_empty());

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn tmux_agent_warmup_does_not_block_startup() {
        let temp = unique_temp_dir("prism-tmux-warmup-test");
        fs::create_dir_all(&temp).unwrap();
        let state = temp.join("tmux-state");
        let tmux = temp.join("tmux");
        fs::write(
            &tmux,
            format!(
                r#"#!/bin/sh
state="$(cat '{}' 2>/dev/null || echo missing)"
case "$1" in
  has-session)
    sleep 1
    [ "$state" = exists ]
    exit $?
    ;;
  new-session)
    echo exists > '{}'
    exit 0
    ;;
  set-option)
    exit 0
    ;;
  display-message)
    echo opencode
    exit 0
    ;;
esac
exit 0
"#,
                state.display(),
                state.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&tmux).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&tmux, permissions).unwrap();

        let mut config = test_config();
        config.default_agent = "opencode".to_string();
        config
            .tools
            .insert("tmux".to_string(), tmux.display().to_string());
        config
            .tools
            .insert("opencode".to_string(), "opencode".to_string());
        let repo = Repository { root: temp.clone() };
        let session = test_session(temp.join("worktree"), "feature");
        let mut tui = Tui::new(repo, config, vec![session], false);

        let started = Instant::now();
        tui.start_tmux_agent_warmup();

        assert!(
            started.elapsed() < Duration::from_millis(250),
            "tmux warm-up blocked startup for {:?}",
            started.elapsed()
        );
        assert_eq!(tui.tmux_warmups_in_flight.len(), 1);

        let wait_started = Instant::now();
        while !tui.tmux_warmups_in_flight.is_empty()
            && wait_started.elapsed() < Duration::from_secs(3)
        {
            tui.poll_tmux_agent_warmup();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(tui.tmux_warmups_in_flight.is_empty());

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn attach_waits_for_selected_tmux_warmup() {
        let temp = unique_temp_dir("prism-tmux-attach-wait-test");
        fs::create_dir_all(&temp).unwrap();
        let tmux = temp.join("tmux");
        fs::write(
            &tmux,
            r#"#!/bin/sh
case "$1" in
  has-session|set-option|attach-session)
    exit 0
    ;;
  display-message)
    echo opencode
    exit 0
    ;;
esac
exit 0
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&tmux).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&tmux, permissions).unwrap();

        let mut config = test_config();
        config.default_agent = "opencode".to_string();
        config
            .tools
            .insert("tmux".to_string(), tmux.display().to_string());
        config
            .tools
            .insert("opencode".to_string(), "opencode".to_string());
        let repo = Repository { root: temp.clone() };
        let session = test_session(temp.join("worktree"), "feature");
        let key = tmux_warmup_key(tmux_slot_key(&session), 0);
        let mut tui = Tui::new(repo, config, vec![session], false);
        tui.tmux_warmups_in_flight.insert(key.clone());
        let tx = tui.tmux_warmup_tx.clone();

        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            let _ = tx.send(TmuxWarmupResult {
                key,
                running: Some(true),
                error: None,
            });
        });

        let started = Instant::now();
        tui.attach_selected_agent_terminal().unwrap();

        assert!(
            started.elapsed() >= Duration::from_millis(100),
            "attach did not wait for selected warm-up"
        );
        let wait_started = Instant::now();
        while !tui.tmux_warmups_in_flight.is_empty()
            && wait_started.elapsed() < Duration::from_secs(3)
        {
            tui.poll_tmux_agent_warmup();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(tui.tmux_warmups_in_flight.is_empty());

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn prompt_paste_targets_tmux_agent_session() {
        let temp = unique_temp_dir("prism-tmux-prompt-paste-test");
        fs::create_dir_all(&temp).unwrap();
        let log = temp.join("tmux.log");
        let prompt_file = temp.join("prompt.txt");
        let tmux = temp.join("tmux");
        fs::write(
            &tmux,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$1" in
  has-session|set-option|move-window|rename-window|new-window)
    exit 0
    ;;
  list-windows)
    exit 0
    ;;
  display-message)
    echo opencode
    exit 0
    ;;
  capture-pane)
    echo 'Ask anything'
    exit 0
    ;;
  load-buffer)
    cat > '{}'
    exit 0
    ;;
  paste-buffer)
    exit 0
    ;;
esac
exit 1
"#,
                log.display(),
                prompt_file.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&tmux).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&tmux, permissions).unwrap();

        let mut config = test_config();
        config.default_agent = "opencode".to_string();
        config
            .tools
            .insert("tmux".to_string(), tmux.display().to_string());
        config
            .tools
            .insert("opencode".to_string(), "opencode".to_string());
        let repo = Repository { root: temp.clone() };
        let session = test_session(temp.join("worktree"), "feature");
        let mut tui = Tui::new(repo, config, vec![session], false);

        tui.paste_prompt_into_tmux_agent(0, "build the thing")
            .unwrap();

        assert_eq!(fs::read_to_string(&prompt_file).unwrap(), "build the thing");
        assert!(tui.sessions[0].agent.is_none());
        assert_eq!(tui.sessions[0].agent_state, AgentState::NeedsInput);
        let commands = fs::read_to_string(&log).unwrap();
        assert!(commands.contains("load-buffer -b"));
        assert!(commands.contains("paste-buffer -d -b"));

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn stale_tmux_warmup_result_does_not_update_current_generation() {
        let temp = unique_temp_dir("prism-tmux-stale-generation-test");
        fs::create_dir_all(&temp).unwrap();
        let mut config = test_config();
        config.default_agent = "opencode".to_string();
        let repo = Repository { root: temp.clone() };
        let session = test_session(temp.join("worktree"), "feature");
        let slot = tmux_slot_key(&session);
        let stale_key = tmux_warmup_key(slot.clone(), 0);
        let mut tui = Tui::new(repo, config, vec![session], false);
        tui.tmux_generations.insert(slot, 1);

        let changed = tui.apply_tmux_warmup_result(TmuxWarmupResult {
            key: stale_key,
            running: Some(true),
            error: None,
        });

        assert!(!changed);
        assert_eq!(tui.sessions[0].agent_state, AgentState::Idle);

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn attach_schedules_delayed_rewarm_after_return() {
        let temp = unique_temp_dir("prism-tmux-delayed-rewarm-test");
        fs::create_dir_all(&temp).unwrap();
        let log = temp.join("tmux.log");
        let count = temp.join("display-count");
        let tmux = temp.join("tmux");
        fs::write(
            &tmux,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$1" in
  has-session|set-option|attach-session|kill-session|new-session)
    exit 0
    ;;
  display-message)
    count="$(cat '{}' 2>/dev/null || echo 0)"
    count="$((count + 1))"
    echo "$count" > '{}'
    if [ "$count" -eq 1 ]; then
      echo opencode
    else
      echo bash
    fi
    exit 0
    ;;
esac
exit 0
"#,
                log.display(),
                count.display(),
                count.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&tmux).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&tmux, permissions).unwrap();

        let mut config = test_config();
        config.default_agent = "opencode".to_string();
        config
            .tools
            .insert("tmux".to_string(), tmux.display().to_string());
        config
            .tools
            .insert("opencode".to_string(), "opencode".to_string());
        let repo = Repository { root: temp.clone() };
        let session = test_session(temp.join("worktree"), "feature");
        let mut tui = Tui::new(repo, config, vec![session], false);

        tui.attach_selected_agent_terminal().unwrap();

        let wait_started = Instant::now();
        while !tui.tmux_warmups_in_flight.is_empty()
            && wait_started.elapsed() < Duration::from_secs(3)
        {
            tui.poll_tmux_agent_warmup();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(tui.tmux_warmups_in_flight.is_empty());
        let commands = fs::read_to_string(&log).unwrap();
        assert!(commands.contains("kill-session -t"));
        assert!(commands.contains("new-session -d -s"));

        let _ = fs::remove_dir_all(temp);
    }

    fn test_session(path: PathBuf, branch: &str) -> Session {
        fs::create_dir_all(&path).unwrap();
        Session {
            path: path.clone(),
            path_display: path.display().to_string(),
            branch: branch.to_string(),
            prompt_summary: String::new(),
            adopted: false,
            hidden: false,
            status_label: "clean".to_string(),
            agent: None,
            agent_output: VecDeque::new(),
            agent_state: AgentState::Idle,
            pr: PrCache::default(),
            wt_columns: BTreeMap::new(),
            unseen_comments: false,
        }
    }

    fn test_config() -> Config {
        Config {
            default_agent: "ask".to_string(),
            default_base: None,
            plan_dir: "plans".to_string(),
            review_packet_dir: ".agent/review".to_string(),
            worktree_command: "wt".to_string(),
            escape_key: EscapeKey::EscEsc,
            checks: Checks::default(),
            worktree_columns: Vec::new(),
            tools: BTreeMap::new(),
            agent_commands: BTreeMap::new(),
            agent_prompt_modes: BTreeMap::new(),
            prompt_templates: BTreeMap::new(),
            user_path: PathBuf::from("/tmp/prism-user-config.toml"),
            repo_config_path: PathBuf::from("/tmp/prism-repo-config.toml"),
        }
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }
}
