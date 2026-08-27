use super::*;

/// Semantic command accepted by the dashboard controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DashboardCommand {
    Up,
    Down,
    Left,
    Right,
    FocusNext,
    FocusPrevious,
    FocusMain,
    FocusStatus,
    FocusRepos,
    FocusWorktrees,
    FocusMerges,
    Bottom,
    G,
    PreviousBlock,
    NextBlock,
    PreviousView,
    NextView,
    Leader,
    LeaderGit,
    LeaderWorkflow,
    OpenTmuxSession,
    WorkflowLauncher,
    WorkflowAi,
    WorkflowPauseResume,
    WorkflowRetry,
    Configuration,
    LazyGit,
    OpenPr,
    OpenDevelopmentUrl,
    WorktrunkLogs,
    SubmitReview,
    Terminal,
    Help,
    Refresh,
    VisibilityUp,
    VisibilityDown,
    RepoShortcut(char),
    OpenRemotePrs,
    Push,
    Merge,
    CiFix,
    ReviewFix,
    ResolveAllComments,
    PullDefault,
    Create,
    AbortOpencode,
    Delete,
    Unarchive,
    MigrateHarness,
    DeletePermanent,
    Search,
    Quit,
    Other,
}

/// State retained across semantic dashboard commands.
#[derive(Default)]
pub(crate) struct CommandState {
    pending_g: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommandOutcome {
    Continue,
    Quit,
}

impl Tui {
    /// Execute one semantic dashboard command.
    ///
    /// Keyboard decoding and terminal event polling live outside this seam. A
    /// scripted terminal adapter can therefore exercise the same command path
    /// as the production Crossterm adapter.
    pub(crate) async fn dispatch_command(
        &mut self,
        runtime: &mut dyn TerminalDriver,
        command: DashboardCommand,
        state: &mut CommandState,
    ) -> Result<CommandOutcome, String> {
        let mut should_quit = false;
        match command {
            DashboardCommand::Quit => {
                self.clear_leader_hint();
                state.pending_g = false;
                should_quit = self.confirm_quit()?;
            }
            DashboardCommand::Down => {
                self.clear_leader_hint();
                self.move_down();
                state.pending_g = false;
            }
            DashboardCommand::Left => {
                self.clear_leader_hint();
                self.move_left();
                state.pending_g = false;
            }
            DashboardCommand::Right => {
                self.clear_leader_hint();
                self.move_right();
                state.pending_g = false;
            }
            DashboardCommand::FocusNext => {
                self.clear_leader_hint();
                self.focus_next_panel();
                state.pending_g = false;
            }
            DashboardCommand::FocusPrevious => {
                self.clear_leader_hint();
                self.focus_previous_panel();
                state.pending_g = false;
            }
            DashboardCommand::FocusMain => {
                self.clear_leader_hint();
                self.focus_main();
                state.pending_g = false;
            }
            DashboardCommand::FocusStatus => {
                self.clear_leader_hint();
                self.focus_status();
                state.pending_g = false;
            }
            DashboardCommand::FocusRepos => {
                self.clear_leader_hint();
                self.focus_repos();
                state.pending_g = false;
            }
            DashboardCommand::FocusWorktrees => {
                self.clear_leader_hint();
                self.focus_worktrees();
                state.pending_g = false;
            }
            DashboardCommand::FocusMerges => {
                self.clear_leader_hint();
                self.focus_merges();
                state.pending_g = false;
            }
            DashboardCommand::Up => {
                self.clear_leader_hint();
                self.move_up();
                state.pending_g = false;
            }
            DashboardCommand::Bottom => {
                self.clear_leader_hint();
                state.pending_g = false;
                self.select_bottom_visible();
            }
            DashboardCommand::G => {
                self.clear_leader_hint();
                if state.pending_g {
                    self.select_top_visible();
                    state.pending_g = false;
                } else {
                    state.pending_g = true;
                }
            }
            DashboardCommand::PreviousBlock => {
                self.clear_leader_hint();
                self.select_adjacent_workflow(-1);
                state.pending_g = false;
            }
            DashboardCommand::NextBlock => {
                self.clear_leader_hint();
                self.select_adjacent_workflow(1);
                state.pending_g = false;
            }
            DashboardCommand::PreviousView => {
                self.clear_leader_hint();
                self.switch_worktree_list_mode(WorktreeListMode::Global);
                state.pending_g = false;
            }
            DashboardCommand::NextView => {
                self.clear_leader_hint();
                self.switch_worktree_list_mode(WorktreeListMode::Repo);
                state.pending_g = false;
            }
            DashboardCommand::Leader => {
                self.leader_hint = Some(LeaderHint::Root);
            }
            DashboardCommand::LeaderGit => {
                self.leader_hint = Some(LeaderHint::Git);
            }
            DashboardCommand::LeaderWorkflow => {
                self.leader_hint = Some(LeaderHint::Workflow);
            }
            DashboardCommand::OpenTmuxSession => {
                self.clear_leader_hint();
                state.pending_g = false;
                if self.handle_workflow_enter(runtime)? {
                    self.draw(runtime)?;
                    return Ok(CommandOutcome::Continue);
                }
                if self.open_selected_comment_dialog(runtime)? {
                    self.draw(runtime)?;
                    return Ok(CommandOutcome::Continue);
                }
                match self.open_tmux_session_target() {
                    OpenTmuxSessionTarget::RepoDefaultAgent(index) => {
                        self.enter_agent_mode_for_index(runtime, index).await?
                    }
                    OpenTmuxSessionTarget::WorktreeAgent => self.enter_agent_mode(runtime).await?,
                    OpenTmuxSessionTarget::RepoPr => {
                        self.open_selected_repo_pr_agent(runtime).await?
                    }
                    OpenTmuxSessionTarget::Blocked(message) => self.show_message(message)?,
                }
            }
            DashboardCommand::WorkflowLauncher => {
                self.clear_leader_hint();
                state.pending_g = false;
                if let Err(error) = self.launch_workflow(runtime).await {
                    self.show_error("workflow launcher failed", &error)?;
                }
            }
            DashboardCommand::WorkflowAi => {
                self.clear_leader_hint();
                state.pending_g = false;
                if let Err(error) = self.create_ai_workflow(runtime).await {
                    self.show_error("AI Workflow creation failed", &error)?;
                }
            }
            DashboardCommand::WorkflowPauseResume => {
                if let Err(error) = self.control_selected_workflow(runtime, "toggle").await {
                    self.show_error("Workflow control failed", &error)?;
                }
            }
            DashboardCommand::WorkflowRetry => {
                if let Err(error) = self.control_selected_workflow(runtime, "retry").await {
                    self.show_error("Workflow retry failed", &error)?;
                }
            }
            DashboardCommand::Configuration => {
                self.clear_leader_hint();
                state.pending_g = false;
                if let Err(error) = self.show_configuration_tree(runtime).await {
                    self.show_error("configuration failed", &error)?;
                }
            }
            DashboardCommand::LazyGit => {
                self.clear_leader_hint();
                state.pending_g = false;
                if self.git_action_enabled(GitAction::LazyGit) {
                    if self.focused_panel == PanelFocus::Repos {
                        if let Err(error) = self.open_selected_repo_lazygit(runtime).await {
                            self.show_error("repository lazygit failed", &error)?;
                        }
                    } else if let Err(error) =
                        self.open_tmux_window(runtime, TmuxWindow::LazyGit).await
                    {
                        self.show_error("lazygit failed", &error)?;
                    }
                }
            }
            DashboardCommand::OpenPr => {
                self.clear_leader_hint();
                state.pending_g = false;
                if self.git_action_enabled(GitAction::OpenPr)
                    && let Err(error) = self.open_selected_pr(runtime).await
                {
                    self.show_error("open PR failed", &error)?;
                }
            }
            DashboardCommand::OpenDevelopmentUrl => {
                self.clear_leader_hint();
                state.pending_g = false;
                if let Err(error) = self.open_selected_development_url().await {
                    self.show_error("open development URL failed", &error)?;
                }
            }
            DashboardCommand::WorktrunkLogs => {
                self.clear_leader_hint();
                state.pending_g = false;
                if let Err(error) = self.show_selected_worktrunk_logs(runtime) {
                    self.show_error("Worktrunk hook logs failed", &error)?;
                }
            }
            DashboardCommand::SubmitReview => {
                self.clear_leader_hint();
                state.pending_g = false;
                if self.git_action_enabled(GitAction::SubmitReview)
                    && let Err(error) = self.submit_selected_repo_pr_review(runtime)
                {
                    self.show_error("submit review failed", &error)?;
                }
            }
            DashboardCommand::Terminal => {
                self.clear_leader_hint();
                state.pending_g = false;
                if self.focused_panel == PanelFocus::Status {
                    self.show_message("focus repos or worktrees to open a terminal")?;
                } else if self.focused_panel == PanelFocus::Repos {
                    if let Err(error) = self.open_selected_repo_terminal(runtime).await {
                        self.show_error("repository terminal failed", &error)?;
                    }
                } else if let Err(error) =
                    self.open_tmux_window(runtime, TmuxWindow::Terminal).await
                {
                    self.show_error("terminal failed", &error)?;
                }
            }
            DashboardCommand::Help => {
                self.clear_leader_hint();
                state.pending_g = false;
                self.show_keybindings_dialog(runtime)?;
            }
            DashboardCommand::Refresh => {
                self.clear_leader_hint();
                state.pending_g = false;
                if self.focused_panel == PanelFocus::Repos && !self.main_focused {
                    if let Err(error) = self.reorder_repositories(runtime).await {
                        self.show_error("reorder repositories failed", &error)?;
                    }
                } else {
                    self.start_wt_column_poll();
                    self.refresh_sessions_after_tmux()?;
                }
            }
            DashboardCommand::VisibilityUp => {
                self.clear_leader_hint();
                state.pending_g = false;
                if !self.is_worktree_session_panel() {
                    self.show_message("focus worktrees or merges to change visibility")?;
                } else if let Err(error) = self.adjust_selected_visibility(1) {
                    self.show_error("visibility update failed", &error)?;
                }
            }
            DashboardCommand::VisibilityDown => {
                self.clear_leader_hint();
                state.pending_g = false;
                if !self.is_worktree_session_panel() {
                    self.show_message("focus worktrees or merges to change visibility")?;
                } else if let Err(error) = self.adjust_selected_visibility(-1) {
                    self.show_error("visibility update failed", &error)?;
                }
            }
            DashboardCommand::RepoShortcut(key) => {
                self.clear_leader_hint();
                state.pending_g = false;
                if let Err(error) = self.select_repo_by_key(key) {
                    self.show_error("select repository failed", &error)?;
                }
            }
            DashboardCommand::Push => {
                self.clear_leader_hint();
                state.pending_g = false;
                if self.git_action_enabled(GitAction::Push)
                    && let Err(error) = self.push_selected_branch(runtime).await
                {
                    self.show_error("push failed", &error)?;
                }
            }
            DashboardCommand::Merge | DashboardCommand::CiFix | DashboardCommand::ReviewFix => {
                self.clear_leader_hint();
                state.pending_g = false;
                let action = git_action_for_command(command)
                    .ok_or_else(|| "unsupported Git action command".to_string())?;
                if self.git_action_enabled(action) {
                    let result = match git_action_execution(action) {
                        GitActionExecution::ProviderMerge => {
                            self.merge_selected_change_request(runtime).await
                        }
                        GitActionExecution::Stabilize => {
                            self.launch_stabilization_workflow(runtime).await
                        }
                    };
                    if let Err(error) = result {
                        self.show_error(git_action_error_title(action), &error)?;
                    }
                }
            }
            DashboardCommand::ResolveAllComments => {
                self.clear_leader_hint();
                state.pending_g = false;
                if self.git_action_enabled(GitAction::ResolveAllComments)
                    && let Err(error) = self.resolve_review_comments(runtime)
                {
                    self.show_error("resolve review comments failed", &error)?;
                }
            }
            DashboardCommand::PullDefault => {
                self.clear_leader_hint();
                state.pending_g = false;
                if self.focused_panel != PanelFocus::Repos {
                    self.show_message("focus repos to pull the default branch")?;
                } else if let Err(error) = self.pull_default_branch(runtime).await {
                    self.show_error("pull failed", &error)?;
                }
            }
            DashboardCommand::Create => {
                self.clear_leader_hint();
                state.pending_g = false;
                if self.focused_panel != PanelFocus::Repos {
                    self.show_message("focus repos to create a worktree session")?;
                } else {
                    match self.create_session(runtime).await {
                        Ok(true) => self.focus_worktrees(),
                        Ok(false) => {}
                        Err(error) => self.show_error("create session failed", &error)?,
                    }
                }
            }
            DashboardCommand::MigrateHarness => {
                if !self.is_worktree_session_panel() {
                    self.show_message("focus worktrees or merges to migrate an agent harness")?;
                } else if let Some(index) = self.selected_worktree_index() {
                    self.migrate_worktree_harness(index).await?;
                }
            }
            DashboardCommand::AbortOpencode => {
                self.clear_leader_hint();
                state.pending_g = false;
                match self.control_selected_workflow(runtime, "cancel").await {
                    Ok(true) => {}
                    Ok(false) if !self.is_worktree_session_panel() => {
                        self.show_message("focus worktrees or merges to abort an agent session")?;
                    }
                    Ok(false) => {
                        if let Err(error) = self.abort_selected_opencode_session(runtime).await {
                            self.show_error("abort failed", &error)?;
                        }
                    }
                    Err(error) => self.show_error("Workflow control failed", &error)?,
                }
            }
            DashboardCommand::OpenRemotePrs => {
                self.clear_leader_hint();
                state.pending_g = false;
                if self.focused_panel != PanelFocus::Repos {
                    self.show_message("focus repos to open a remote PR worktree")?;
                } else if self.selected_repo_list_support()
                    != Some(crate::remote::SupportLevel::Supported)
                {
                    self.show_message("remote PR listing is unavailable for this provider")?;
                } else if let Err(error) = self.open_remote_pr_worktree(runtime).await {
                    self.show_error("open remote PR worktree failed", &error)?;
                }
            }
            DashboardCommand::Delete => {
                self.clear_leader_hint();
                state.pending_g = false;
                if self.focused_panel == PanelFocus::Status {
                    self.show_message("focus worktrees to delete a worktree/session")?;
                } else if self.focused_panel == PanelFocus::Repos {
                    self.show_message("repository removal is available from r")?;
                } else if let Err(error) = self.archive_session(runtime).await {
                    self.show_error("archive failed", &error)?;
                }
            }
            DashboardCommand::Unarchive => {
                self.clear_leader_hint();
                state.pending_g = false;
                if self.focused_panel != PanelFocus::Repos {
                    self.show_message("focus repos to unarchive a worktree")?;
                } else if let Err(error) = self.unarchive_session(runtime).await {
                    self.show_error("unarchive failed", &error)?;
                }
            }
            DashboardCommand::DeletePermanent => {
                self.clear_leader_hint();
                state.pending_g = false;
                if !self.is_worktree_session_panel() {
                    self.show_message(
                        "focus worktrees or merges to permanently delete a worktree/session",
                    )?;
                } else if let Err(error) = self.delete_session(runtime).await {
                    self.show_error("delete failed", &error)?;
                }
            }
            DashboardCommand::Search => {
                self.clear_leader_hint();
                state.pending_g = false;
                self.search_sessions(runtime)?;
            }
            DashboardCommand::Other => {
                self.clear_leader_hint();
                state.pending_g = false;
            }
        }
        Ok(if should_quit {
            CommandOutcome::Quit
        } else {
            CommandOutcome::Continue
        })
    }
}
