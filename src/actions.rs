use std::collections::BTreeMap;
use std::fs;
use std::process::Command;
use std::time::Duration;

use crate::agent::{AgentAdapter, AgentProcess, AgentState};
use crate::git::{has_upstream, selected_dirty, worktree_dirty};
use crate::github::{
    PR_SUMMARY_POLL_INTERVAL, PrCache, fetch_pr_summary_index, pr_details_due, refresh_pr_cache,
    refresh_pr_details_cache, refresh_pr_summary_index, remove_pr_cache,
};
use crate::plan::{build_plan_prompt, default_plan_path, infer_total_phases, run_codex_plan};
use crate::process::{run_configured_commands, run_status};
use crate::review::{build_review_fix_prompt, write_review_packet};
use crate::session::{
    append_agent_log, append_runtime_log, clear_hidden, discover_sessions, mark_hidden,
    remove_logs, remove_process_state, remove_task_metadata, save_agent_state, write_task_metadata,
};
use crate::tmux::{
    agent_session_running, attach_or_create_agent, ensure_agent_session, kill_agent_session,
    latest_agent_session_generation, paste_agent_prompt,
};
use crate::tui::{PrPollKey, PrPollResult, TmuxSlotKey, TmuxWarmupKey, TmuxWarmupResult, Tui};
use crate::util::{truncate, yes};

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
            }
        }
        self.sessions = fresh;
        if self.selected >= self.sessions.len() {
            self.selected = self.sessions.len().saturating_sub(1);
        }
        Ok(())
    }

    pub(crate) fn create_session(&mut self) -> Result<(), String> {
        if !self.allow_dirty && worktree_dirty(&self.repo, &self.config)? {
            self.show_message(
                "current worktree is dirty; restart Prism with --allow-dirty to create anyway",
            )?;
            return Ok(());
        }
        let branch = self.prompt_line("Branch name: ")?;
        if branch.trim().is_empty() {
            return Ok(());
        }
        let initial_prompt = self.prompt_line("Initial prompt (optional): ")?;
        self.show_message(&format!("creating worktree for {branch}"))?;
        run_status(
            Command::new(self.config.tool(&self.config.worktree_command))
                .current_dir(&self.repo.root)
                .args(["switch", "-c", branch.trim()]),
        )?;
        clear_hidden(&self.repo, branch.trim())?;
        self.refresh_sessions()?;
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
            write_task_metadata(&self.repo, &self.sessions[index], &initial_prompt)?;
            self.sessions[index].prompt_summary = truncate(&initial_prompt.replace('\n', " "), 50);
            self.sessions[index].adopted = true;
            self.launch_agent(index, &initial_prompt)?;
        }
        Ok(())
    }

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
                    changed |= before != after;
                }
                PrPollResult::Details { key, cache } => {
                    self.pr_polls_in_flight.remove(&key);
                    if let Some(session) = self
                        .sessions
                        .iter_mut()
                        .find(|session| pr_poll_key(session) == key)
                    {
                        let before = pr_render_signature(&session.pr);
                        let current_pr = session.pr.summary.as_ref().map(|summary| summary.number);
                        let result_pr = cache.summary.as_ref().map(|summary| summary.number);
                        if current_pr == result_pr {
                            session.pr.details = cache.details;
                            session.pr.details_last_polled = cache.details_last_polled;
                            session.pr.error = cache.error;
                        }
                        changed |= before != pr_render_signature(&session.pr);
                    }
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

    pub(crate) fn create_or_update_pr(&mut self) -> Result<(), String> {
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
        if let Some(summary) = &self.sessions[self.selected].pr.summary {
            let message = format!("PR #{} {}", summary.number, summary.url);
            self.show_message(&message)?;
            return Ok(());
        }
        let path = self.sessions[self.selected].path.clone();
        let branch = self.sessions[self.selected].branch.clone();

        if selected_dirty(&path, &self.config)? {
            self.show_message("working tree is dirty; commit or stash before creating a PR")?;
            return Ok(());
        }

        run_configured_commands(&self.config.checks.pre_pr, &path, "pre_pr")?;
        run_configured_commands(&self.config.checks.pre_push, &path, "pre_push")?;

        let push = self.prompt_line("No PR found. Push branch and create PR? [y/N] ")?;
        if !yes(&push) {
            return Ok(());
        }
        self.show_message("pushing branch")?;
        run_status(
            Command::new(self.config.tool("git"))
                .arg("-C")
                .arg(&path)
                .args(["push", "-u", "origin", &branch]),
        )?;
        self.show_message("creating pull request")?;
        run_status(
            Command::new(self.config.tool("gh"))
                .arg("pr")
                .arg("create")
                .arg("--fill")
                .current_dir(&path),
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
        if let Some(summary) = &self.sessions[self.selected].pr.summary {
            let message = format!("created PR #{} {}", summary.number, summary.url);
            self.show_message(&message)?;
        }
        Ok(())
    }

    pub(crate) fn refresh_review_packet(&mut self) -> Result<(), String> {
        if self.selected >= self.sessions.len() {
            return Ok(());
        }
        if self
            .config
            .is_default_branch(&self.sessions[self.selected].branch)
        {
            self.show_message("default branch has no PR review packet")?;
            return Ok(());
        }
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
        let path = write_review_packet(&self.sessions[self.selected], &self.config)?;
        self.show_message(&format!("wrote {}", path.display()))?;
        Ok(())
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
        if self.sessions[self.selected].agent_state == AgentState::Running {
            self.show_message("agent is already running; wait or select another session")?;
            return Ok(());
        }
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
        let path = self.sessions[self.selected].path.clone();
        run_configured_commands(&self.config.checks.review_fix, &path, "review_fix")?;
        let prompt = build_review_fix_prompt(&self.sessions[self.selected])?;
        let session = session_for_tmux_warmup(&self.sessions[self.selected]);
        let slot = tmux_slot_key(&session);
        let generation = self.tmux_generation_for_slot(&slot);
        let key = tmux_warmup_key(slot.clone(), generation);
        self.finish_tmux_warmup_for_key(&key);
        paste_agent_prompt(&self.repo, &self.config, &session, generation, &prompt)?;
        let running = agent_session_running(&self.repo, &self.config, &session, generation);
        self.update_tmux_agent_state_for_slot(&slot, running);
        self.show_message("pasted review-fix prompt into agent session")?;
        Ok(())
    }

    pub(crate) fn commit_review_fix(&mut self) -> Result<(), String> {
        if self.selected >= self.sessions.len() {
            return Ok(());
        }
        let path = self.sessions[self.selected].path.clone();
        if !selected_dirty(&path, &self.config)? {
            self.show_message("nothing to commit")?;
            return Ok(());
        }
        let message = self.prompt_line_with_default("Commit message: ", "fix: code review")?;
        let message = message.trim();
        if message.is_empty() {
            return Ok(());
        }
        run_status(
            Command::new(self.config.tool("git"))
                .arg("-C")
                .arg(&path)
                .args(["add", "-A"]),
        )?;
        run_status(
            Command::new(self.config.tool("git"))
                .arg("-C")
                .arg(&path)
                .args(["commit", "-m"])
                .arg(message),
        )?;
        self.refresh_sessions()?;
        self.show_message(&format!("created commit: {message}"))?;
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
            let answer =
                self.prompt_line(&format!("No upstream. Push -u origin {branch}? [y/N] "))?;
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
        self.show_message("pushing branch")?;
        run_status(
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
        self.show_message("push complete")?;
        Ok(())
    }

    pub(crate) fn create_plan(&mut self) -> Result<(), String> {
        if self.selected >= self.sessions.len() {
            return Ok(());
        }
        if self.sessions[self.selected].agent_state == AgentState::Running {
            self.show_message("agent is already running; wait or select another session")?;
            return Ok(());
        }
        let path = default_plan_path(&self.sessions[self.selected], &self.config);
        let request = self.prompt_line("Plan request (optional): ")?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| format!("create plan dir: {error}"))?;
        }
        let prompt = build_plan_prompt(&self.sessions[self.selected], &path, &request);
        self.launch_agent(self.selected, &prompt)?;
        self.show_message(&format!("started planning agent for {}", path.display()))?;
        Ok(())
    }

    pub(crate) fn run_selected_plan(&mut self) -> Result<(), String> {
        if self.selected >= self.sessions.len() {
            return Ok(());
        }
        let session = &self.sessions[self.selected];
        let plan_path = default_plan_path(session, &self.config);
        if !plan_path.is_file() {
            return Err(format!("plan file not found: {}", plan_path.display()));
        }
        let inferred_total = infer_total_phases(&plan_path)?;
        let total = if inferred_total > 0 {
            inferred_total
        } else {
            let input = self.prompt_line("Total phases: ")?;
            input
                .trim()
                .parse::<usize>()
                .map_err(|_| "total phases must be a positive integer".to_string())?
        };
        if total == 0 {
            return Err("total phases must be positive".to_string());
        }
        let start_input = self.prompt_line("Start phase [1]: ")?;
        let start = if start_input.trim().is_empty() {
            1
        } else {
            start_input
                .trim()
                .parse::<usize>()
                .map_err(|_| "start phase must be a positive integer".to_string())?
        };
        if start == 0 || start > total {
            return Err("start phase must be between 1 and total phases".to_string());
        }
        let parallel_input = self.prompt_line("Run phases in parallel? [y/N] ")?;
        let parallel = matches!(
            parallel_input.trim(),
            "y" | "Y" | "yes" | "YES" | "true" | "TRUE"
        );
        let answer = self.prompt_line(&format!(
            "Run {} phases from {} starting at {}? [y/N] ",
            total,
            plan_path.display(),
            start
        ))?;
        if !yes(&answer) {
            return Ok(());
        }
        run_codex_plan(session, &self.config, &plan_path, total, start, parallel)
    }

    pub(crate) fn remove_session_from_board(&mut self) -> Result<(), String> {
        if self.selected >= self.sessions.len() {
            return Ok(());
        }
        let branch = self.sessions[self.selected].branch.clone();
        let answer = self.prompt_line(&format!("Remove {branch} from Prism board only? [y/N] "))?;
        if !yes(&answer) {
            return Ok(());
        }
        mark_hidden(&self.repo, &branch)?;
        self.refresh_sessions()?;
        self.show_message("session removed from board")?;
        Ok(())
    }

    pub(crate) fn delete_session(&mut self) -> Result<(), String> {
        if self.selected >= self.sessions.len() {
            return Ok(());
        }
        let branch = self.sessions[self.selected].branch.clone();
        let path = self.sessions[self.selected].path.clone();
        let adopted = self.sessions[self.selected].adopted;
        let warning = if adopted {
            "Delete local Prism data, worktree, and local branch? [y/N] "
        } else {
            "Untracked worktree. Type Y to delete worktree and local branch; y hides only: "
        };
        let answer = self.prompt_line(warning)?;
        if answer.trim() == "Y" {
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
        } else if !adopted && yes(&answer) {
            mark_hidden(&self.repo, &branch)?;
            self.refresh_sessions()?;
            self.show_message("untracked session hidden")?;
        }
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
    session.branch != "(detached)" && !config.is_default_branch(&session.branch)
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
    }
}

#[cfg(test)]
mod tests {
    use crate::agent::AgentState;
    use crate::config::{Checks, Config, EscapeKey};
    use crate::github::PrCache;
    use crate::repo::Repository;
    use crate::session::Session;
    use crate::tui::{TmuxWarmupResult, Tui};

    use super::{tmux_agent_state, tmux_slot_key, tmux_warmup_key};
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
            tools: BTreeMap::new(),
            agent_commands: BTreeMap::new(),
            agent_prompt_modes: BTreeMap::new(),
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
