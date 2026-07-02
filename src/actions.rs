use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use serde_json::Value;

use crate::agent::AgentState;
use crate::agent_session::{AgentSessionWarmupKey, AgentSessionWarmupResult};
use crate::auto_flow::{
    AutoExecutorConfig, AutoImplementationSource, AutoLaunch, AutoLaunchOptions, AutoRunMode,
    AutoRunStatus, AutoStepKey, AutoStepStatus, PersistedAutoRun, abort_auto_step,
    archive_auto_run, execute_auto_initial_step, load_auto_run, prepare_auto_run_for_resume,
    request_auto_run_pause, resume_paused_auto_run, retry_auto_from_step,
    retry_failed_auto_step as retry_auto_failed_step, save_auto_run,
};
use crate::ci::build_ci_failure_prompt;
use crate::config::Config;
use crate::git::{branch_behind, git_status_label, has_upstream, pull_branch, selected_dirty};
use crate::github::{
    PR_SUMMARY_POLL_INTERVAL, PrCacheRepository, apply_pr_details_poll_result,
    fetch_pr_summary_index, pr_cache_comment_count, pr_cache_pollable, pr_cache_render_signature,
    pr_details_pollable, pr_summary_or_error, refresh_pr_cache, refresh_pr_details_cache,
    refresh_pr_summary_index_for_sessions, wait_for_pr_merged,
};
use crate::json::{json_bool_field, json_object_field, json_string_field, json_top_level_objects};
use crate::lifecycle::{
    create_pull_request, create_worktree_session, delete_worktree_session, merge_pull_request,
    push_branch, refresh_branch_pr_cache, run_pre_pr_checks, run_pre_push_checks,
};
use crate::opencode::{self, OpencodeStatus, load_runtime};
use crate::plan::{PlanExecution, infer_total_phases, open_plan_mode, select_plan_path};
use crate::plan_run::{
    DEFAULT_OUTPUT_LINES_PER_STEP, PlanExecutorConfig, PlanRunMode, PlanRunStatus, PlanStepStatus,
    abort_plan_run, abort_plan_step, archive_plan_run, execute_plan_parallel,
    execute_plan_sequential, load_plan_run, load_resumable_plan_run, prepare_plan_plugin_config,
    prepare_plan_run_for_resume, request_plan_run_pause, resume_paused_plan_run,
    retry_failed_steps, retry_from_step, save_plan_run, skip_plan_step,
};
use crate::process::{command_exists, run_capture};
use crate::repo::Repository;
use crate::review::build_review_fix_prompt;
use crate::session::{
    append_runtime_log, archive_worktree_session, discover_sessions, save_agent_state,
    write_task_metadata, write_task_summary_metadata,
};
use crate::tmux::TmuxWindow;
use crate::tui::{
    DEFAULT_BRANCH_AGENT_MESSAGE, DefaultBranchPollResult, ManagedRepo, OpencodeEventResult,
    OpencodePollKey, OpencodePollResult, PlanRunResult, PrPollKey, PrPollResult, Tui, WtPollResult,
};

enum AutoStartupSource {
    Prompt,
    ExistingPlan,
    DraftPlan,
}

fn validate_existing_auto_plan(plan_path: &Path) -> Result<(), String> {
    if !plan_path.is_file() {
        return Err(format!("plan file not found: {}", plan_path.display()));
    }
    if infer_total_phases(plan_path)? == 0 {
        return Err("could not infer phases; add headings like 'Phase 1'".to_string());
    }
    Ok(())
}

fn next_auto_step_description(run: &PersistedAutoRun) -> Option<String> {
    let step = run.steps.iter().find(|step| {
        step.status == AutoStepStatus::Queued
            || matches!(step.status, AutoStepStatus::Waiting)
                && matches!(step.step_key, AutoStepKey::RunPlan)
    })?;
    let detail = step.summary.as_deref().or(step.reason.as_deref());
    Some(match detail {
        Some(detail) if !detail.trim().is_empty() => {
            format!("#{} {} ({})", step.sequence, step.step_key.as_str(), detail)
        }
        _ => format!("#{} {}", step.sequence, step.step_key.as_str()),
    })
}
use crate::util::{status_count, yes};

