use std::path::Path;
use std::time::Instant;

use crate::agent::AgentState;
use crate::agent_session::{AgentSessionSlot, AgentSessionWarmupKey};
use crate::tui_runtime::TerminalRuntime;
use crate::view;
use crate::workspace_state::CiState;

use super::{
    GitAction, LeaderHint, PanelFocus, Tui, auto_status, choice_list, plan_status,
    worktree_updated_label,
};

impl Tui {
    pub(crate) fn draw(&mut self, runtime: &mut TerminalRuntime) -> Result<(), String> {
        let input = crate::flight_recorder::take_input_for_frame();
        let started = Instant::now();
        self.tmux_portal_size =
            view::tmux_portal_size(runtime.area()?, self.config.layout.sidebar_width);
        let model_started = Instant::now();
        let model = self.frame_model();
        let model_elapsed = model_started.elapsed();
        let timing = runtime.draw(&model)?;
        let total = started.elapsed();
        let mut fields = vec![
            crate::flight_recorder::unsigned("model_us", model_elapsed.as_micros()),
            crate::flight_recorder::unsigned("render_us", timing.render.as_micros()),
            crate::flight_recorder::unsigned("terminal_us", timing.terminal.as_micros()),
            crate::flight_recorder::unsigned(
                "backend_us",
                timing.terminal.saturating_sub(timing.render).as_micros(),
            ),
        ];
        if let Some(input) = input.as_ref() {
            fields.push(crate::flight_recorder::unsigned("input_id", input.id()));
            fields.push(crate::flight_recorder::unsigned(
                "input_to_frame_us",
                input.elapsed().as_micros(),
            ));
            crate::flight_recorder::record(
                "input",
                "frame",
                Some(input.elapsed()),
                vec![crate::flight_recorder::unsigned("input_id", input.id())],
            );
        }
        crate::flight_recorder::record("tui", "frame", Some(total), fields);
        Ok(())
    }

