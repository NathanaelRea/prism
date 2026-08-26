use std::time::Instant;

use crate::agent::AgentState;
use crate::agent_session::{AgentSessionSlot, AgentSessionWarmupKey};
use crate::tui_runtime::TerminalDriver;
use crate::view;
use crate::workspace_state::CiState;

use super::{GitAction, LeaderHint, PanelFocus, Tui, choice_list, worktree_updated_label};

fn workflow_phase_label(phase: crate::PromptStepPhase) -> &'static str {
    match phase {
        crate::PromptStepPhase::Pending => "pending",
        crate::PromptStepPhase::Checking => "checking",
        crate::PromptStepPhase::Preparing | crate::PromptStepPhase::Prepared => "preparing",
        crate::PromptStepPhase::RunningAgent => "running Agent",
        crate::PromptStepPhase::AgentSucceeded | crate::PromptStepPhase::Finalizing => "finalizing",
        crate::PromptStepPhase::Waiting => "waiting",
        crate::PromptStepPhase::Satisfied => "satisfied",
        crate::PromptStepPhase::Completed => "completed",
        crate::PromptStepPhase::Failed => "failed",
        crate::PromptStepPhase::Cancelled => "cancelled",
        crate::PromptStepPhase::RecoveryRequired => "recovery required",
    }
}