const DEFAULT_BRANCH_STATUS_POLL_INTERVAL: Duration = Duration::from_secs(60);
const BACKGROUND_PR_SUMMARY_POLL_INTERVAL: Duration = Duration::from_secs(60);
const SELECTED_OPENCODE_STATUS_POLL_INTERVAL: Duration = Duration::from_secs(2);
const VISIBLE_OPENCODE_STATUS_POLL_INTERVAL: Duration = Duration::from_secs(5);
const OPENCODE_SSE_RECONNECT_INITIAL: Duration = Duration::from_millis(500);
const OPENCODE_SSE_RECONNECT_MAX: Duration = Duration::from_secs(10);

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
        create_worktree_session(&context.repo, &context.config, branch.trim())?;
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
            self.paste_prompt_into_tmux_agent(index, &initial_prompt)?;
            self.show_message("pasted initial prompt into agent session")?;
        }
        Ok(true)
    }

    fn ensure_default_branch_ready_for_create(
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
        let answer = self.prompt_line_dialog(
            raw,
            "Default Branch Behind",
            &format!("{base} is behind origin/{base} by {behind}. Pull first? [Y/n] "),
            "",
        )?;
        if answer.as_deref().map(yes_default).unwrap_or(false) {
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

    fn default_branch_path_for_repo(&self, repo_index: usize, base: &str) -> PathBuf {
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

    pub(crate) fn edit_config(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let context = self
            .selected_repo_context()
            .ok_or_else(|| "no selected repository".to_string())?;
        ensure_repo_config_file(&context.config.repo_config_path, false)?;
        let editor =
            editor_command().ok_or_else(|| "no editor found; set VISUAL or EDITOR".to_string())?;
        raw.suspend()?;
        let result = Command::new(&editor)
            .arg(&context.config.repo_config_path)
            .status();
        let resume_result = raw.resume();
        resume_result?;
        let status = result.map_err(|error| format!("{editor}: {error}"))?;
        if !status.success() {
            return Err(format!("{editor} exited with {status}"));
        }
        let config = crate::config::Config::load(&context.repo);
        if let Some(repo) = self.repos.get_mut(context.repo_index) {
            repo.config = config.clone();
        }
        self.sync_selected_repo_context();
        self.refresh_sessions()?;
        self.start_tmux_agent_warmup();
        self.start_wt_column_poll();
        self.show_message("config reloaded")?;
        Ok(())
    }

    pub(crate) fn edit_user_config(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let path = self
            .repos
            .get(self.current_repo)
            .map(|repo| repo.config.user_path.clone())
            .ok_or_else(|| "no selected repository".to_string())?;
        ensure_user_config_file(&path)?;
        let editor =
            editor_command().ok_or_else(|| "no editor found; set VISUAL or EDITOR".to_string())?;
        raw.suspend()?;
        let result = Command::new(&editor).arg(&path).status();
        let resume_result = raw.resume();
        resume_result?;
        let status = result.map_err(|error| format!("{editor}: {error}"))?;
        if !status.success() {
            return Err(format!("{editor} exited with {status}"));
        }
        for repo in &mut self.repos {
            repo.config = Config::load(&repo.repo);
        }
        self.sync_selected_repo_context();
        self.refresh_sessions()?;
        self.start_tmux_agent_warmup();
        self.start_wt_column_poll();
        self.show_message("user config reloaded")?;
        Ok(())
    }

    pub(crate) fn edit_worktree_columns(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let context = self
            .selected_repo_context()
            .ok_or_else(|| "no selected repository".to_string())?;
        ensure_repo_config_file(&context.config.repo_config_path, true)?;
        self.edit_config(raw)
    }

    pub(crate) fn add_repository(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let Some(path) = self.prompt_line_dialog(raw, "Add Repository", "Base/main path: ", "")?
        else {
            return Ok(());
        };
        let path = path.trim();
        if path.is_empty() {
            return Ok(());
        }
        let (_, index, entries) = crate::workspace::ensure_repo_entry(Path::new(path))?;
        self.reload_repositories(entries)?;
        self.select_repo(index);
        self.start_tmux_agent_warmup();
        self.start_wt_column_poll();
        self.start_default_branch_status_poll(true);
        self.show_message("repository added")?;
        Ok(())
    }

    pub(crate) fn edit_repositories(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let path = crate::workspace::repos_path();
        if !path.exists() {
            let entries = self
                .repos
                .iter()
                .map(|repo| crate::workspace::RepoEntry {
                    root: repo.repo.root.clone(),
                    key: repo.key,
                })
                .collect::<Vec<_>>();
            crate::workspace::save_entries(&entries)?;
        }
        let editor =
            editor_command().ok_or_else(|| "no editor found; set VISUAL or EDITOR".to_string())?;
        raw.suspend()?;
        let result = Command::new(&editor).arg(&path).status();
        let resume_result = raw.resume();
        resume_result?;
        let status = result.map_err(|error| format!("{editor}: {error}"))?;
        if !status.success() {
            return Err(format!("{editor} exited with {status}"));
        }
        let entries = crate::workspace::load_entries();
        if entries.is_empty() {
            return Err("repository list is empty; add at least one [[repos]] block".to_string());
        }
        let current_root = self
            .selected_repo_context()
            .map(|context| context.repo.root)
            .unwrap_or_else(|| self.repo.root.clone());
        self.reload_repositories(entries)?;
        let index = self
            .repos
            .iter()
            .position(|repo| repo.repo.root == current_root)
            .unwrap_or(0);
        self.select_repo(index);
        self.start_tmux_agent_warmup();
        self.start_wt_column_poll();
        self.start_default_branch_status_poll(true);
        self.show_message("repositories reloaded")?;
        Ok(())
    }

    fn reload_repositories(
        &mut self,
        entries: Vec<crate::workspace::RepoEntry>,
    ) -> Result<(), String> {
        let mut repos = Vec::new();
        for entry in crate::workspace::discover_valid_entries(entries) {
            let repo = entry.repo;
            let config = crate::config::Config::load(&repo);
            repos.push(ManagedRepo::new(repo, config, entry.key));
        }
        self.repos = repos;
        self.current_repo = self.current_repo.min(self.repos.len().saturating_sub(1));
        self.selected_repo_root = self
            .repos
            .get(self.current_repo)
            .map(|repo| repo.repo.root.clone());
        self.refresh_sessions()?;
        self.sync_selected_repo_context();
        Ok(())
    }

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
                    }
                    if let Some(message) = event.latest_message {
                        status.latest_message = Some(message);
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

    fn apply_opencode_status(&mut self, index: usize, status: OpencodeStatus) -> bool {
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

    pub(crate) fn poll_pull_requests(&mut self, force: bool) -> bool {
        let changed = self.drain_pr_poll_results();
        for repo_index in 0..self.repos.len() {
            let Some(managed) = self.repos.get(repo_index) else {
                continue;
            };
            let interval = if repo_index == self.current_repo {
                PR_SUMMARY_POLL_INTERVAL
            } else {
                BACKGROUND_PR_SUMMARY_POLL_INTERVAL
            };
            let summaries_due = managed
                .pr_summary_last_polled
                .map(|last| last.elapsed() >= interval)
                .unwrap_or(true);
            let has_pr_branches = self.sessions.iter().any(|session| {
                session.repo_index == repo_index
                    && pr_cache_pollable(&managed.config, &session.branch, &session.pr)
            });
            if has_pr_branches && (force || summaries_due) && !managed.pr_summary_poll_in_flight {
                let path = managed.repo.root.clone();
                let config = managed.config.clone();
                let tx = self.pr_poll_tx.clone();
                let poll_started_at = std::time::Instant::now();
                if let Some(managed) = self.repos.get_mut(repo_index) {
                    managed.pr_summary_last_polled = Some(poll_started_at);
                    managed.pr_summary_poll_in_flight = true;
                }
                std::thread::spawn(move || {
                    let summaries = fetch_pr_summary_index(&path, &config);
                    let _ = tx.send(PrPollResult::Summary {
                        repo_index,
                        summaries,
                        poll_started_at,
                    });
                });
            }
        }

        let selected = self.selected_worktree_index();
        if let Some(session) = selected.and_then(|index| self.sessions.get_mut(index)) {
            let Some(managed) = self.repos.get(session.repo_index) else {
                return changed;
            };
            let key = pr_poll_key(session);
            if pr_details_pollable(&managed.config, &session.branch, &session.pr)
                && !self.pr_polls_in_flight.contains(&key)
            {
                let config = managed.config.clone();
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
        let selected = self.selected_worktree_index();
        while let Ok(result) = self.pr_poll_rx.try_recv() {
            match result {
                PrPollResult::Summary {
                    repo_index,
                    summaries,
                    poll_started_at,
                } => {
                    if let Some(repo) = self.repos.get_mut(repo_index) {
                        repo.pr_summary_poll_in_flight = false;
                    }
                    let before = self
                        .sessions
                        .iter()
                        .map(|session| pr_cache_render_signature(&session.pr))
                        .collect::<Vec<_>>();
                    let before_comments = self
                        .sessions
                        .iter()
                        .map(|session| pr_cache_comment_count(&session.pr))
                        .collect::<Vec<_>>();
                    match summaries {
                        Ok(summaries) => {
                            let repos = self
                                .repos
                                .iter()
                                .map(|managed| PrCacheRepository {
                                    repo: &managed.repo,
                                    config: &managed.config,
                                })
                                .collect::<Vec<_>>();
                            refresh_pr_summary_index_for_sessions(
                                &repos,
                                &mut self.sessions,
                                repo_index,
                                summaries,
                                poll_started_at,
                            );
                        }
                        Err(error) => {
                            for session in &mut self.sessions {
                                if session.repo_index == repo_index {
                                    session.pr.error = Some(error.clone());
                                }
                            }
                        }
                    }
                    let after = self
                        .sessions
                        .iter()
                        .map(|session| pr_cache_render_signature(&session.pr))
                        .collect::<Vec<_>>();
                    for (index, session) in self.sessions.iter_mut().enumerate() {
                        let before = before_comments.get(index).copied().unwrap_or(0);
                        let after = pr_cache_comment_count(&session.pr);
                        if after > before && Some(index) != selected {
                            session.unseen_comments = true;
                        }
                    }
                    changed |= before != after;
                }
                PrPollResult::Details { key, cache } => {
                    self.pr_polls_in_flight.remove(&key);
                    let selected_key =
                        selected.and_then(|index| self.sessions.get(index).map(pr_poll_key));
                    if let Some(session) = self
                        .sessions
                        .iter_mut()
                        .find(|session| pr_poll_key(session) == key)
                    {
                        let before = pr_cache_render_signature(&session.pr);
                        let before_comments = pr_cache_comment_count(&session.pr);
                        if let Some(repo) = self.repos.get(session.repo_index)
                            && apply_pr_details_poll_result(
                                &repo.repo,
                                &session.branch,
                                &mut session.pr,
                                *cache,
                            )
                            && pr_cache_comment_count(&session.pr) > before_comments
                            && selected_key.as_ref() != Some(&key)
                        {
                            session.unseen_comments = true;
                        }
                        changed |= before != pr_cache_render_signature(&session.pr);
                    }
                }
            }
        }
        changed
    }

    pub(crate) fn start_wt_column_poll(&mut self) {
        self.poll_wt_columns();
        for repo_index in 0..self.repos.len() {
            let Some(managed) = self.repos.get(repo_index) else {
                continue;
            };
            if managed.wt_poll_in_flight || managed.config.worktree_columns.is_empty() {
                continue;
            }
            let repo = managed.repo.clone();
            let config = managed.config.clone();
            let tx = self.wt_poll_tx.clone();
            if let Some(managed) = self.repos.get_mut(repo_index) {
                managed.wt_poll_in_flight = true;
            }
            std::thread::spawn(move || {
                let columns = fetch_wt_columns(&repo, &config);
                let _ = tx.send(WtPollResult {
                    repo_index,
                    columns,
                });
            });
        }
    }

    pub(crate) fn poll_wt_columns(&mut self) -> bool {
        let mut changed = false;
        while let Ok(result) = self.wt_poll_rx.try_recv() {
            if let Some(repo) = self.repos.get_mut(result.repo_index) {
                repo.wt_poll_in_flight = false;
            }
            match result.columns {
                Ok(columns_by_path) => {
                    for session in &mut self.sessions {
                        if session.repo_index != result.repo_index {
                            continue;
                        }
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
                    if let Some(repo) = self.repos.get(result.repo_index) {
                        let _ = append_runtime_log(
                            &repo.repo,
                            &format!("wt column refresh failed: {error}"),
                        );
                    }
                }
            }
        }
        changed
    }

    pub(crate) fn start_default_branch_status_poll(&mut self, force: bool) {
        self.poll_default_branch_status();
        for repo_index in 0..self.repos.len() {
            let Some(managed) = self.repos.get(repo_index) else {
                continue;
            };
            if managed.default_branch_poll_in_flight {
                continue;
            }
            let due = managed
                .default_branch_last_polled
                .map(|last| last.elapsed() >= DEFAULT_BRANCH_STATUS_POLL_INTERVAL)
                .unwrap_or(true);
            if !force && !due {
                continue;
            }
            let Some(branch) = managed
                .config
                .default_base
                .as_deref()
                .map(str::trim)
                .filter(|branch| !branch.is_empty())
                .map(str::to_string)
            else {
                continue;
            };
            let path = self.default_branch_path_for_repo(repo_index, &branch);
            let config = managed.config.clone();
            let tx = self.default_branch_poll_tx.clone();
            if let Some(managed) = self.repos.get_mut(repo_index) {
                managed.default_branch_poll_in_flight = true;
                managed.default_branch_last_polled = Some(std::time::Instant::now());
            }
            std::thread::spawn(move || {
                let status_label = default_branch_status_label(&path, &branch, &config);
                let _ = tx.send(DefaultBranchPollResult {
                    repo_index,
                    branch,
                    path,
                    status_label,
                });
            });
        }
    }

    pub(crate) fn poll_default_branch_status(&mut self) -> bool {
        let mut changed = false;
        while let Ok(result) = self.default_branch_poll_rx.try_recv() {
            if let Some(repo) = self.repos.get_mut(result.repo_index) {
                repo.default_branch_poll_in_flight = false;
            }
            match result.status_label {
                Ok(status_label) => {
                    if let Some(session) = self.sessions.iter_mut().find(|session| {
                        session.repo_index == result.repo_index
                            && session.branch == result.branch
                            && session.path == result.path
                    }) && session.status_label != status_label
                    {
                        session.status_label = status_label;
                        changed = true;
                    }
                }
                Err(error) => {
                    if let Some(repo) = self.repos.get(result.repo_index) {
                        let _ = append_runtime_log(
                            &repo.repo,
                            &format!("default branch status refresh failed: {error}"),
                        );
                    }
                }
            }
        }
        changed
    }

    pub(crate) fn attach_selected_tmux_session(&mut self) -> Result<(), String> {
        let Some(context) = self.selected_worktree_context() else {
            return Ok(());
        };
        if self.sessions[context.session_index].is_default_branch(&context.config) {
            self.show_message(DEFAULT_BRANCH_AGENT_MESSAGE)?;
            return Ok(());
        }
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

    fn attach_tmux_window_for_session_index(
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

    pub(crate) fn open_selected_repo_lazygit(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let context = self
            .selected_repo_context()
            .ok_or_else(|| "no selected repository".to_string())?;
        raw.suspend()?;
        let result = Command::new(context.config.tool("lazygit"))
            .current_dir(&context.repo.root)
            .status();
        let resume_result = raw.resume();
        resume_result?;
        let status = result.map_err(|error| format!("lazygit: {error}"))?;
        if !status.success() {
            return Err(format!("lazygit exited with {status}"));
        }
        self.show_message("returned from repository lazygit")?;
        Ok(())
    }

    pub(crate) fn open_selected_repo_terminal(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let context = self
            .selected_repo_context()
            .ok_or_else(|| "no selected repository".to_string())?;
        let shell = std::env::var("SHELL")
            .ok()
            .filter(|shell| !shell.trim().is_empty())
            .unwrap_or_else(|| "/bin/sh".to_string());
        raw.suspend()?;
        let result = Command::new(&shell)
            .current_dir(&context.repo.root)
            .status();
        let resume_result = raw.resume();
        resume_result?;
        let status = result.map_err(|error| format!("{shell}: {error}"))?;
        if !status.success() {
            return Err(format!("{shell} exited with {status}"));
        }
        self.show_message("returned from repository terminal")?;
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn open_selected_repo_plan_mode(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let context = self
            .selected_repo_context()
            .ok_or_else(|| "no selected repository".to_string())?;
        let root = context.repo.root.clone();
        let config = context.config.clone();
        raw.suspend()?;
        let result = open_plan_mode(&config, &root);
        let resume_result = raw.resume();
        self.refresh_sessions()?;
        self.start_tmux_agent_warmup();
        self.start_wt_column_poll();
        resume_result?;
        result?;
        self.show_message("returned from plan mode")?;
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn open_selected_worktree_plan_mode(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let Some(context) = self.selected_worktree_context() else {
            return Ok(());
        };
        let path = self.sessions[context.session_index].path.clone();
        let config = context.config.clone();
        raw.suspend()?;
        let result = open_plan_mode(&config, &path);
        let resume_result = raw.resume();
        self.refresh_sessions()?;
        self.start_tmux_agent_warmup();
        self.start_wt_column_poll();
        resume_result?;
        result?;
        self.show_message("returned from plan mode")?;
        Ok(())
    }

    pub(crate) fn start_selected_repo_plan_run(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let context = self
            .selected_repo_context()
            .ok_or_else(|| "no selected repository".to_string())?;
        self.start_plan_run_for_scope(
            raw,
            context.repo.clone(),
            context.config.clone(),
            context.repo.root,
        )
    }

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

    fn start_plan_run_for_scope(
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
        self.focus_status();
        if should_execute {
            self.show_message("started plan run")?;
        } else {
            self.show_message("plan run is already running")?;
        }
        Ok(())
    }

    fn spawn_plan_run_executor(
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
                scope_path,
                run_id,
                result,
            });
        });
    }

    pub(crate) fn start_or_focus_selected_auto_run(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let Some(context) = self.selected_worktree_context() else {
            return Ok(());
        };
        let session_path = self.sessions[context.session_index].path.clone();
        let session_branch = self.sessions[context.session_index].branch.clone();
        if let Some(run_id) = self.active_auto_runs.get(&session_path).cloned() {
            self.load_auto_run_snapshot(&context.repo.root, &run_id);
            self.selected_auto_run = Some(run_id);
            self.focus_status();
            self.show_message("focused Auto Flow run")?;
            return Ok(());
        }
        let is_detached = session_branch == "(detached)";
        if context.config.is_default_branch(&session_branch) || is_detached {
            return if is_detached {
                Err("Auto Flow cannot start on a detached worktree".to_string())
            } else {
                Err("Auto Flow cannot start on the default branch".to_string())
            };
        }
        if selected_dirty(&session_path, &context.config)? {
            return Err("Auto Flow requires a clean worktree at launch".to_string());
        }
        let Some(source) = self.prompt_auto_implementation_source(raw)? else {
            return Ok(());
        };
        let (mode, implementation_source, plan_path, plan_run_mode, variant, prompt) = match source
        {
            AutoStartupSource::Prompt => {
                let Some(prompt) =
                    self.prompt_line_dialog(raw, "Auto Flow", "Initial prompt: ", "")?
                else {
                    return Ok(());
                };
                if prompt.trim().is_empty() {
                    return Ok(());
                }
                (
                    AutoRunMode::Standard,
                    AutoImplementationSource::Prompt,
                    None,
                    PlanRunMode::Sequential,
                    "default".to_string(),
                    prompt.trim().to_string(),
                )
            }
            AutoStartupSource::ExistingPlan => {
                raw.suspend()?;
                let selected = select_plan_path(&session_path, &context.config);
                let resume_result = raw.resume();
                resume_result?;
                let plan_path = selected?;
                validate_existing_auto_plan(&plan_path)?;
                let Some(plan_run_mode) = self.prompt_auto_plan_run_mode(raw)? else {
                    return Ok(());
                };
                (
                    AutoRunMode::Standard,
                    AutoImplementationSource::ExistingPlan,
                    Some(plan_path.clone()),
                    plan_run_mode,
                    "plan".to_string(),
                    format!("Run plan phases from {}", plan_path.display()),
                )
            }
            AutoStartupSource::DraftPlan => {
                let plan_path = session_path.join("plan.md");
                if plan_path.exists() {
                    return Err(
                            "worktree/plan.md already exists; choose existing-plan mode or move/remove the file"
                                .to_string(),
                        );
                }
                let Some(prompt) =
                    self.prompt_line_dialog(raw, "Auto Flow", "Task prompt: ", "")?
                else {
                    return Ok(());
                };
                if prompt.trim().is_empty() {
                    return Ok(());
                }
                let Some(plan_run_mode) = self.prompt_auto_plan_run_mode(raw)? else {
                    return Ok(());
                };
                (
                    AutoRunMode::PlanFirst,
                    AutoImplementationSource::DraftPlan,
                    Some(plan_path),
                    plan_run_mode,
                    "draft-plan".to_string(),
                    prompt.trim().to_string(),
                )
            }
        };
        let launch = AutoLaunch::with_options(
            &context.repo.root,
            &session_path,
            AutoLaunchOptions {
                branch: session_branch,
                mode,
                implementation_source,
                plan_path,
                plan_run_mode,
                variant,
                agent_profile: None,
                initial_prompt: prompt,
            },
        )?;
        let mut persisted = launch.create_run();
        crate::observability::with_writable_db(&context.repo, |conn| {
            save_auto_run(conn, &mut persisted)
        })?;
        let run_id = persisted.run.id.clone();
        self.remember_auto_run(persisted.clone());
        self.selected_auto_run = Some(run_id);
        self.spawn_auto_run_executor(context.repo, context.config, persisted);
        self.focus_status();
        self.show_message("started Auto Flow run")?;
        Ok(())
    }

    fn prompt_auto_implementation_source(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<Option<AutoStartupSource>, String> {
        loop {
            let Some(answer) = self.prompt_line_dialog(
                raw,
                "Auto Flow",
                "Implementation source [p]rompt/[e]xisting plan/[d]raft plan: ",
                "p",
            )?
            else {
                return Ok(None);
            };
            match answer.trim().to_ascii_lowercase().as_str() {
                "" | "p" | "prompt" => return Ok(Some(AutoStartupSource::Prompt)),
                "e" | "existing" | "existing plan" | "plan" | "file plan" | "plan file" => {
                    return Ok(Some(AutoStartupSource::ExistingPlan));
                }
                "d" | "draft" | "draft plan" => return Ok(Some(AutoStartupSource::DraftPlan)),
                _ => self.show_message("choose prompt, existing plan, or draft plan")?,
            }
        }
    }

    fn prompt_auto_plan_run_mode(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<Option<PlanRunMode>, String> {
        loop {
            let Some(answer) = self.prompt_line_dialog(
                raw,
                "Auto Flow",
                "Plan execution [s]equential/[p]arallel: ",
                "s",
            )?
            else {
                return Ok(None);
            };
            match answer.trim().to_ascii_lowercase().as_str() {
                "" | "s" | "sequential" => return Ok(Some(PlanRunMode::Sequential)),
                "p" | "parallel" => return Ok(Some(PlanRunMode::Parallel)),
                _ => self.show_message("choose sequential or parallel plan execution")?,
            }
        }
    }

    fn spawn_auto_run_executor(
        &self,
        repo: crate::repo::Repository,
        config: crate::config::Config,
        mut persisted: crate::auto_flow::PersistedAutoRun,
    ) {
        thread::spawn(move || {
            let worktree_path = persisted.run.worktree_path.clone();
            let server_url = crate::opencode::ensure_opencode_server(
                &repo,
                &config,
                &persisted.run.branch,
                &worktree_path,
            )
            .ok()
            .map(|runtime| runtime.server_url);
            let executor = AutoExecutorConfig::new(
                config.tool("opencode"),
                server_url,
                worktree_path,
                format!("Auto Flow {}", persisted.run.prompt_summary),
            );
            let _ = crate::observability::with_writable_db(&repo, |conn| {
                execute_auto_initial_step(
                    conn,
                    &repo,
                    &config,
                    &mut persisted,
                    &executor,
                    &mut io::sink(),
                )
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

    fn current_tmux_plan_run(
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

    pub(crate) fn abort_selected_auto_run_or_step(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<bool, String> {
        let Some(dashboard) = self.current_auto_dashboard() else {
            return Ok(false);
        };
        let answer = self.prompt_line_dialog(
            raw,
            "Abort Auto Flow",
            "Abort selected step? Use 'all' for the whole run. [y/N/all] ",
            "",
        )?;
        let Some(answer) = answer else {
            return Ok(true);
        };
        if !answer.trim().eq_ignore_ascii_case("all") && !yes(&answer) {
            return Ok(true);
        }
        let repo = Repository {
            root: PathBuf::from(&dashboard.run.run.repo_root),
        };
        let run_id = dashboard.run.run.id.clone();
        crate::observability::with_writable_db(&repo, |conn| {
            let mut run = load_auto_run(conn, &run_id)?
                .ok_or_else(|| format!("auto flow run not found: {run_id}"))?;
            if answer.trim().eq_ignore_ascii_case("all") {
                for step in &mut run.steps {
                    if matches!(
                        step.status,
                        AutoStepStatus::Queued
                            | AutoStepStatus::Starting
                            | AutoStepStatus::Running
                            | AutoStepStatus::Waiting
                    ) {
                        if matches!(
                            step.status,
                            AutoStepStatus::Starting | AutoStepStatus::Running
                        ) {
                            let _ = abort_auto_step(conn, step);
                        } else {
                            step.status = AutoStepStatus::Aborted;
                            step.finished_unix_ms = Some(crate::auto_flow::unix_ms());
                        }
                    }
                }
                run.run.status = AutoRunStatus::Aborted;
                run.run.pause_requested = false;
            } else {
                let selected = run
                    .run
                    .selected_step_run_id
                    .or_else(|| run.steps.first().and_then(|step| step.id))
                    .ok_or_else(|| "auto flow run has no selected step".to_string())?;
                let step = run
                    .steps
                    .iter_mut()
                    .find(|step| step.id == Some(selected))
                    .ok_or_else(|| format!("auto flow step not found: {selected}"))?;
                if matches!(
                    step.status,
                    AutoStepStatus::Starting | AutoStepStatus::Running
                ) {
                    abort_auto_step(conn, step)?;
                } else {
                    step.status = AutoStepStatus::Aborted;
                    step.finished_unix_ms = Some(crate::auto_flow::unix_ms());
                }
                run.run.status = run.aggregate_status();
            }
            save_auto_run(conn, &mut run)
        })?;
        self.load_auto_run_snapshot(&repo.root, &run_id);
        self.show_message("abort recorded for Auto Flow")?;
        Ok(true)
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

    fn show_standalone_plan_actions_dialog(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        if self.current_plan_dashboard().is_none() {
            self.show_message("focus an Auto Flow or plan run to show plan actions")?;
            return Ok(());
        }

        let answer = self
            .prompt_choice_dialog(raw, Self::plan_action_choices("Plan Actions", "skip phase"))?;
        let Some(answer) = answer else {
            return Ok(());
        };
        match answer.trim().to_ascii_lowercase().as_str() {
            "" => Ok(()),
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

    fn show_auto_plan_actions_dialog(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let answer = self.prompt_choice_dialog(
            raw,
            Self::plan_action_choices("Auto Plan Actions", "skip linked phase"),
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

    fn plan_action_choices(title: &str, skip_label: &str) -> crate::view::ChoiceList {
        let choices = [
            ("u", "pause/resume"),
            ("f", "retry failed"),
            ("b", "retry from selected"),
            ("s", skip_label),
            ("x", "abort"),
        ];
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

    pub(crate) fn retry_failed_auto_step(&mut self) -> Result<bool, String> {
        let Some(dashboard) = self.current_auto_dashboard() else {
            return Ok(false);
        };
        let repo = Repository {
            root: PathBuf::from(&dashboard.run.run.repo_root),
        };
        let config = Config::load(&repo);
        let run_id = dashboard.run.run.id.clone();
        let persisted = crate::observability::with_writable_db(&repo, |conn| {
            let mut run = load_auto_run(conn, &run_id)?
                .ok_or_else(|| format!("auto flow run not found: {run_id}"))?;
            retry_auto_failed_step(conn, &mut run)?;
            Ok(run)
        })?;
        self.remember_auto_run(persisted.clone());
        self.spawn_auto_run_executor(repo, config, persisted);
        self.show_message("retrying Auto Flow step")?;
        Ok(true)
    }

    pub(crate) fn retry_auto_from_selected_step(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<bool, String> {
        let Some(dashboard) = self.current_auto_dashboard() else {
            return Ok(false);
        };
        let selected = dashboard
            .run
            .run
            .selected_step_run_id
            .or_else(|| dashboard.run.steps.first().and_then(|step| step.id));
        let Some(selected) = selected else {
            return Ok(true);
        };
        let answer = self.prompt_line_dialog(
            raw,
            "Retry Auto Flow",
            "Retry from selected step? [y/N] ",
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
            let mut run = load_auto_run(conn, &run_id)?
                .ok_or_else(|| format!("auto flow run not found: {run_id}"))?;
            retry_auto_from_step(conn, &mut run, selected)?;
            Ok(run)
        })?;
        self.remember_auto_run(persisted.clone());
        self.spawn_auto_run_executor(repo, config, persisted);
        self.show_message("retrying Auto Flow from selected step")?;
        Ok(true)
    }

    pub(crate) fn toggle_selected_auto_pause(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<bool, String> {
        let Some(dashboard) = self.current_auto_dashboard() else {
            return Ok(false);
        };
        let repo = Repository {
            root: PathBuf::from(&dashboard.run.run.repo_root),
        };
        let config = Config::load(&repo);
        let run_id = dashboard.run.run.id.clone();
        let mut should_execute = false;
        let resuming =
            dashboard.run.run.pause_requested || dashboard.run.run.status == AutoRunStatus::Paused;
        if resuming && !self.confirm_resume_auto_step(raw, &dashboard.run)? {
            self.show_message("Auto Flow resume cancelled")?;
            return Ok(true);
        }
        let persisted = crate::observability::with_writable_db(&repo, |conn| {
            let mut run = load_auto_run(conn, &run_id)?
                .ok_or_else(|| format!("auto flow run not found: {run_id}"))?;
            if run.run.pause_requested || run.run.status == AutoRunStatus::Paused {
                resume_paused_auto_run(conn, &mut run)?;
                should_execute =
                    prepare_auto_run_for_resume(conn, &mut run, DEFAULT_OUTPUT_LINES_PER_STEP)?;
            } else {
                request_auto_run_pause(conn, &mut run)?;
            }
            Ok(run)
        })?;
        self.remember_auto_run(persisted.clone());
        if persisted.run.pause_requested || persisted.run.status == AutoRunStatus::Paused {
            self.show_message("Auto Flow will pause before the next step")?;
        } else {
            if should_execute {
                self.spawn_auto_run_executor(repo, config, persisted);
                self.show_message("resumed Auto Flow run")?;
            } else {
                self.show_message("Auto Flow has no queued agent step")?;
            }
        }
        Ok(true)
    }

    fn confirm_resume_auto_step(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
        run: &PersistedAutoRun,
    ) -> Result<bool, String> {
        let description = next_auto_step_description(run)
            .unwrap_or_else(|| "determine the next Auto Flow step".to_string());
        let answer = self.prompt_line_dialog(
            raw,
            "Resume Auto Flow",
            &format!("Next: {description}. Continue? [y/N] "),
            "",
        )?;
        Ok(answer.as_deref().map(yes).unwrap_or(false))
    }

    pub(crate) fn dismiss_selected_auto_run(&mut self) -> Result<bool, String> {
        let Some(dashboard) = self.current_auto_dashboard() else {
            return Ok(false);
        };
        let repo = Repository {
            root: PathBuf::from(&dashboard.run.run.repo_root),
        };
        let run_id = dashboard.run.run.id.clone();
        crate::observability::with_writable_db(&repo, |conn| {
            let mut run = load_auto_run(conn, &run_id)?
                .ok_or_else(|| format!("auto flow run not found: {run_id}"))?;
            archive_auto_run(conn, &mut run)
        })?;
        self.auto_runs.remove(&run_id);
        self.active_auto_runs.retain(|_, active| active != &run_id);
        if self.selected_auto_run.as_deref() == Some(run_id.as_str()) {
            self.selected_auto_run = None;
        }
        self.selected_auto_step_by_run.remove(&run_id);
        self.auto_output_state_by_run.remove(&run_id);
        self.show_message("dismissed Auto Flow run")?;
        Ok(true)
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
        self.show_message("dismissed plan run")?;
        Ok(true)
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

    fn start_tmux_agent_warmup_for_key(&mut self, key: AgentSessionWarmupKey, delay: Duration) {
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

    fn spawn_tmux_warmup_job(&mut self, job: crate::agent_session::AgentSessionWarmupJob) {
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

    fn finish_tmux_warmup_for_key(&mut self, key: &AgentSessionWarmupKey) -> bool {
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

    fn apply_tmux_warmup_result(&mut self, result: AgentSessionWarmupResult) -> bool {
        self.tmux_warmups_in_flight.remove(&result.key);
        crate::agent_session::apply_warmup_result(
            &self.repos,
            &self.repo,
            &mut self.sessions,
            &self.tmux_generations,
            result,
        )
    }

    pub(crate) fn start_review_fix(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        self.show_loading_dialog(
            raw,
            "Review Fix Prompt",
            "Refreshing pull request review details",
        )?;
        self.send_review_fix_prompt()
    }

    fn send_review_fix_prompt(&mut self) -> Result<(), String> {
        let Some(context) = self.selected_worktree_context() else {
            return Ok(());
        };
        let selected = context.session_index;
        if self.sessions[selected].is_default_branch(&context.config) {
            self.show_message("default branch has no PR review comments")?;
            return Ok(());
        }
        {
            let session = &mut self.sessions[selected];
            refresh_branch_pr_cache(
                &context.repo,
                &context.config,
                &session.branch,
                &session.path,
                &mut session.pr,
                true,
            );
        }
        let prompt = build_review_fix_prompt(&self.sessions[selected], &context.config)?;
        self.submit_action_prompt_to_agent(selected, &context.repo, "review fix", &prompt)?;
        self.show_message("review-fix prompt sent to agent session")?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn start_review_fix_for_test(&mut self) -> Result<(), String> {
        self.send_review_fix_prompt()
    }

    pub(crate) fn start_ci_fix(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let Some(context) = self.selected_worktree_context() else {
            return Ok(());
        };
        let selected = context.session_index;
        if self.sessions[selected].is_default_branch(&context.config) {
            self.show_message("default branch has no PR CI failures")?;
            return Ok(());
        }
        self.show_loading_dialog(
            raw,
            "CI Failure Prompt",
            "Refreshing pull request CI details",
        )?;
        self.send_ci_fix_prompt()
    }

    fn send_ci_fix_prompt(&mut self) -> Result<(), String> {
        let Some(context) = self.selected_worktree_context() else {
            return Ok(());
        };
        let selected = context.session_index;
        if self.sessions[selected].is_default_branch(&context.config) {
            self.show_message("default branch has no PR CI failures")?;
            return Ok(());
        }
        {
            let session = &mut self.sessions[selected];
            refresh_branch_pr_cache(
                &context.repo,
                &context.config,
                &session.branch,
                &session.path,
                &mut session.pr,
                true,
            );
        }
        let prompt = build_ci_failure_prompt(&self.sessions[selected], &context.config)?;
        self.submit_action_prompt_to_agent(selected, &context.repo, "ci fix", &prompt)?;
        self.show_message("CI-failure prompt sent to agent session")?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn start_ci_fix_for_test(&mut self) -> Result<(), String> {
        self.send_ci_fix_prompt()
    }

    pub(crate) fn open_selected_pr(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let Some(context) = self.selected_worktree_context() else {
            return Ok(());
        };
        let selected = context.session_index;
        if self.sessions[selected].is_default_branch(&context.config) {
            self.show_message("default branch is not treated as a PR branch")?;
            return Ok(());
        }
        if self.sessions[selected].is_detached() {
            self.show_message("cannot open a PR for a detached worktree")?;
            return Ok(());
        }
        if self.sessions[selected].pr.summary.is_none() {
            self.show_loading_dialog(raw, "Open Pull Request", "Refreshing pull request")?;
            let session = &mut self.sessions[selected];
            refresh_pr_cache(
                &context.repo,
                &session.branch,
                &mut session.pr,
                &session.path,
                &context.config,
                false,
            );
        }
        let Some(summary) = pr_summary_or_error(&self.sessions[selected].pr)? else {
            self.show_message("no pull request found for selected branch")?;
            return Ok(());
        };
        let url = summary.url.trim();
        if url.is_empty() {
            return Err(format!("PR #{} has no URL", summary.number));
        }
        open_url_in_browser(url)?;
        self.show_message(&format!("opened PR #{} in browser", summary.number))?;
        Ok(())
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
        let answer = self.prompt_line_dialog(
            raw,
            "Abort OpenCode",
            &format!("Abort {}? [y/N] ", self.sessions[selected].branch),
            "",
        )?;
        if !answer.as_deref().map(yes).unwrap_or(false) {
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

    fn paste_prompt_into_tmux_agent(&mut self, index: usize, prompt: &str) -> Result<(), String> {
        #[cfg(test)]
        if let Some(submissions) = &mut self.prompt_submissions {
            submissions.push((index, prompt.to_string()));
            return Ok(());
        }

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
        let use_ =
            crate::agent_session::session_use(&self.repos, &mut self.tmux_generations, &session);
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

    fn submit_action_prompt_to_agent(
        &mut self,
        index: usize,
        repo: &crate::repo::Repository,
        summary: &str,
        prompt: &str,
    ) -> Result<(), String> {
        self.paste_prompt_into_tmux_agent(index, prompt)
            .map_err(|error| format!("send {summary} prompt to agent session: {error}"))?;
        write_task_summary_metadata(repo, &self.sessions[index], summary)?;
        self.sessions[index].mark_adopted_with_summary(summary);
        Ok(())
    }

    pub(crate) fn push_selected_branch(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let Some(context) = self.selected_worktree_context() else {
            return Ok(());
        };
        let selected = context.session_index;
        let path = self.sessions[selected].path.clone();
        let branch = self.sessions[selected].branch.clone();
        if self.sessions[selected].is_default_branch(&context.config) {
            self.show_message("default branch is not treated as a PR branch")?;
            return Ok(());
        }
        if self.sessions[selected].is_detached() {
            self.show_message("cannot push a detached worktree")?;
            return Ok(());
        }
        run_pre_push_checks(&context.config, &path)?;
        let set_upstream = !has_upstream(&path, &context.config)?;
        self.show_loading_dialog(raw, "Push Branch", "Pushing selected branch")?;
        push_branch(&context.config, &path, &branch, set_upstream)?;
        {
            let session = &mut self.sessions[selected];
            refresh_branch_pr_cache(
                &context.repo,
                &context.config,
                &session.branch,
                &session.path,
                &mut session.pr,
                true,
            );
        }
        if self.sessions[selected].pr.summary.is_none()
            && !self.sessions[selected].is_default_branch(&context.config)
        {
            run_pre_pr_checks(&context.config, &path)?;
            let Some(pr_body) = self.prompt_pr_description(raw)? else {
                return Ok(());
            };
            self.show_loading_dialog(raw, "Create Pull Request", "Creating pull request")?;
            let session = &mut self.sessions[selected];
            create_pull_request(
                &context.repo,
                &context.config,
                &session.branch,
                &session.path,
                &pr_body,
                &mut session.pr,
            )?;
            self.show_message("push complete; pull request created")?;
        } else {
            self.show_message("push complete")?;
        }
        Ok(())
    }

    fn prompt_pr_description(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<Option<String>, String> {
        self.prompt_line_dialog(raw, "Create Pull Request", "Description: ", "")
    }

    pub(crate) fn merge_selected_pr(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let Some(context) = self.selected_worktree_context() else {
            return Ok(());
        };
        let selected = context.session_index;
        if self.sessions[selected].is_default_branch(&context.config) {
            self.show_message("default branch is not treated as a PR branch")?;
            return Ok(());
        }
        let path = self.sessions[selected].path.clone();
        let branch = self.sessions[selected].branch.clone();
        {
            let session = &mut self.sessions[selected];
            refresh_branch_pr_cache(
                &context.repo,
                &context.config,
                &session.branch,
                &session.path,
                &mut session.pr,
                false,
            );
        }
        let Some(summary) = self.sessions[selected].pr.summary.clone() else {
            self.show_message("no pull request found for selected branch")?;
            return Ok(());
        };
        if summary.merged {
            self.show_message("pull request is already merged")?;
            return Ok(());
        }
        if selected_dirty(&path, &context.config)? {
            self.show_message("working tree is dirty; commit or stash before merging")?;
            return Ok(());
        }
        run_pre_push_checks(&context.config, &path)?;
        self.show_loading_dialog(
            raw,
            "Merge Pull Request",
            &format!("Merging PR #{}", summary.number),
        )?;
        merge_pull_request(&context.config, &path, summary.number)?;
        self.show_loading_dialog(
            raw,
            "Merge Pull Request",
            &format!("Verifying PR #{} is merged", summary.number),
        )?;
        let merged = match wait_for_pr_merged(&path, summary.number, &context.config) {
            Ok(merged) => merged,
            Err(error) => {
                self.refresh_sessions()?;
                self.show_message(&format!(
                    "merge complete; could not verify PR merged: {error}"
                ))?;
                return Ok(());
            }
        };
        if !merged {
            self.refresh_sessions()?;
            self.show_message("merge complete; GitHub has not marked the PR merged yet")?;
            return Ok(());
        }

        if let Some(summary) = self.sessions[selected].pr.summary.as_mut() {
            summary.merged = true;
            summary.state = "MERGED".to_string();
        }
        let path_display = self.sessions[selected].path_display.clone();
        let warnings = self.sessions[selected].deletion_warnings();
        if self.confirm_delete_dialog(raw, &branch, &path_display, &warnings)? {
            delete_worktree_session(&context.repo, &context.config, &path, &branch)?;
            if self.selected_worktree_by_repo.get(&context.repo.root) == Some(&path) {
                self.selected_worktree_by_repo.remove(&context.repo.root);
            }
            self.refresh_sessions()?;
            self.show_message("merge complete; deleted local session data, worktree, and branch")?;
        } else {
            self.refresh_sessions()?;
            self.show_message("merge complete")?;
        }
        Ok(())
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
        let path = self.sessions[selected].path.clone();
        let path_display = self.sessions[selected].path_display.clone();
        let warnings = self.sessions[selected].deletion_warnings();
        if !self.confirm_archive_dialog(raw, &branch, &path_display, &warnings)? {
            return Ok(());
        }
        archive_worktree_session(&context.repo, &self.sessions[selected])?;
        if self.selected_worktree_by_repo.get(&context.repo.root) == Some(&path) {
            self.selected_worktree_by_repo.remove(&context.repo.root);
        }
        self.refresh_sessions()?;
        self.show_message("archived worktree; files and branch were left intact")?;
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
        delete_worktree_session(&context.repo, &context.config, &path, &branch)?;
        if self.selected_worktree_by_repo.get(&context.repo.root) == Some(&path) {
            self.selected_worktree_by_repo.remove(&context.repo.root);
        }
        self.refresh_sessions()?;
        self.show_message("deleted local session data, worktree, and branch")?;
        Ok(())
    }
}

fn ensure_repo_config_file(path: &Path, include_worktree_columns: bool) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("create config dir: {error}"))?;
    }
    if path.exists() {
        if include_worktree_columns {
            let mut text =
                fs::read_to_string(path).map_err(|error| format!("read config file: {error}"))?;
            if !text.contains("[worktrees]") {
                if !text.ends_with('\n') && !text.is_empty() {
                    text.push('\n');
                }
                text.push_str("\n[worktrees]\ncolumns = [\"url\", \"vars.localdev\"]\n");
                fs::write(path, text).map_err(|error| format!("update config file: {error}"))?;
            }
        }
        return Ok(());
    }
    let text = crate::config::repo_config_template(include_worktree_columns);
    fs::write(path, text).map_err(|error| format!("create config file: {error}"))
}

fn ensure_user_config_file(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("create config dir: {error}"))?;
    }
    if path.exists() {
        return Ok(());
    }
    let text = crate::config::user_config_template();
    fs::write(path, text).map_err(|error| format!("create user config file: {error}"))
}

fn open_url_in_browser(url: &str) -> Result<(), String> {
    run_browser_opener(&browser_opener_candidates(), url).map(|_| ())
}

const NO_BROWSER_ARGS: &[&str] = &[];
const GIO_BROWSER_ARGS: &[&str] = &["open"];
const WINDOWS_BROWSER_ARGS: &[&str] = &["/C", "start", ""];

fn browser_opener_candidates() -> Vec<(&'static str, &'static [&'static str])> {
    if cfg!(target_os = "macos") {
        vec![("open", NO_BROWSER_ARGS)]
    } else if cfg!(target_os = "windows") {
        vec![("cmd", WINDOWS_BROWSER_ARGS)]
    } else {
        vec![
            ("xdg-open", NO_BROWSER_ARGS),
            ("gio", GIO_BROWSER_ARGS),
            ("wslview", NO_BROWSER_ARGS),
        ]
    }
}

fn run_browser_opener(candidates: &[(&str, &[&str])], url: &str) -> Result<String, String> {
    let mut errors = Vec::new();
    for (program, args) in candidates {
        if !command_exists(program) {
            continue;
        }
        match Command::new(program)
            .args(*args)
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        {
            Ok(status) if status.success() => return Ok((*program).to_string()),
            Ok(status) => errors.push(format!("{program}: exited with {status}")),
            Err(error) => errors.push(format!("{program}: {error}")),
        }
    }
    if errors.is_empty() {
        let names = candidates
            .iter()
            .map(|(program, _)| *program)
            .collect::<Vec<_>>()
            .join(", ");
        Err(format!("no browser opener found; tried {names}"))
    } else {
        Err(format!("browser open failed: {}", errors.join("; ")))
    }
}

fn pr_poll_key(session: &crate::session::Session) -> PrPollKey {
    PrPollKey::for_session(session)
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
        let mut columns = discover_wt_columns(object);
        for column in &config.worktree_columns {
            if let Some(value) = wt_column_value(object, column) {
                columns.insert(column.clone(), value);
            }
        }
        by_path.insert(PathBuf::from(path), columns);
    }
    Ok(by_path)
}

fn discover_wt_columns(object: &str) -> BTreeMap<String, String> {
    let Ok(value) = serde_json::from_str::<Value>(object) else {
        return BTreeMap::new();
    };
    let mut columns = BTreeMap::new();
    let Some(fields) = value.as_object() else {
        return columns;
    };
    for (key, value) in fields {
        if key == "path" {
            continue;
        }
        collect_wt_column(&mut columns, key, value);
    }
    columns
}

fn collect_wt_column(columns: &mut BTreeMap<String, String>, key: &str, value: &Value) {
    match value {
        Value::String(value) => {
            if !value.is_empty() {
                columns.insert(key.to_string(), value.clone());
            }
        }
        Value::Bool(value) => {
            columns.insert(key.to_string(), value.to_string());
        }
        Value::Number(value) => {
            columns.insert(key.to_string(), value.to_string());
        }
        Value::Object(fields) => {
            for (field, value) in fields {
                collect_wt_column(columns, &format!("{key}.{field}"), value);
            }
        }
        Value::Array(_) | Value::Null => {}
    }
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

fn current_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
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

fn opencode_poll_key(session: &crate::session::Session) -> OpencodePollKey {
    OpencodePollKey::for_session(session)
}

#[cfg(test)]
mod tests {
    use crate::agent::AgentState;
    use crate::agent_session::{AgentSessionSlot, AgentSessionWarmupKey, AgentSessionWarmupResult};
    use crate::config::{Checks, Config, EscapeKey, MergeMethod};
    use crate::github::{PrCache, PrComment, PrDetails, PrSummary, pr_summary_or_error};
    use crate::repo::Repository;
    use crate::session::Session;
    use crate::tui::Tui;

    use super::{discover_wt_columns, run_browser_opener, status_label_with_behind};
    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    #[test]
    fn browser_opener_invokes_first_available_candidate() {
        let temp = unique_temp_dir("prism-browser-opener-test");
        fs::create_dir_all(&temp).unwrap();
        let log = temp.join("open.log");
        let opener = temp.join("opener");
        fs::write(
            &opener,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$@" > '{}'
exit 0
"#,
                log.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&opener).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&opener, permissions).unwrap();
        let opener = opener.display().to_string();

        let no_args: &[&str] = &[];
        let flag_args: &[&str] = &["--flag"];
        let candidates = [
            ("/definitely/missing", no_args),
            (opener.as_str(), flag_args),
        ];

        let used = run_browser_opener(&candidates, "https://example.test/pr/42").unwrap();

        assert_eq!(used, opener);
        assert_eq!(
            fs::read_to_string(&log).unwrap(),
            "--flag\nhttps://example.test/pr/42\n"
        );
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn discover_wt_columns_flattens_available_primitive_values() {
        let columns = discover_wt_columns(
            r#"{
                "path":"/repo/feature",
                "url":"https://example.test/pr/42",
                "url_active":true,
                "ci":{"status":"success","number":42},
                "vars":{"localdev":"on"},
                "empty":"",
                "labels":["bug"]
            }"#,
        );

        assert_eq!(
            columns.get("url").map(String::as_str),
            Some("https://example.test/pr/42")
        );
        assert_eq!(columns.get("url_active").map(String::as_str), Some("true"));
        assert_eq!(
            columns.get("ci.status").map(String::as_str),
            Some("success")
        );
        assert_eq!(columns.get("ci.number").map(String::as_str), Some("42"));
        assert_eq!(columns.get("vars.localdev").map(String::as_str), Some("on"));
        assert!(!columns.contains_key("path"));
        assert!(!columns.contains_key("empty"));
        assert!(!columns.contains_key("labels"));
    }

    #[test]
    fn review_fix_refreshes_pr_details_before_sending_prompt() {
        let temp = unique_temp_dir("prism-review-fix-refresh-test");
        let repo_root = temp.join("repo");
        let worktree = repo_root.join("feature");
        fs::create_dir_all(&worktree).unwrap();
        let gh = temp.join("gh");
        let git = temp.join("git");

        fs::write(
            &gh,
            r#"#!/bin/sh
case "$*" in
  "pr view feature --json comments,reviews,files,statusCheckRollup")
    cat <<'JSON'
{"comments":[{"id":"PRC_fresh","author":{"login":"reviewer"},"body":"fresh top-level comment","createdAt":"2026-06-14T12:00:00Z"}],"reviews":[{"id":"PRR_fresh","author":{"login":"bot"},"state":"CHANGES_REQUESTED","body":"fresh review body","submittedAt":"2026-06-14T12:01:00Z"}],"files":[],"statusCheckRollup":{"contexts":{"nodes":[]}}}
JSON
    ;;
  api\ graphql*)
    cat <<'JSON'
{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[]}}}}}
JSON
    ;;
  *)
    cat <<'JSON'
{"number":42,"title":"Review refresh","body":"","url":"https://github.com/example/repo/pull/42","state":"OPEN","reviewDecision":"CHANGES_REQUESTED","reviewRequests":{"nodes":[]},"headRefName":"feature","baseRefName":"main","headRefOid":"abc123","updatedAt":"2026-06-14T12:02:00Z","comments":{"totalCount":2},"statusCheckRollup":{"contexts":{"nodes":[]}},"isDraft":false}
JSON
    ;;
esac
"#,
        )
        .unwrap();
        fs::write(
            &git,
            r#"#!/bin/sh
case "$*" in
  *"remote get-url origin"*)
    echo "https://github.com/example/repo.git"
    ;;
esac
"#,
        )
        .unwrap();
        for executable in [&gh, &git] {
            let mut permissions = fs::metadata(executable).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(executable, permissions).unwrap();
        }

        let mut config = test_config();
        config.default_base = Some("main".to_string());
        config
            .tools
            .insert("gh".to_string(), gh.display().to_string());
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        let repo = Repository::with_config_dir_for_test(repo_root.clone(), temp.join("config"));
        let mut session = test_session(worktree, "feature");
        session.pr = PrCache {
            summary: Some(PrSummary {
                number: 42,
                title: "Stale review".to_string(),
                body: String::new(),
                url: "https://github.com/example/repo/pull/42".to_string(),
                state: "OPEN".to_string(),
                review_decision: "CHANGES_REQUESTED".to_string(),
                requested_reviewers: Vec::new(),
                head_ref: "feature".to_string(),
                base_ref: "main".to_string(),
                head_sha: "oldsha".to_string(),
                updated_at: "2026-06-14T11:00:00Z".to_string(),
                check_status: "unknown".to_string(),
                comment_count: 1,
                merged: false,
                draft: false,
            }),
            details: Some(PrDetails {
                comments: vec![PrComment {
                    author: "reviewer".to_string(),
                    body: "stale cached comment".to_string(),
                    ..PrComment::default()
                }],
                ..PrDetails::default()
            }),
            ..PrCache::default()
        };
        let mut tui = Tui::new_single(repo, config, vec![session]);
        tui.prompt_submissions = Some(Vec::new());

        tui.start_review_fix_for_test().unwrap();

        let submissions = tui.prompt_submissions.take().unwrap();
        assert_eq!(submissions.len(), 1);
        assert_eq!(submissions[0].0, 0);
        let prompt = &submissions[0].1;
        assert!(prompt.contains("fresh top-level comment"));
        assert!(prompt.contains("fresh review body"));
        assert!(!prompt.contains("stale cached comment"));
        assert_eq!(tui.sessions[0].prompt_summary, "review fix");

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn ci_fix_sends_prompt_to_agent_session() {
        let temp = unique_temp_dir("prism-ci-fix-send-test");
        let repo_root = temp.join("repo");
        let worktree = repo_root.join("feature");
        fs::create_dir_all(&worktree).unwrap();
        let gh = temp.join("gh");
        let git = temp.join("git");

        fs::write(
            &gh,
            r#"#!/bin/sh
case "$*" in
  "pr view feature --json comments,reviews,files,statusCheckRollup")
    cat <<'JSON'
{"comments":[],"reviews":[],"files":[],"statusCheckRollup":{"contexts":{"nodes":[{"name":"test","status":"COMPLETED","conclusion":"FAILURE"}]}}}
JSON
    ;;
  *)
    cat <<'JSON'
{"number":42,"title":"CI refresh","body":"","url":"https://github.com/example/repo/pull/42","state":"OPEN","reviewDecision":"","reviewRequests":{"nodes":[]},"headRefName":"feature","baseRefName":"main","headRefOid":"abc123","updatedAt":"2026-06-14T12:02:00Z","comments":{"totalCount":0},"statusCheckRollup":{"contexts":{"nodes":[{"name":"test","status":"COMPLETED","conclusion":"FAILURE"}]}},"isDraft":false}
JSON
    ;;
esac
"#,
        )
        .unwrap();
        fs::write(
            &git,
            r#"#!/bin/sh
case "$*" in
  *"remote get-url origin"*)
    echo "https://github.com/example/repo.git"
    ;;
esac
"#,
        )
        .unwrap();
        for executable in [&gh, &git] {
            let mut permissions = fs::metadata(executable).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(executable, permissions).unwrap();
        }

        let mut config = test_config();
        config.default_base = Some("main".to_string());
        config
            .tools
            .insert("gh".to_string(), gh.display().to_string());
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        let repo = Repository::with_config_dir_for_test(repo_root.clone(), temp.join("config"));
        let session = test_session(worktree, "feature");
        let mut tui = Tui::new_single(repo, config, vec![session]);
        tui.prompt_submissions = Some(Vec::new());

        tui.start_ci_fix_for_test().unwrap();

        let submissions = tui.prompt_submissions.take().unwrap();
        assert_eq!(submissions.len(), 1);
        assert_eq!(submissions[0].0, 0);
        assert!(submissions[0].1.contains("Here are CI failures on PR 42."));
        assert!(submissions[0].1.contains("- test"));
        assert_eq!(tui.sessions[0].prompt_summary, "ci fix");

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn pr_summary_or_error_returns_refresh_error() {
        let cache = PrCache {
            error: Some("gh pr view: authentication failed".to_string()),
            ..PrCache::default()
        };

        let error = pr_summary_or_error(&cache).unwrap_err();

        assert_eq!(error, "gh pr view: authentication failed");
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
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let session = test_session(temp.join("worktree"), "feature");
        let mut tui = Tui::new_single(repo, config, vec![session]);

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
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let session = test_session(temp.join("worktree"), "main");
        let mut tui = Tui::new_single(repo, config, vec![session]);

        let changed = tui.poll_pull_requests(false);

        assert!(!changed);
        assert!(!tui.repos[0].pr_summary_poll_in_flight);
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
        config.default_agent = "custom".to_string();
        config
            .agent_commands
            .insert("custom".to_string(), "opencode".to_string());
        config
            .tools
            .insert("tmux".to_string(), tmux.display().to_string());
        config
            .tools
            .insert("opencode".to_string(), "opencode".to_string());
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let session = test_session(temp.join("worktree"), "feature");
        let mut tui = Tui::new_single(repo, config, vec![session]);

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
        config.default_agent = "custom".to_string();
        config
            .agent_commands
            .insert("custom".to_string(), "opencode".to_string());
        config
            .tools
            .insert("tmux".to_string(), tmux.display().to_string());
        config
            .tools
            .insert("opencode".to_string(), "opencode".to_string());
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let session = test_session(temp.join("worktree"), "feature");
        let key = AgentSessionWarmupKey::new(AgentSessionSlot::for_session(&session), 0);
        let mut tui = Tui::new_single(repo, config, vec![session]);
        tui.tmux_warmups_in_flight.insert(key.clone());
        let tx = tui.tmux_warmup_tx.clone();

        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            let _ = tx.send(AgentSessionWarmupResult {
                key,
                running: Some(true),
                error: None,
            });
        });

        let started = Instant::now();
        tui.attach_selected_tmux_session().unwrap();

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
        config.default_agent = "custom".to_string();
        config
            .agent_commands
            .insert("custom".to_string(), "opencode".to_string());
        config
            .tools
            .insert("tmux".to_string(), tmux.display().to_string());
        config
            .tools
            .insert("opencode".to_string(), "opencode".to_string());
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let session = test_session(temp.join("worktree"), "feature");
        let mut tui = Tui::new_single(repo, config, vec![session]);

        tui.paste_prompt_into_tmux_agent(0, "build the thing")
            .unwrap();

        assert_eq!(fs::read_to_string(&prompt_file).unwrap(), "build the thing");
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
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let session = test_session(temp.join("worktree"), "feature");
        let slot = AgentSessionSlot::for_session(&session);
        let stale_key = AgentSessionWarmupKey::new(slot.clone(), 0);
        let mut tui = Tui::new_single(repo, config, vec![session]);
        tui.tmux_generations.insert(slot, 1);

        let changed = tui.apply_tmux_warmup_result(AgentSessionWarmupResult {
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
        config.default_agent = "custom".to_string();
        config
            .agent_commands
            .insert("custom".to_string(), "opencode".to_string());
        config
            .tools
            .insert("tmux".to_string(), tmux.display().to_string());
        config
            .tools
            .insert("opencode".to_string(), "opencode".to_string());
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let session = test_session(temp.join("worktree"), "feature");
        let mut tui = Tui::new_single(repo, config, vec![session]);

        tui.attach_selected_tmux_session().unwrap();

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
            repo_index: 0,
            repo_label: "repo".to_string(),
            repo_key: None,
            path: path.clone(),
            path_display: path.display().to_string(),
            branch: branch.to_string(),
            prompt_summary: String::new(),
            classification: crate::session::SessionClassification::Work,
            adopted: false,
            hidden: false,
            status_label: "clean".to_string(),
            agent_state: AgentState::Idle,
            opencode_status: None,
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
            opencode_port_base: 41_000,
            opencode_port_span: 1_000,
            opencode_shutdown_owned_servers: false,
            opencode_plan_plugin: false,
            escape_key: EscapeKey::EscEsc,
            merge_method: MergeMethod::Squash,
            icon_style: crate::config::IconStyle::Unicode,
            icon_style_configured: false,
            auto: crate::config::AutoConfig::default(),
            layout: crate::config::LayoutConfig::default(),
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