    pub(super) fn frame_model(&self) -> view::FrameModel<'_> {
        let repos = self
            .visible_repo_indices()
            .into_iter()
            .filter_map(|index| {
                let repo = self.repos.get(index)?;
                Some(view::RepoRow {
                    label: repo.label.clone(),
                    root: repo.repo.root.display().to_string(),
                    key: repo.key,
                    health: self.repo_health_label(index),
                    selected: index == self.current_repo,
                })
            })
            .collect::<Vec<_>>();
        let worktrees = self
            .visible_session_indices()
            .into_iter()
            .filter_map(|index| {
                let session = self.sessions.get(index)?;
                let repo_root = self
                    .repos
                    .get(session.repo_index)
                    .map(|repo| repo.repo.root.display().to_string())
                    .unwrap_or_default();
                let repo_label = self
                    .repos
                    .get(session.repo_index)
                    .map(|repo| repo.label.clone())
                    .unwrap_or_else(|| session.repo_label.clone());
                let auto_status = self
                    .worktree_workflow_snapshot(
                        Path::new(&repo_root),
                        &session.path,
                        crate::execution::WorkflowKind::Auto,
                    )
                    .and_then(|workflow| auto_status(workflow.lifecycle));
                let plan_status = self
                    .worktree_workflow_snapshot(
                        Path::new(&repo_root),
                        &session.path,
                        crate::execution::WorkflowKind::Plan,
                    )
                    .and_then(|workflow| plan_status(workflow.lifecycle));
                let snapshot_status = self
                    .repos
                    .get(session.repo_index)
                    .and_then(|managed| self.workspace_repositories.get(&managed.identity))
                    .and_then(|repository| {
                        repository
                            .worktrees
                            .iter()
                            .find(|worktree| worktree.identity.path == session.path)
                    })
                    .map(|worktree| worktree.git.label());
                Some(view::WorktreeRow {
                    session_index: index,
                    repo_label,
                    repo_root,
                    worktree_path: session.path_display.clone(),
                    branch: session.branch.clone(),
                    visibility: session.visibility,
                    kind: if self
                        .repos
                        .get(session.repo_index)
                        .is_some_and(|repo| repo.config.is_default_branch(&session.branch))
                    {
                        view::WorktreeKind::DefaultBranch
                    } else if session.branch == "(detached)" {
                        view::WorktreeKind::Detached
                    } else {
                        view::WorktreeKind::FeatureWorktree
                    },
                    agent_state: session.agent_state,
                    status_label: snapshot_status.unwrap_or_else(|| session.status_label.clone()),
                    pr: session.pr.clone(),
                    wt_columns: session.wt_columns.clone(),
                    development: self.repos.get(session.repo_index).and_then(|managed| {
                        let key = session.identity_key(&managed.identity);
                        let dev_server = managed.wt_facts.get(&key)?.dev_server.as_ref()?;
                        Some(view::DevelopmentEnvironment {
                            url: dev_server.url.clone(),
                            listening: dev_server.listening,
                            quality: view::DevelopmentEnvironmentQuality::from(&managed.wt_quality),
                        })
                    }),
                    auto_status,
                    plan_status,
                    updated_label: worktree_updated_label(session),
                    unseen_comments: session.unseen_comments,
                    prompt_summary: session.prompt_summary.clone(),
                    classification: session.classification,
                    selected: Some(index) == self.selected_worktree_index(),
                })
            })
            .collect::<Vec<_>>();
        let selected_pr_identity = self.selected_repo_pr_identity();
        let repo_pr_summaries = self.current_repo_change_request_summaries();
        let repo_prs = self
            .repos
            .get(self.current_repo)
            .map(|managed| {
                repo_pr_summaries
                    .iter()
                    .map(|summary| {
                        let has_worktree = self.sessions.iter().any(|session| {
                            session.repo_index == self.current_repo
                                && session.pr.summary().is_some_and(|pr| {
                                    pr.change_request_identity.as_ref()
                                        == summary.change_request_identity.as_ref()
                                })
                        });
                        let repo_label = summary
                            .change_request_identity
                            .as_ref()
                            .map(|identity| identity.project_path().to_string())
                            .unwrap_or_else(|| managed.label.clone());
                        view::RepoPrRow::from_summary(
                            repo_label,
                            summary,
                            has_worktree,
                            selected_pr_identity == summary.change_request_identity.as_ref(),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let selected_repo_label = self
            .repos
            .get(self.current_repo)
            .map(|repo| repo.label.clone())
            .unwrap_or_else(|| "no repo".to_string());
        let selected_repo_root = self
            .repos
            .get(self.current_repo)
            .map(|repo| repo.repo.root.display().to_string())
            .unwrap_or_else(|| self.repo.root.display().to_string());
        view::FrameModel {
            config: &self.config,
            sessions: &self.sessions,
            status: self.status_rows(),
            repos,
            worktrees,
            repo_prs,
            current_repo_index: self.current_repo,
            selected_repo_label,
            selected_repo_root,
            selected_session: self.selected_worktree_index(),
            selected_comment: self.selected_comment,
            focus: self.focused_panel,
            main_focused: self.main_focused,
            main_scroll: self.main_scroll,
            repo_main_view: self.repo_main_view,
            worktree_main_view: self.worktree_main_view,
            worktree_list_mode: self.worktree_list_mode,
            mode_label: "normal",
            status_message: self.status_message.as_deref(),
            repo_filter: &self.repo_filter,
            worktree_filter: &self.worktree_filter,
            leader_hint: self.leader_hint_model(),
            auto_dashboard: self.current_auto_dashboard(),
            plan_dashboard: self.current_plan_dashboard(),
            tmux_portal: self.tmux_portal_model(),
            dialog: self.dialog.clone(),
        }
    }

    pub(super) fn tmux_portal_model(&self) -> Option<view::TmuxPortalModel<'_>> {
        if self.focused_panel != PanelFocus::Worktrees {
            return None;
        }
        let session = self.sessions.get(self.selected_worktree_index()?)?;
        let managed = self.repos.get(session.repo_index)?;
        let slot = AgentSessionSlot::for_repository_session(&managed.identity, session);
        let generation = self.tmux_generations.get(&slot)?;
        let current_key = AgentSessionWarmupKey::new(slot, *generation);
        let capture = self
            .tmux_portal
            .as_ref()
            .and_then(|portal| portal.capture.as_ref());
        let (branch, state) = match capture {
            Some(capture) if capture.key == current_key => match &capture.result {
                Ok(lines) => (&session.branch, view::TmuxPortalState::Ready(lines)),
                Err(_) => (&session.branch, view::TmuxPortalState::Unavailable),
            },
            Some(capture) => match &capture.result {
                Ok(lines) => (
                    &capture.key.slot.worktree.branch,
                    view::TmuxPortalState::Ready(lines),
                ),
                Err(_) => (&session.branch, view::TmuxPortalState::Loading),
            },
            None => (&session.branch, view::TmuxPortalState::Loading),
        };
        Some(view::TmuxPortalModel { branch, state })
    }

    pub(super) fn repo_health_label(&self, repo_index: usize) -> String {
        let mut attention = 0;
        let mut prs = 0;
        let mut ci_failed = 0;
        let mut ci_running = 0;
        let mut behind = 0;
        let snapshot = self
            .repos
            .get(repo_index)
            .and_then(|managed| self.workspace_repositories.get(&managed.identity));
        for worktree in snapshot
            .into_iter()
            .flat_map(|snapshot| &snapshot.worktrees)
        {
            if matches!(
                worktree.agent.state,
                Some(AgentState::NeedsInput | AgentState::NeedsRestart | AgentState::ExitedError)
            ) {
                attention += 1;
            }
            if let Some(pr) = &worktree.pull_request {
                prs += 1;
                match pr.ci {
                    Some(CiState::Failed | CiState::Mixed) => ci_failed += 1,
                    Some(CiState::Pending) => ci_running += 1,
                    _ => {}
                }
            }
            if self.repos.get(repo_index).is_some_and(|repo| {
                matches!(&worktree.branch, crate::workspace_state::BranchState::Named(branch) if repo.config.is_default_branch(branch))
            }) {
                behind += worktree.git.behind;
            }
        }
        attention += snapshot.map_or(0, |snapshot| snapshot.totals.attention);
        attention += self
            .sessions
            .iter()
            .filter(|session| session.repo_index == repo_index && session.unseen_comments)
            .count();

        let parts = [
            (view::RepoHealthKind::Attention, attention),
            (view::RepoHealthKind::PullRequests, prs),
            (view::RepoHealthKind::CiFailed, ci_failed),
            (view::RepoHealthKind::CiRunning, ci_running),
            (view::RepoHealthKind::Behind, behind),
        ];
        if parts.iter().all(|(_, count)| *count == 0) {
            "ok".to_string()
        } else {
            parts
                .iter()
                .map(|(kind, count)| {
                    format!(
                        "{}{count}",
                        view::repo_health_icon(*kind, self.config.icon_style)
                    )
                })
                .collect::<Vec<_>>()
                .join(" ")
        }
    }

    pub(super) fn status_rows(&self) -> Vec<view::StatusRow> {
        let mut running = 0;
        let mut attention = 0;
        let mut prs = 0;
        let mut ci_failed = 0;
        let mut ci_running = 0;
        let mut dirty = 0;
        let mut active_plans = 0;
        let mut failed_plans = 0;
        let mut active_auto = 0;
        let mut failed_auto = 0;
        for workflow in self
            .workspace_repositories
            .values()
            .flat_map(|repository| &repository.workflows)
        {
            match (workflow.identity.kind.as_str(), workflow.lifecycle.as_str()) {
                ("auto", "queued" | "running" | "paused") => active_auto += 1,
                ("auto", "failed" | "aborted") => failed_auto += 1,
                ("plan", "queued" | "running" | "paused") => active_plans += 1,
                ("plan", "failed" | "aborted") => failed_plans += 1,
                _ => {}
            }
        }
        for worktree in self
            .workspace_repositories
            .values()
            .flat_map(|repository| &repository.worktrees)
        {
            if worktree.git.dirty > 0 {
                dirty += 1;
            }
            if matches!(
                worktree.agent.state,
                Some(AgentState::Attached | AgentState::Running)
            ) {
                running += 1;
            }
            if matches!(
                worktree.agent.state,
                Some(AgentState::NeedsInput | AgentState::NeedsRestart | AgentState::ExitedError)
            ) {
                attention += 1;
            }
            if worktree.pull_request.is_some() {
                prs += 1;
            }
            match worktree.pull_request.as_ref().and_then(|pr| pr.ci) {
                Some(CiState::Failed | CiState::Mixed) => ci_failed += 1,
                Some(CiState::Pending) => ci_running += 1,
                _ => {}
            }
        }
        let behind: usize = self
            .repos
            .iter()
            .filter_map(|managed| self.workspace_repositories.get(&managed.identity).map(|snapshot| (managed, snapshot)))
            .flat_map(|(managed, snapshot)| {
                snapshot.worktrees.iter().filter_map(move |worktree| {
                    matches!(&worktree.branch, crate::workspace_state::BranchState::Named(branch) if managed.config.is_default_branch(branch))
                        .then_some(worktree.git.behind)
                })
            })
            .sum();
        attention += self
            .sessions
            .iter()
            .filter(|session| session.unseen_comments)
            .count();

        vec![
            view::StatusRow {
                label: "repos".to_string(),
                value: self.workspace_repositories.len().to_string(),
                attention: false,
            },
            view::StatusRow {
                label: "worktrees".to_string(),
                value: self
                    .workspace_repositories
                    .values()
                    .map(|repository| repository.worktrees.len())
                    .sum::<usize>()
                    .to_string(),
                attention: false,
            },
            view::StatusRow {
                label: "dirty".to_string(),
                value: dirty.to_string(),
                attention: dirty > 0,
            },
            view::StatusRow {
                label: "agents".to_string(),
                value: running.to_string(),
                attention: running > 0,
            },
            view::StatusRow {
                label: "auto".to_string(),
                value: active_auto.to_string(),
                attention: active_auto > 0,
            },
            view::StatusRow {
                label: "auto fail".to_string(),
                value: failed_auto.to_string(),
                attention: failed_auto > 0,
            },
            view::StatusRow {
                label: "plans".to_string(),
                value: active_plans.to_string(),
                attention: active_plans > 0,
            },
            view::StatusRow {
                label: "plan fail".to_string(),
                value: failed_plans.to_string(),
                attention: failed_plans > 0,
            },
            view::StatusRow {
                label: "attention".to_string(),
                value: attention.to_string(),
                attention: attention > 0,
            },
            view::StatusRow {
                label: "open prs".to_string(),
                value: prs.to_string(),
                attention: false,
            },
            view::StatusRow {
                label: "ci failed".to_string(),
                value: ci_failed.to_string(),
                attention: ci_failed > 0,
            },
            view::StatusRow {
                label: "ci running".to_string(),
                value: ci_running.to_string(),
                attention: ci_running > 0,
            },
            view::StatusRow {
                label: "behind".to_string(),
                value: behind.to_string(),
                attention: behind > 0,
            },
        ]
    }

    pub(super) fn leader_hint_model(&self) -> Option<view::LeaderHintModel> {
        match (self.leader_hint, self.focused_panel) {
            (Some(LeaderHint::Root), PanelFocus::Status) => Some(choice_list(
                "Shortcuts",
                &[
                    ("g", "git actions"),
                    ("p", "plan actions"),
                    ("0", "focus main"),
                ],
            )),
            (Some(LeaderHint::Root), PanelFocus::Repos) => Some(view::ChoiceList {
                title: "Shortcuts".to_string(),
                choices: vec![
                    view::KeyChoice::new("g", "git actions"),
                    self.remote_pr_list_choice(),
                    view::KeyChoice::new("W", "worktree columns"),
                    view::KeyChoice::new("0", "focus main"),
                    view::KeyChoice::new("space/enter", "open default tmux"),
                ],
            }),
            (Some(LeaderHint::Root), PanelFocus::Worktrees) => Some(choice_list(
                "Shortcuts",
                &[
                    ("g", "git actions"),
                    ("p", "plan actions"),
                    ("0", "focus main"),
                    ("enter", "terminal"),
                    ("space", "agent if valid"),
                ],
            )),
            (Some(LeaderHint::Git), PanelFocus::Status) => Some(view::ChoiceList {
                title: "Git Actions".to_string(),
                choices: vec![self.git_choice(
                    GitAction::LazyGit,
                    "g",
                    "lazygit after focusing repos/worktrees",
                )],
            }),
            (Some(LeaderHint::Git), PanelFocus::Repos) => Some(view::ChoiceList {
                title: "Git Actions".to_string(),
                choices: vec![
                    self.git_choice(GitAction::LazyGit, "g", "lazygit"),
                    self.git_choice(GitAction::SubmitReview, "v", "review selected PR"),
                    view::KeyChoice::new("p", "pull default branch"),
                ],
            }),
            (Some(LeaderHint::Git), PanelFocus::Worktrees) => Some(view::ChoiceList {
                title: "Git Actions".to_string(),
                choices: vec![
                    self.git_choice(GitAction::LazyGit, "g", "lazygit"),
                    self.git_choice(GitAction::Push, "P", "push/create PR"),
                    self.git_choice(GitAction::OpenPr, "o", "open PR"),
                    self.git_choice(GitAction::MergeIntent, "M", "toggle merge queue"),
                    self.git_choice(GitAction::CiFix, "c", "CI repair"),
                    self.git_choice(GitAction::ReviewFix, "f", "review repair"),
                    self.git_choice(GitAction::ResolveAllComments, "R", "resolve all comments"),
                ],
            }),
            (None, _) => None,
        }
    }
}