impl Tui {
    pub(crate) fn draw(&mut self, runtime: &mut dyn TerminalDriver) -> Result<(), String> {
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
            worktree_list_mode: self.worktree_list_mode,
            mode_label: "normal",
            status_message: self.status_message.as_deref(),
            repo_filter: &self.repo_filter,
            worktree_filter: &self.worktree_filter,
            leader_hint: self.leader_hint_model(),
            workflow_dashboard: self.current_workflow_dashboard(),
            tmux_portal: self.tmux_portal_model(),
            dialog: self.dialog.clone(),
        }
    }

    pub(super) fn current_workflow_dashboard(&self) -> Option<view::WorkflowDashboard> {
        let selected_path = self
            .selected_worktree_index()
            .and_then(|index| self.sessions.get(index))
            .map(|session| &session.path);
        let all_runs = self
            .workspace_repositories
            .values()
            .flat_map(|repository| &repository.workflows)
            .collect::<Vec<_>>();
        let mut candidates = all_runs
            .iter()
            .copied()
            .filter(|workflow| selected_path.is_none_or(|path| &workflow.worktree.path == path))
            .collect::<Vec<_>>();
        candidates.sort_by_key(|workflow| (workflow.updated_unix_ms, &workflow.identity.run_id));
        let workflow = self
            .selected_workflow_run
            .as_ref()
            .and_then(|selected| {
                all_runs
                    .iter()
                    .copied()
                    .find(|workflow| &workflow.identity.run_id == selected)
            })
            .or_else(|| {
                candidates.iter().rev().copied().find(|workflow| {
                    matches!(
                        workflow.lifecycle.label(),
                        "queued" | "running" | "paused" | "waiting" | "input required"
                    )
                })
            })
            .or_else(|| candidates.last().copied())?;
        let run_position = candidates
            .iter()
            .position(|candidate| candidate.identity.run_id == workflow.identity.run_id)
            .map_or(0, |index| index + 1);
        let detail = self
            .workspace_repositories
            .values()
            .flat_map(|repository| &repository.workflow_details)
            .find(|detail| detail.id == workflow.identity.run_id)
            .cloned();
        let current_step = workflow
            .current_step
            .as_ref()
            .map(|step| step.label.clone());
        let selected_step = self
            .selected_workflow_step
            .as_ref()
            .filter(|selected| {
                detail
                    .as_ref()
                    .is_some_and(|run| run.steps.iter().any(|step| &step.key == *selected))
            })
            .cloned()
            .or_else(|| current_step.clone())
            .or_else(|| detail.as_ref()?.steps.first().map(|step| step.key.clone()));
        Some(view::WorkflowDashboard {
            run_id: workflow.identity.run_id.clone(),
            status: workflow.lifecycle.label().into(),
            current_step,
            selected_step,
            completed_steps: workflow.progress.completed,
            total_steps: workflow.progress.total,
            run_position,
            run_count: candidates.len(),
            detail,
            can_pause: workflow.available_controls.pause,
            can_resume: workflow.available_controls.resume,
            can_cancel: workflow.available_controls.stop,
            can_retry: workflow.available_controls.retry,
        })
    }

    pub(super) fn select_adjacent_workflow(&mut self, direction: isize) {
        let selected_path = self
            .selected_worktree_index()
            .and_then(|index| self.sessions.get(index))
            .map(|session| &session.path);
        let mut runs = self
            .workspace_repositories
            .values()
            .flat_map(|repository| &repository.workflows)
            .filter(|workflow| selected_path.is_none_or(|path| &workflow.worktree.path == path))
            .collect::<Vec<_>>();
        runs.sort_by_key(|workflow| (workflow.updated_unix_ms, &workflow.identity.run_id));
        if runs.is_empty() {
            self.selected_workflow_run = None;
            return;
        }
        let current = self
            .selected_workflow_run
            .as_ref()
            .and_then(|selected| runs.iter().position(|run| &run.identity.run_id == selected))
            .unwrap_or(runs.len() - 1);
        let next = (current as isize + direction).rem_euclid(runs.len() as isize) as usize;
        self.selected_workflow_run = Some(runs[next].identity.run_id.clone());
        self.selected_workflow_step = runs[next]
            .current_step
            .as_ref()
            .map(|step| step.label.clone());
        self.workflow_step_selection_manual = false;
        self.main_scroll = 0;
    }

    pub(super) fn follow_current_workflow_step(&mut self) {
        if self.workflow_step_selection_manual {
            return;
        }
        self.selected_workflow_step = self
            .current_workflow_dashboard()
            .and_then(|dashboard| dashboard.current_step.or(dashboard.selected_step));
    }

    pub(super) fn move_workflow_step_selection(&mut self, direction: isize) -> bool {
        if self.focused_panel != PanelFocus::Worktrees {
            return false;
        }
        let Some(dashboard) = self.current_workflow_dashboard() else {
            return false;
        };
        let Some(run) = dashboard.detail else {
            return true;
        };
        if run.steps.is_empty() {
            return true;
        }
        let current = dashboard
            .selected_step
            .as_ref()
            .and_then(|selected| run.steps.iter().position(|step| &step.key == selected))
            .unwrap_or(0);
        let next = (current as isize + direction).clamp(0, run.steps.len() as isize - 1) as usize;
        self.selected_workflow_step = Some(run.steps[next].key.clone());
        self.workflow_step_selection_manual = true;
        true
    }

    pub(super) fn handle_workflow_enter(
        &mut self,
        runtime: &mut dyn TerminalDriver,
    ) -> Result<bool, String> {
        if !self.main_focused || self.focused_panel != PanelFocus::Worktrees {
            return Ok(false);
        }
        let Some(dashboard) = self.current_workflow_dashboard() else {
            return Ok(false);
        };
        let Some(run) = dashboard.detail else {
            return Ok(false);
        };
        let Some(selected) = dashboard.selected_step else {
            return Ok(false);
        };
        let Some(step) = run.steps.iter().find(|step| step.key == selected) else {
            return Ok(false);
        };
        let mut lines = vec![
            view::DialogLine {
                text: format!("workflow: {}", run.workflow_name),
                attention: false,
            },
            view::DialogLine {
                text: format!("run: {}", run.id),
                attention: false,
            },
            view::DialogLine {
                text: format!("status: {}", dashboard.status),
                attention: false,
            },
            view::DialogLine {
                text: format!(
                    "progress: {}/{} steps",
                    dashboard.completed_steps, dashboard.total_steps
                ),
                attention: false,
            },
            view::DialogLine {
                text: format!(
                    "Agent runs: {}/{}",
                    run.agent_runs_consumed, run.max_agent_runs
                ),
                attention: false,
            },
            view::DialogLine {
                text: String::new(),
                attention: false,
            },
            view::DialogLine {
                text: format!("stage: {}", step.key),
                attention: false,
            },
            view::DialogLine {
                text: format!("phase: {}", workflow_phase_label(step.phase)),
                attention: matches!(
                    step.phase,
                    crate::PromptStepPhase::Failed | crate::PromptStepPhase::RecoveryRequired
                ),
            },
        ];
        if let Some(summary) = step.summary.as_deref() {
            lines.push(view::DialogLine {
                text: format!("summary: {summary}"),
                attention: false,
            });
        }
        if step.explicit_dependencies {
            lines.push(view::DialogLine {
                text: if step.dependencies.is_empty() {
                    "dependencies: root".to_string()
                } else {
                    format!("dependencies: {}", step.dependencies.join(", "))
                },
                attention: false,
            });
        }
        if let Some(wake) = step.wake_at_unix_ms {
            lines.push(view::DialogLine {
                text: format!("next check: {wake}"),
                attention: false,
            });
        }
        if let Some(error) = step
            .attempts
            .iter()
            .rev()
            .find_map(|attempt| attempt.error.as_deref())
        {
            lines.push(view::DialogLine {
                text: format!("error: {error}"),
                attention: true,
            });
        }
        if let Some(final_text) = step.final_text() {
            lines.push(view::DialogLine {
                text: String::new(),
                attention: false,
            });
            lines.push(view::DialogLine {
                text: final_text.to_string(),
                attention: false,
            });
        }
        self.notice_dialog(runtime, "Workflow Details", lines)?;
        Ok(true)
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
        let mut active_workflows = 0;
        let mut failed_workflows = 0;
        let mut input_required_workflows = 0;
        for workflow in self
            .workspace_repositories
            .values()
            .flat_map(|repository| &repository.workflows)
        {
            match workflow.lifecycle.as_str() {
                "queued" | "running" | "paused" | "waiting" => active_workflows += 1,
                "input_required" => input_required_workflows += 1,
                "failed" | "aborted" | "recovery_required" => failed_workflows += 1,
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
                label: "workflows".to_string(),
                value: active_workflows.to_string(),
                attention: active_workflows > 0,
            },
            view::StatusRow {
                label: "workflow input".to_string(),
                value: input_required_workflows.to_string(),
                attention: input_required_workflows > 0,
            },
            view::StatusRow {
                label: "workflow fail".to_string(),
                value: failed_workflows.to_string(),
                attention: failed_workflows > 0,
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
                    ("w", "workflow actions"),
                    ("c", "configuration"),
                    ("0", "focus main"),
                ],
            )),
            (Some(LeaderHint::Root), PanelFocus::Repos) => Some(view::ChoiceList {
                title: "Shortcuts".to_string(),
                choices: vec![
                    view::KeyChoice::new("g", "git actions"),
                    view::KeyChoice::new("w", "workflow actions"),
                    view::KeyChoice::new("c", "configuration"),
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
                    ("w", "workflow actions"),
                    ("c", "configuration"),
                    ("0", "focus main"),
                    ("enter", "terminal"),
                    ("space", "agent if valid"),
                ],
            )),
            (Some(LeaderHint::Workflow), _) => Some(choice_list(
                "Workflow Actions",
                &[("a", "AI one-off"), ("w", "workflow picker")],
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
                    self.git_choice(GitAction::Push, "P", "push branch"),
                    self.git_choice(GitAction::OpenPr, "o", "open PR"),
                    self.git_choice(GitAction::Merge, "M", "merge via provider"),
                    self.git_choice(GitAction::CiFix, "c", "CI repair"),
                    self.git_choice(GitAction::ReviewFix, "f", "review repair"),
                    self.git_choice(GitAction::ResolveAllComments, "R", "resolve all comments"),
                ],
            }),
            (None, _) => None,
        }
    }
}
