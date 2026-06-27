use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, ErrorKind, Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use crate::agent::AgentState;
use crate::agent_session::{AgentSessionSlot, AgentSessionWarmupKey, AgentSessionWarmupResult};
use crate::config::Config;
use crate::github::{PrCache, PrSummary};
use crate::input::{Key, KeyInput};
use crate::opencode::{OpencodeEvent, OpencodeStatus};
use crate::repo::Repository;
use crate::session::{Session, append_runtime_log};
use crate::terminal::{RawTerminal, stdin_is_tty, terminal_size, write_stdout};
use crate::tmux::TmuxWindow;
use crate::util::{status_count, strip_ansi, truncate_line, yes};
use crate::view;

pub struct Tui {
    pub(crate) repo: Repository,
    pub(crate) config: Config,
    pub(crate) repos: Vec<ManagedRepo>,
    pub(crate) current_repo: usize,
    pub(crate) sessions: Vec<Session>,
    pub(crate) selected: usize,
    pub(crate) selected_repo_root: Option<PathBuf>,
    pub(crate) focused_panel: PanelFocus,
    pub(crate) repo_main_view: view::RepoMainView,
    pub(crate) selected_worktree_by_repo: BTreeMap<PathBuf, PathBuf>,
    pub(crate) pr_poll_tx: Sender<PrPollResult>,
    pub(crate) pr_poll_rx: Receiver<PrPollResult>,
    pub(crate) pr_polls_in_flight: BTreeSet<PrPollKey>,
    pub(crate) tmux_warmup_tx: Sender<AgentSessionWarmupResult>,
    pub(crate) tmux_warmup_rx: Receiver<AgentSessionWarmupResult>,
    pub(crate) tmux_warmups_in_flight: BTreeSet<AgentSessionWarmupKey>,
    pub(crate) tmux_generations: BTreeMap<AgentSessionSlot, u64>,
    pub(crate) wt_poll_tx: Sender<WtPollResult>,
    pub(crate) wt_poll_rx: Receiver<WtPollResult>,
    pub(crate) default_branch_poll_tx: Sender<DefaultBranchPollResult>,
    pub(crate) default_branch_poll_rx: Receiver<DefaultBranchPollResult>,
    pub(crate) opencode_poll_tx: Sender<OpencodePollResult>,
    pub(crate) opencode_poll_rx: Receiver<OpencodePollResult>,
    pub(crate) opencode_polls_in_flight: BTreeSet<OpencodePollKey>,
    pub(crate) opencode_last_polled: BTreeMap<OpencodePollKey, Instant>,
    pub(crate) opencode_event_tx: Sender<OpencodeEventResult>,
    pub(crate) opencode_event_rx: Receiver<OpencodeEventResult>,
    pub(crate) opencode_sse_servers: BTreeSet<String>,
    pub(crate) repo_filter: String,
    pub(crate) worktree_filter: String,
    pub(crate) leader_hint: Option<LeaderHint>,
    status_message: Option<String>,
    status_message_until: Option<Instant>,
}

const STATUS_MESSAGE_DURATION: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub(crate) struct ManagedRepo {
    pub repo: Repository,
    pub config: Config,
    pub label: String,
    pub key: Option<char>,
    pub pr_summary_poll_in_flight: bool,
    pub pr_summary_last_polled: Option<std::time::Instant>,
    pub wt_poll_in_flight: bool,
    pub default_branch_poll_in_flight: bool,
    pub default_branch_last_polled: Option<std::time::Instant>,
}

#[derive(Clone)]
pub(crate) struct SelectedRepoContext {
    pub repo_index: usize,
    pub repo: Repository,
    pub config: Config,
}

#[derive(Clone)]
pub(crate) struct SelectedWorktreeContext {
    pub session_index: usize,
    pub repo: Repository,
    pub config: Config,
}

impl ManagedRepo {
    pub(crate) fn new(repo: Repository, config: Config, key: Option<char>) -> Self {
        let label = crate::workspace::label_for_root(&repo.root);
        Self {
            repo,
            config,
            label,
            key,
            pr_summary_poll_in_flight: false,
            pr_summary_last_polled: None,
            wt_poll_in_flight: false,
            default_branch_poll_in_flight: false,
            default_branch_last_polled: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct PrPollKey {
    pub repo_index: usize,
    pub branch: String,
    pub path: PathBuf,
}

pub(crate) enum PrPollResult {
    Summary {
        repo_index: usize,
        summaries: Result<Vec<PrSummary>, String>,
        poll_started_at: Instant,
    },
    Details {
        key: PrPollKey,
        cache: Box<PrCache>,
    },
}

impl PrPollKey {
    pub(crate) fn for_session(session: &Session) -> Self {
        Self {
            repo_index: session.repo_index,
            branch: session.branch.clone(),
            path: session.path.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LeaderHint {
    Root,
    Git,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PanelFocus {
    Status,
    Repos,
    Worktrees,
}

pub(crate) struct WtPollResult {
    pub repo_index: usize,
    pub columns: Result<BTreeMap<PathBuf, BTreeMap<String, String>>, String>,
}

pub(crate) struct DefaultBranchPollResult {
    pub repo_index: usize,
    pub branch: String,
    pub path: PathBuf,
    pub status_label: Result<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct OpencodePollKey {
    pub repo_index: usize,
    pub branch: String,
    pub path: PathBuf,
}

pub(crate) struct OpencodePollResult {
    pub key: OpencodePollKey,
    pub status: Result<OpencodeStatus, String>,
}

pub(crate) struct OpencodeEventResult {
    pub server_url: String,
    pub event: Result<OpencodeEvent, String>,
}

impl OpencodePollKey {
    pub(crate) fn for_session(session: &Session) -> Self {
        Self {
            repo_index: session.repo_index,
            branch: session.branch.clone(),
            path: session.path.clone(),
        }
    }
}

#[derive(Default)]
struct TuiBackgroundChanges {
    tmux: bool,
    worktree_columns: bool,
    default_branch: bool,
    opencode_status: bool,
    opencode_events: bool,
    pull_requests: bool,
    status_message: bool,
    resized: bool,
}

impl TuiBackgroundChanges {
    fn any(&self) -> bool {
        self.tmux
            || self.worktree_columns
            || self.default_branch
            || self.opencode_status
            || self.opencode_events
            || self.pull_requests
            || self.status_message
            || self.resized
    }
}

impl Tui {
    pub fn new(repos: Vec<ManagedRepo>, current_repo: usize, sessions: Vec<Session>) -> Self {
        let (pr_poll_tx, pr_poll_rx) = mpsc::channel();
        let (tmux_warmup_tx, tmux_warmup_rx) = mpsc::channel();
        let (wt_poll_tx, wt_poll_rx) = mpsc::channel();
        let (default_branch_poll_tx, default_branch_poll_rx) = mpsc::channel();
        let (opencode_poll_tx, opencode_poll_rx) = mpsc::channel();
        let (opencode_event_tx, opencode_event_rx) = mpsc::channel();
        let current_repo = current_repo.min(repos.len().saturating_sub(1));
        let fallback_repo = Repository {
            root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        };
        let repo = repos
            .get(current_repo)
            .map(|repo| repo.repo.clone())
            .unwrap_or_else(|| fallback_repo.clone());
        let config = repos
            .get(current_repo)
            .map(|repo| repo.config.clone())
            .unwrap_or_else(|| Config::load(&fallback_repo));
        let mut tui = Self {
            repo,
            config,
            repos,
            current_repo,
            sessions,
            selected: 0,
            selected_repo_root: None,
            focused_panel: PanelFocus::Repos,
            repo_main_view: view::RepoMainView::Github,
            selected_worktree_by_repo: BTreeMap::new(),
            pr_poll_tx,
            pr_poll_rx,
            pr_polls_in_flight: BTreeSet::new(),
            tmux_warmup_tx,
            tmux_warmup_rx,
            tmux_warmups_in_flight: BTreeSet::new(),
            tmux_generations: BTreeMap::new(),
            wt_poll_tx,
            wt_poll_rx,
            default_branch_poll_tx,
            default_branch_poll_rx,
            opencode_poll_tx,
            opencode_poll_rx,
            opencode_polls_in_flight: BTreeSet::new(),
            opencode_last_polled: BTreeMap::new(),
            opencode_event_tx,
            opencode_event_rx,
            opencode_sse_servers: BTreeSet::new(),
            repo_filter: String::new(),
            worktree_filter: String::new(),
            leader_hint: None,
            status_message: None,
            status_message_until: None,
        };
        tui.selected_repo_root = tui
            .repos
            .get(tui.current_repo)
            .map(|repo| repo.repo.root.clone());
        tui.ensure_navigation_valid();
        tui
    }

    #[cfg(test)]
    pub(crate) fn new_single(repo: Repository, config: Config, sessions: Vec<Session>) -> Self {
        Self::new(vec![ManagedRepo::new(repo, config, None)], 0, sessions)
    }

    pub(crate) fn sync_selected_repo_context(&mut self) {
        self.current_repo = self.current_repo.min(self.repos.len().saturating_sub(1));
        if let Some(repo) = self.repos.get(self.current_repo) {
            self.repo = repo.repo.clone();
            self.config = repo.config.clone();
        }
    }

    pub(crate) fn selected_repo_context(&self) -> Option<SelectedRepoContext> {
        let managed = self.repos.get(self.current_repo)?;
        Some(SelectedRepoContext {
            repo_index: self.current_repo,
            repo: managed.repo.clone(),
            config: managed.config.clone(),
        })
    }

    pub(crate) fn selected_worktree_context(&self) -> Option<SelectedWorktreeContext> {
        let session_index = self.selected_worktree_index()?;
        let session = self.sessions.get(session_index)?;
        let managed = self.repos.get(session.repo_index)?;
        Some(SelectedWorktreeContext {
            session_index,
            repo: managed.repo.clone(),
            config: managed.config.clone(),
        })
    }

    pub fn run(&mut self) -> Result<(), String> {
        if !stdin_is_tty() {
            return Err("TUI requires an interactive terminal".to_string());
        }

        let mut raw = RawTerminal::enter()?;
        self.start_tmux_agent_warmup();
        self.start_wt_column_poll();
        self.start_default_branch_status_poll(true);
        self.start_opencode_status_poll(true);
        self.start_opencode_event_listeners();
        self.draw()?;
        if self.repos.is_empty() {
            match self.add_repository() {
                Ok(()) => {}
                Err(error) => self.show_error("add repository failed", &error)?,
            }
        }
        let mut stdin = io::stdin();
        let mut buffer = [0_u8; 64];
        let mut key_input = KeyInput::default();
        let mut pending_g = false;
        let mut last_size = terminal_size();

        loop {
            if self.tick_tui_action_jobs(&mut last_size).any() {
                self.draw()?;
            }
            let count = match stdin.read(&mut buffer) {
                Ok(count) => count,
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    continue;
                }
                Err(error) => return Err(error.to_string()),
            };
            if count == 0 {
                continue;
            }

            let mut should_quit = false;
            for key in key_input.feed(&buffer[..count]) {
                match key {
                    Key::Quit => {
                        self.clear_leader_hint();
                        pending_g = false;
                        should_quit = self.confirm_quit()?;
                    }
                    Key::Down => {
                        self.clear_leader_hint();
                        self.move_down();
                        pending_g = false;
                    }
                    Key::Left => {
                        self.clear_leader_hint();
                        self.move_left();
                        pending_g = false;
                    }
                    Key::Right => {
                        self.clear_leader_hint();
                        self.move_right();
                        pending_g = false;
                    }
                    Key::FocusNext => {
                        self.clear_leader_hint();
                        self.focus_next_panel();
                        pending_g = false;
                    }
                    Key::FocusStatus => {
                        self.clear_leader_hint();
                        self.focus_status();
                        pending_g = false;
                    }
                    Key::FocusRepos => {
                        self.clear_leader_hint();
                        self.focus_repos();
                        pending_g = false;
                    }
                    Key::FocusWorktrees => {
                        self.clear_leader_hint();
                        self.focus_worktrees();
                        pending_g = false;
                    }
                    Key::Up => {
                        self.clear_leader_hint();
                        self.move_up();
                        pending_g = false;
                    }
                    Key::Bottom => {
                        self.clear_leader_hint();
                        pending_g = false;
                        self.select_bottom_visible();
                    }
                    Key::G => {
                        self.clear_leader_hint();
                        if pending_g {
                            self.select_top_visible();
                            pending_g = false;
                        } else {
                            pending_g = true;
                        }
                    }
                    Key::Leader => {
                        self.leader_hint = Some(LeaderHint::Root);
                    }
                    Key::LeaderGit => {
                        self.leader_hint = Some(LeaderHint::Git);
                    }
                    Key::OpenTmuxSession => {
                        self.clear_leader_hint();
                        pending_g = false;
                        match self.focused_panel {
                            PanelFocus::Status => self.focus_repos(),
                            PanelFocus::Repos => {
                                if self.visible_session_indices().is_empty() {
                                    self.show_message(
                                        "selected repository has no visible worktrees",
                                    )?;
                                } else {
                                    self.focus_worktrees();
                                }
                            }
                            PanelFocus::Worktrees => self.enter_agent_mode(&mut raw)?,
                        }
                    }
                    Key::LazyGit => {
                        self.clear_leader_hint();
                        pending_g = false;
                        if self.focused_panel == PanelFocus::Status {
                            self.show_message("focus repos or worktrees to open lazygit")?;
                        } else if self.focused_panel == PanelFocus::Repos {
                            if let Err(error) = self.open_selected_repo_lazygit(&mut raw) {
                                self.show_error("repository lazygit failed", &error)?;
                            }
                        } else if let Err(error) =
                            self.open_tmux_window(&mut raw, TmuxWindow::LazyGit)
                        {
                            self.show_error("lazygit failed", &error)?;
                        }
                    }
                    Key::OpenPr => {
                        self.clear_leader_hint();
                        pending_g = false;
                        if self.focused_panel != PanelFocus::Worktrees {
                            self.show_message("focus worktrees to open a PR")?;
                        } else if let Err(error) = self.open_selected_pr() {
                            self.show_error("open PR failed", &error)?;
                        }
                    }
                    Key::Terminal => {
                        self.clear_leader_hint();
                        pending_g = false;
                        if self.focused_panel == PanelFocus::Status {
                            self.show_message("focus repos or worktrees to open a terminal")?;
                        } else if self.focused_panel == PanelFocus::Repos {
                            if let Err(error) = self.open_selected_repo_terminal(&mut raw) {
                                self.show_error("repository terminal failed", &error)?;
                            }
                        } else if let Err(error) =
                            self.open_tmux_window(&mut raw, TmuxWindow::Terminal)
                        {
                            self.show_error("terminal failed", &error)?;
                        }
                    }
                    Key::Help => {
                        self.clear_leader_hint();
                        pending_g = false;
                        self.show_keybindings_dialog()?;
                    }
                    Key::Refresh => {
                        self.clear_leader_hint();
                        pending_g = false;
                        self.refresh_sessions()?;
                        self.start_tmux_agent_warmup();
                        self.start_wt_column_poll();
                        self.start_default_branch_status_poll(true);
                        self.start_opencode_status_poll(true);
                        self.start_opencode_event_listeners();
                        self.poll_pull_requests(true);
                    }
                    Key::RepoShortcut(key) => {
                        self.clear_leader_hint();
                        pending_g = false;
                        if let Err(error) = self.select_repo_by_key(key) {
                            self.show_error("select repository failed", &error)?;
                        }
                    }
                    Key::ReviewFix => {
                        self.clear_leader_hint();
                        pending_g = false;
                        if self.focused_panel != PanelFocus::Worktrees {
                            self.show_message("focus worktrees to stage a review-fix prompt")?;
                        } else if let Err(error) = self.start_review_fix() {
                            self.show_error("review fix failed", &error)?;
                        }
                    }
                    Key::CiFix => {
                        self.clear_leader_hint();
                        pending_g = false;
                        if self.focused_panel != PanelFocus::Worktrees {
                            self.show_message("focus worktrees to copy a CI-failure prompt")?;
                        } else if let Err(error) = self.start_ci_fix() {
                            self.show_error("CI failure prompt failed", &error)?;
                        }
                    }
                    Key::Push => {
                        self.clear_leader_hint();
                        pending_g = false;
                        if self.focused_panel != PanelFocus::Worktrees {
                            self.show_message("focus worktrees to push a branch")?;
                        } else if let Err(error) = self.push_selected_branch() {
                            self.show_error("push failed", &error)?;
                        }
                    }
                    Key::Merge => {
                        self.clear_leader_hint();
                        pending_g = false;
                        if self.focused_panel != PanelFocus::Worktrees {
                            self.show_message("focus worktrees to merge a PR")?;
                        } else if let Err(error) = self.merge_selected_pr() {
                            self.show_error("merge failed", &error)?;
                        }
                    }
                    Key::PullDefault => {
                        self.clear_leader_hint();
                        pending_g = false;
                        if self.focused_panel == PanelFocus::Status {
                            self.show_message(
                                "focus repos or worktrees to pull the default branch",
                            )?;
                        } else if let Err(error) = self.pull_default_branch() {
                            self.show_error("pull failed", &error)?;
                        }
                    }
                    Key::PlanMode => {
                        self.clear_leader_hint();
                        pending_g = false;
                        if self.focused_panel == PanelFocus::Status {
                            self.show_message("focus repos or worktrees to run plan mode")?;
                        } else if self.focused_panel == PanelFocus::Repos {
                            if let Err(error) = self.open_selected_repo_plan_mode(&mut raw) {
                                self.show_error("plan mode failed", &error)?;
                            }
                        } else if let Err(error) = self.open_selected_worktree_plan_mode(&mut raw) {
                            self.show_error("plan mode failed", &error)?;
                        }
                    }
                    Key::Create => {
                        self.clear_leader_hint();
                        pending_g = false;
                        match self.create_session() {
                            Ok(true) => self.focus_worktrees(),
                            Ok(false) => {}
                            Err(error) => self.show_error("create session failed", &error)?,
                        }
                    }
                    Key::AbortOpencode => {
                        self.clear_leader_hint();
                        pending_g = false;
                        if self.focused_panel != PanelFocus::Worktrees {
                            self.show_message("focus worktrees to abort an OpenCode session")?;
                        } else if let Err(error) = self.abort_selected_opencode_session() {
                            self.show_error("abort failed", &error)?;
                        }
                    }
                    Key::AddRepo => {
                        self.clear_leader_hint();
                        pending_g = false;
                        if self.focused_panel != PanelFocus::Repos {
                            self.show_message("focus repos to add a repository")?;
                        } else if let Err(error) = self.add_repository() {
                            self.show_error("add repository failed", &error)?;
                        }
                    }
                    Key::ManageRepos => {
                        self.clear_leader_hint();
                        pending_g = false;
                        if let Err(error) = self.edit_repositories(&mut raw) {
                            self.show_error("edit repositories failed", &error)?;
                        }
                    }
                    Key::Delete => {
                        self.clear_leader_hint();
                        pending_g = false;
                        if self.focused_panel == PanelFocus::Status {
                            self.show_message("focus worktrees to delete a worktree/session")?;
                        } else if self.focused_panel == PanelFocus::Repos {
                            self.show_message("repository removal is available from R")?;
                        } else if let Err(error) = self.delete_session() {
                            self.show_error("delete failed", &error)?;
                        }
                    }
                    Key::EditConfig => {
                        self.clear_leader_hint();
                        pending_g = false;
                        if self.focused_panel != PanelFocus::Repos {
                            self.show_message("focus repos to edit repository config")?;
                        } else if let Err(error) = self.edit_config(&mut raw) {
                            self.show_error("edit config failed", &error)?;
                        }
                    }
                    Key::Search => {
                        self.clear_leader_hint();
                        pending_g = false;
                        self.search_sessions()?;
                    }
                    Key::Other => {
                        self.clear_leader_hint();
                        pending_g = false;
                    }
                }
                if should_quit {
                    break;
                }
            }
            if should_quit {
                break;
            }
            self.draw()?;
        }
        self.shutdown_owned_opencode_servers();
        Ok(())
    }

    fn tick_tui_action_jobs(&mut self, last_size: &mut (u16, u16)) -> TuiBackgroundChanges {
        let mut changes = TuiBackgroundChanges {
            tmux: self.poll_tmux_agent_warmup(),
            worktree_columns: self.poll_wt_columns(),
            default_branch: self.poll_default_branch_status(),
            opencode_status: self.poll_opencode_status(),
            opencode_events: self.poll_opencode_events(),
            pull_requests: self.poll_pull_requests(false),
            status_message: self.expire_status_message(),
            ..TuiBackgroundChanges::default()
        };
        self.start_default_branch_status_poll(false);
        self.start_opencode_status_poll(false);
        self.start_opencode_event_listeners();
        let current_size = terminal_size();
        changes.resized = current_size != *last_size;
        if changes.resized {
            *last_size = current_size;
        }
        changes
    }

    fn confirm_quit(&self) -> Result<bool, String> {
        if !self
            .sessions
            .iter()
            .any(|session| session.agent_state == AgentState::Running)
        {
            return Ok(true);
        }
        let answer = self.prompt_line("Agents are running. Quit Prism? [y/N] ")?;
        Ok(yes(&answer))
    }

    fn enter_agent_mode(&mut self, raw: &mut RawTerminal) -> Result<(), String> {
        let Some(context) = self.selected_worktree_context() else {
            return Ok(());
        };
        if context
            .config
            .is_default_branch(&self.sessions[context.session_index].branch)
        {
            self.show_message("default branch does not have an agent session")?;
            return Ok(());
        }
        raw.suspend()?;
        let result = self.attach_selected_tmux_session();
        let resume_result = raw.resume();
        self.refresh_sessions()?;
        self.start_tmux_agent_warmup();
        resume_result?;
        if let Err(error) = result {
            self.show_error("tmux session failed", &error)?;
        }
        Ok(())
    }

    fn open_tmux_window(
        &mut self,
        raw: &mut RawTerminal,
        window: TmuxWindow,
    ) -> Result<(), String> {
        if self.selected >= self.sessions.len() {
            return Ok(());
        }
        raw.suspend()?;
        let result = self.attach_selected_tmux_window(window);
        let resume_result = raw.resume();
        self.refresh_sessions()?;
        self.start_tmux_agent_warmup();
        resume_result?;
        result
    }

    fn show_keybindings_dialog(&self) -> Result<(), String> {
        let items = [
            "1 / 2 / 3    focus status / repos / worktrees",
            "Tab          move focus between panels",
            "h/l, left/right arrows  switch horizontal view in repos",
            "Space Space  status: focus repos; repos: focus worktrees; worktrees: open agent if valid",
            "Enter        status: focus repos; repos: focus worktrees; worktrees: open agent if valid",
            "Space Enter  open tmux window 3: terminal",
            "Space g g    open tmux window 2: lazygit",
            "Ctrl-/       open tmux window 3: terminal",
            "Space g o    open selected PR in browser",
            "Space g P    push branch, create PR if needed",
            "Space g M    merge selected PR",
            "Space g c    copy CI-failure prompt",
            "Space g f    stage review-fix prompt",
            "Space g p    repos/worktrees: pull default branch",
            "p            repos/worktrees: pull default branch",
            "P            repos/worktrees: run plan mode",
            "A            add repository",
            "R            edit repositories/order/keys/remove",
            "c            create worktree session in selected repo",
            "x            worktrees: abort selected OpenCode session",
            "e            repos: edit Prism repo config, then reload",
            "/            search/filter focused panel",
            "?            show keybindings; / filters this dialog",
            "D            delete non-default worktree/session",
            "j/k, up/down move selection",
            "g g / G      top / bottom",
            "r            refresh",
            "q, Ctrl-C    quit",
        ];
        let mut filter = String::new();
        let mut editing_filter = false;
        self.draw_keybindings_dialog(&items, &filter)?;

        let mut stdin = io::stdin();
        let mut byte = [0_u8; 1];
        loop {
            match stdin.read(&mut byte) {
                Ok(1) => {}
                Ok(_) => {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                    continue;
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                    continue;
                }
                Err(error) => return Err(error.to_string()),
            }
            match byte[0] {
                b'/' if !editing_filter => {
                    editing_filter = true;
                    filter.clear();
                    self.draw_keybindings_dialog(&items, &filter)?;
                }
                b'\r' | b'\n' if editing_filter => editing_filter = false,
                8 | 127 if editing_filter => {
                    filter.pop();
                    self.draw_keybindings_dialog(&items, &filter)?;
                }
                3 | 27 | b'q' => return Ok(()),
                byte if editing_filter && !byte.is_ascii_control() => {
                    filter.push(byte as char);
                    self.draw_keybindings_dialog(&items, &filter)?;
                }
                _ if !editing_filter => return Ok(()),
                _ => {}
            }
        }
    }

    fn draw_keybindings_dialog(&self, items: &[&str], filter: &str) -> Result<(), String> {
        let query = filter.trim().to_ascii_lowercase();
        let mut lines = vec!["Keybindings".to_string()];
        lines.push(if filter.is_empty() {
            "Filter: /".to_string()
        } else {
            format!("Filter: /{filter}")
        });
        lines.push(String::new());
        lines.extend(
            items
                .iter()
                .filter(|line| query.is_empty() || line.to_ascii_lowercase().contains(&query))
                .map(|line| (*line).to_string()),
        );
        if lines.len() == 3 {
            lines.push("No matching keybindings".to_string());
        }
        lines.extend([String::new(), "Esc/q closes. / searches.".to_string()]);
        let (cols, rows) = terminal_size();
        let available_width = (cols as usize).saturating_sub(2).max(4);
        let width = lines
            .iter()
            .map(|line| line.chars().count())
            .max()
            .unwrap_or(0)
            .saturating_add(4)
            .max(24)
            .min(available_width);
        let height = lines.len() + 2;
        let left = ((cols as usize).saturating_sub(width) / 2).saturating_add(1);
        let top = ((rows as usize).saturating_sub(height) / 2).saturating_add(1);

        let mut frame = format!(
            "\x1b[?25l\x1b[{top};{left}H+{}+",
            "-".repeat(width.saturating_sub(2))
        );
        for (index, line) in lines.iter().enumerate() {
            let y = top + index + 1;
            let text_width = width.saturating_sub(4);
            let text = truncate_line(line, text_width);
            frame.push_str(&format!(
                "\x1b[{y};{left}H| {:<text_width$} |",
                text,
                text_width = text_width
            ));
        }
        frame.push_str(&format!(
            "\x1b[{};{}H+{}+",
            top + height - 1,
            left,
            "-".repeat(width.saturating_sub(2))
        ));
        write_stdout(&frame)
    }

    pub(crate) fn confirm_delete_dialog(
        &self,
        branch: &str,
        path: &str,
        warnings: &[String],
    ) -> Result<bool, String> {
        let mut lines = vec![
            "Delete Session".to_string(),
            String::new(),
            format!("branch: {branch}"),
            format!("path: {path}"),
            String::new(),
        ];
        if warnings.is_empty() {
            lines.push("No warnings detected.".to_string());
        } else {
            lines.push("Warnings".to_string());
            for warning in warnings {
                lines.push(format!("\x1b[31m•\x1b[0m {warning}"));
            }
        }
        lines.extend([
            String::new(),
            "Enter confirms delete. Esc/q cancels.".to_string(),
        ]);

        let (cols, rows) = terminal_size();
        let available_width = (cols as usize).saturating_sub(2).max(4);
        let width = lines
            .iter()
            .map(|line| strip_ansi(line).chars().count())
            .max()
            .unwrap_or(0)
            .saturating_add(4)
            .max(42)
            .min(available_width);
        let height = lines.len() + 2;
        let left = ((cols as usize).saturating_sub(width) / 2).saturating_add(1);
        let top = ((rows as usize).saturating_sub(height) / 2).saturating_add(1);

        print!("\x1b[?25l");
        print!(
            "\x1b[{top};{left}H+{}+",
            "-".repeat(width.saturating_sub(2))
        );
        for (index, line) in lines.iter().enumerate() {
            let y = top + index + 1;
            let text_width = width.saturating_sub(4);
            let text = truncate_line(&strip_ansi(line), text_width);
            let text = if line.contains("\x1b[") {
                truncate_ansi_dialog_line(line, text_width)
            } else {
                text
            };
            print!(
                "\x1b[{y};{left}H| {}{} |",
                text,
                " ".repeat(text_width.saturating_sub(strip_ansi(&text).chars().count()))
            );
        }
        print!(
            "\x1b[{};{}H+{}+",
            top + height - 1,
            left,
            "-".repeat(width.saturating_sub(2))
        );
        io::stdout().flush().map_err(|error| error.to_string())?;

        let mut stdin = io::stdin();
        let mut byte = [0_u8; 1];
        loop {
            match stdin.read(&mut byte) {
                Ok(1) => match byte[0] {
                    b'\r' | b'\n' => return Ok(true),
                    3 | 27 | b'q' => return Ok(false),
                    _ => {}
                },
                Ok(_) => std::thread::sleep(std::time::Duration::from_millis(25)),
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                Err(error) => return Err(error.to_string()),
            }
        }
    }

    pub(crate) fn prompt_line(&self, prompt: &str) -> Result<String, String> {
        self.prompt_line_with_initial(prompt, "")
    }

    pub(crate) fn prompt_line_dialog(
        &self,
        title: &str,
        prompt: &str,
        initial: &str,
    ) -> Result<Option<String>, String> {
        let mut input = initial.to_string();
        self.draw()?;
        self.draw_prompt_dialog(title, prompt, &input)?;
        let mut stdin = io::stdin();
        let mut byte = [0_u8; 1];
        loop {
            match stdin.read(&mut byte) {
                Ok(1) => {}
                Ok(_) => {
                    std::thread::sleep(Duration::from_millis(25));
                    continue;
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(25));
                    continue;
                }
                Err(error) => return Err(error.to_string()),
            }
            match byte[0] {
                b'\r' | b'\n' => {
                    write_stdout("\x1b[?25l")?;
                    return Ok(Some(input));
                }
                3 | 27 => {
                    write_stdout("\x1b[?25l")?;
                    return Ok(None);
                }
                8 | 127 => {
                    input.pop();
                    self.draw_prompt_dialog(title, prompt, &input)?;
                }
                byte if !byte.is_ascii_control() => {
                    input.push(byte as char);
                    self.draw_prompt_dialog(title, prompt, &input)?;
                }
                _ => {}
            }
        }
    }

    pub(crate) fn show_loading_dialog(&self, title: &str, message: &str) -> Result<(), String> {
        self.draw()?;
        self.draw_static_dialog(title, &["[*] Please wait", message])
    }

    fn draw_prompt_dialog(&self, title: &str, prompt: &str, input: &str) -> Result<(), String> {
        let (cols, rows) = terminal_size();
        let prompt_len = prompt.chars().count();
        let requested_width = title
            .chars()
            .count()
            .max(prompt_len.saturating_add(input.chars().count()))
            .saturating_add(4)
            .max(44);
        let width = requested_width.min((cols as usize).saturating_sub(2).max(12));
        let text_width = width.saturating_sub(4);
        let max_input_width = text_width.saturating_sub(prompt_len);
        let input_display = tail_chars(input, max_input_width);
        let input_line = format!("{prompt}{input_display}");
        let cursor_col = prompt_len
            .saturating_add(input_display.chars().count())
            .min(text_width);
        self.draw_dialog_frame(
            title,
            &[
                String::new(),
                input_line,
                "Enter to continue, Esc to cancel".to_string(),
            ],
            Some((1, cursor_col)),
            width,
            rows,
            cols,
        )
    }

    fn draw_static_dialog(&self, title: &str, lines: &[&str]) -> Result<(), String> {
        let (cols, rows) = terminal_size();
        let requested_width = lines
            .iter()
            .map(|line| line.chars().count())
            .chain(std::iter::once(title.chars().count()))
            .max()
            .unwrap_or(0)
            .saturating_add(4)
            .max(44);
        let width = requested_width.min((cols as usize).saturating_sub(2).max(12));
        let owned_lines = lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>();
        self.draw_dialog_frame(title, &owned_lines, None, width, rows, cols)
    }

    fn draw_dialog_frame(
        &self,
        title: &str,
        lines: &[String],
        cursor: Option<(usize, usize)>,
        width: usize,
        rows: u16,
        cols: u16,
    ) -> Result<(), String> {
        let height = lines.len() + 3;
        let left = ((cols as usize).saturating_sub(width) / 2).saturating_add(1);
        let top = ((rows as usize).saturating_sub(height) / 3).saturating_add(1);
        let text_width = width.saturating_sub(4);
        let mut frame = format!(
            "\x1b[?25l\x1b[{top};{left}H+{}+",
            "-".repeat(width.saturating_sub(2))
        );
        let title_line = truncate_line(title, text_width);
        frame.push_str(&format!(
            "\x1b[{};{}H| {:<text_width$} |",
            top + 1,
            left,
            title_line,
            text_width = text_width
        ));
        for (index, line) in lines.iter().enumerate() {
            let y = top + index + 2;
            let text = truncate_line(line, text_width);
            frame.push_str(&format!(
                "\x1b[{y};{left}H| {:<text_width$} |",
                text,
                text_width = text_width
            ));
        }
        frame.push_str(&format!(
            "\x1b[{};{}H+{}+",
            top + height - 1,
            left,
            "-".repeat(width.saturating_sub(2))
        ));
        if let Some((line_index, cursor_col)) = cursor {
            frame.push_str(&format!(
                "\x1b[{};{}H\x1b[?25h",
                top + line_index + 2,
                left + 2 + cursor_col
            ));
        }
        write_stdout(&frame)
    }

    fn prompt_line_with_initial(&self, prompt: &str, initial: &str) -> Result<String, String> {
        let mut input = initial.to_string();
        write_stdout(&format!(
            "\x1b[{};1H\x1b[2K\x1b[?25h{}{}",
            terminal_size().1,
            prompt,
            input
        ))?;
        let mut stdin = io::stdin();
        let mut byte = [0_u8; 1];
        loop {
            match stdin.read(&mut byte) {
                Ok(1) => {}
                Ok(_) => {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                    continue;
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                    continue;
                }
                Err(error) => return Err(error.to_string()),
            }
            match byte[0] {
                b'\r' | b'\n' => {
                    write_stdout("\r\n\x1b[?25l")?;
                    return Ok(input);
                }
                3 | 27 => {
                    write_stdout("\r\n\x1b[?25l")?;
                    return Ok(String::new());
                }
                8 | 127 => {
                    if input.pop().is_some() {
                        write_stdout("\x08 \x08")?;
                    }
                }
                byte if !byte.is_ascii_control() => {
                    let ch = byte as char;
                    input.push(ch);
                    write_stdout(&ch.to_string())?;
                }
                _ => {}
            }
        }
    }

    pub(crate) fn show_message(&mut self, message: &str) -> Result<(), String> {
        self.status_message = Some(message.to_string());
        self.status_message_until = Some(Instant::now() + STATUS_MESSAGE_DURATION);
        let _ = append_runtime_log(&self.repo, message);
        self.draw()
    }

    fn show_error(&mut self, context: &str, error: &str) -> Result<(), String> {
        let message = format!("{context}: {error}");
        self.show_message(&message)
    }

    fn move_down(&mut self) {
        match self.focused_panel {
            PanelFocus::Status => {}
            PanelFocus::Repos => self.move_repo_selection(1),
            PanelFocus::Worktrees => self.move_worktree_selection(1),
        }
    }

    fn move_up(&mut self) {
        match self.focused_panel {
            PanelFocus::Status => {}
            PanelFocus::Repos => self.move_repo_selection(-1),
            PanelFocus::Worktrees => self.move_worktree_selection(-1),
        }
    }

    fn move_left(&mut self) {
        if self.focused_panel == PanelFocus::Repos {
            self.repo_main_view = view::RepoMainView::Github;
        }
    }

    fn move_right(&mut self) {
        if self.focused_panel == PanelFocus::Repos {
            self.repo_main_view = view::RepoMainView::Kanban;
        }
    }

    fn focus_next_panel(&mut self) {
        self.focused_panel = match self.focused_panel {
            PanelFocus::Status => PanelFocus::Repos,
            PanelFocus::Repos => PanelFocus::Worktrees,
            PanelFocus::Worktrees => PanelFocus::Status,
        };
    }

    fn focus_status(&mut self) {
        self.focused_panel = PanelFocus::Status;
    }

    fn focus_repos(&mut self) {
        self.focused_panel = PanelFocus::Repos;
    }

    fn focus_worktrees(&mut self) {
        self.focused_panel = PanelFocus::Worktrees;
    }

    fn move_repo_selection(&mut self, direction: isize) {
        let indices = self.visible_repo_indices();
        let current = indices
            .iter()
            .position(|index| *index == self.current_repo)
            .unwrap_or(0);
        let next = current as isize + direction;
        if next < 0 {
            return;
        }
        if let Some(repo_index) = indices.get(next as usize).copied() {
            self.select_repo(repo_index);
        }
    }

    fn move_worktree_selection(&mut self, direction: isize) {
        let indices = self.visible_session_indices();
        let current = indices
            .iter()
            .position(|index| *index == self.selected)
            .unwrap_or(0);
        let next = current as isize + direction;
        if next < 0 {
            return;
        }
        if let Some(next) = indices.get(next as usize).copied() {
            self.select_worktree(next);
        }
    }

    pub(crate) fn select_top_visible(&mut self) {
        match self.focused_panel {
            PanelFocus::Status => {}
            PanelFocus::Repos => {
                if let Some(index) = self.visible_repo_indices().first().copied() {
                    self.select_repo(index);
                }
            }
            PanelFocus::Worktrees => {
                if let Some(index) = self.visible_session_indices().first().copied() {
                    self.select_worktree(index);
                }
            }
        }
    }

    fn select_bottom_visible(&mut self) {
        match self.focused_panel {
            PanelFocus::Status => {}
            PanelFocus::Repos => {
                if let Some(index) = self.visible_repo_indices().last().copied() {
                    self.select_repo(index);
                }
            }
            PanelFocus::Worktrees => {
                if let Some(index) = self.visible_session_indices().last().copied() {
                    self.select_worktree(index);
                }
            }
        }
    }

    pub(crate) fn visible_repo_indices(&self) -> Vec<usize> {
        let filter = self.repo_filter.trim().to_ascii_lowercase();
        self.repos
            .iter()
            .enumerate()
            .filter_map(|(index, repo)| {
                (filter.is_empty()
                    || repo.label.to_ascii_lowercase().contains(&filter)
                    || repo
                        .repo
                        .root
                        .display()
                        .to_string()
                        .to_ascii_lowercase()
                        .contains(&filter)
                    || repo.key.is_some_and(|key| key.to_string() == filter))
                .then_some(index)
            })
            .collect()
    }

    pub(crate) fn visible_session_indices(&self) -> Vec<usize> {
        let filter = self.worktree_filter.trim().to_ascii_lowercase();
        self.sessions
            .iter()
            .enumerate()
            .filter_map(|(index, session)| {
                (session.repo_index == self.current_repo
                    && (filter.is_empty()
                        || session.branch.to_ascii_lowercase().contains(&filter)
                        || session
                            .prompt_summary
                            .to_ascii_lowercase()
                            .contains(&filter)
                        || session.path_display.to_ascii_lowercase().contains(&filter)
                        || session
                            .wt_columns
                            .values()
                            .any(|value| value.to_ascii_lowercase().contains(&filter))))
                .then_some(index)
            })
            .collect()
    }

    fn mark_selected_seen(&mut self) {
        if let Some(session) = self.sessions.get_mut(self.selected) {
            session.unseen_comments = false;
        }
    }

    pub(crate) fn select_worktree(&mut self, index: usize) {
        let Some(session) = self.sessions.get(index) else {
            return;
        };
        self.selected = index;
        if let Some(repo) = self.repos.get(session.repo_index) {
            self.selected_worktree_by_repo
                .insert(repo.repo.root.clone(), session.path.clone());
        }
        self.mark_selected_seen();
    }

    pub(crate) fn selected_worktree_index(&self) -> Option<usize> {
        let selected_is_current_repo = self
            .sessions
            .get(self.selected)
            .is_some_and(|session| session.repo_index == self.current_repo);
        (selected_is_current_repo && self.visible_session_indices().contains(&self.selected))
            .then_some(self.selected)
    }

    pub(crate) fn ensure_navigation_valid(&mut self) {
        if self.repos.is_empty() {
            self.current_repo = 0;
            self.selected_repo_root = None;
            self.selected = self.sessions.len();
            return;
        }
        if let Some(root) = &self.selected_repo_root
            && let Some(index) = self.repos.iter().position(|repo| repo.repo.root == *root)
        {
            self.current_repo = index;
        }
        self.current_repo = self.current_repo.min(self.repos.len().saturating_sub(1));
        if !self.visible_repo_indices().contains(&self.current_repo)
            && let Some(repo_index) = self.visible_repo_indices().first().copied()
        {
            self.current_repo = repo_index;
        }
        self.selected_repo_root = self
            .repos
            .get(self.current_repo)
            .map(|repo| repo.repo.root.clone());
        self.sync_selected_repo_context();
        self.restore_selected_worktree_for_repo();
    }

    fn restore_selected_worktree_for_repo(&mut self) {
        let indices = self.visible_session_indices();
        let remembered = self
            .repos
            .get(self.current_repo)
            .and_then(|repo| self.selected_worktree_by_repo.get(&repo.repo.root));
        if let Some(index) = remembered.and_then(|path| {
            indices.iter().copied().find(|index| {
                self.sessions
                    .get(*index)
                    .is_some_and(|session| session.path == *path)
            })
        }) {
            self.selected = index;
            return;
        }
        if indices.contains(&self.selected) {
            return;
        }
        self.selected = indices.first().copied().unwrap_or(self.sessions.len());
    }

    fn select_repo_by_key(&mut self, key: char) -> Result<(), String> {
        let Some(repo_index) = self.repos.iter().position(|repo| repo.key == Some(key)) else {
            self.show_message(&format!("no repository is bound to {key}"))?;
            return Ok(());
        };
        if !self.visible_repo_indices().contains(&repo_index) {
            self.repo_filter.clear();
        }
        self.select_repo(repo_index);
        Ok(())
    }

    pub(crate) fn select_repo(&mut self, repo_index: usize) {
        self.current_repo = repo_index.min(self.repos.len().saturating_sub(1));
        self.selected_repo_root = self
            .repos
            .get(self.current_repo)
            .map(|repo| repo.repo.root.clone());
        self.sync_selected_repo_context();
        self.restore_selected_worktree_for_repo();
    }

    fn clear_leader_hint(&mut self) {
        self.leader_hint = None;
    }

    fn search_sessions(&mut self) -> Result<(), String> {
        match self.focused_panel {
            PanelFocus::Status => {
                self.show_message("status panel has no filter")?;
            }
            PanelFocus::Repos => {
                let Some(input) = self.prompt_line_dialog(
                    "Search Repositories",
                    "Filter (empty clears): ",
                    &self.repo_filter,
                )?
                else {
                    return Ok(());
                };
                self.repo_filter = input;
                self.ensure_navigation_valid();
            }
            PanelFocus::Worktrees => {
                let Some(input) = self.prompt_line_dialog(
                    "Search Worktrees",
                    "Filter (empty clears): ",
                    &self.worktree_filter,
                )?
                else {
                    return Ok(());
                };
                self.worktree_filter = input;
                self.restore_selected_worktree_for_repo();
            }
        }
        Ok(())
    }

    fn expire_status_message(&mut self) -> bool {
        if self
            .status_message_until
            .is_some_and(|until| Instant::now() >= until)
        {
            self.status_message = None;
            self.status_message_until = None;
            return true;
        }
        false
    }

    fn draw(&self) -> Result<(), String> {
        let model = self.frame_model();
        view::draw_model(&model)
    }

    fn frame_model(&self) -> view::FrameModel<'_> {
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
                Some(view::WorktreeRow {
                    session_index: index,
                    repo_root,
                    worktree_path: session.path_display.clone(),
                    branch: session.branch.clone(),
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
                    pr: session.pr.clone(),
                    wt_columns: session.wt_columns.clone(),
                    unseen_comments: session.unseen_comments,
                    prompt_summary: session.prompt_summary.clone(),
                    selected: Some(index) == self.selected_worktree_index(),
                })
            })
            .collect::<Vec<_>>();
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
            current_repo_index: self.current_repo,
            selected_repo_label,
            selected_repo_root,
            selected_session: self.selected_worktree_index(),
            focus: self.focused_panel,
            repo_main_view: self.repo_main_view,
            mode_label: "normal",
            status_message: self.status_message.as_deref(),
            repo_filter: &self.repo_filter,
            worktree_filter: &self.worktree_filter,
            leader_hint: self.leader_hint_label(),
        }
    }

    fn repo_health_label(&self, repo_index: usize) -> String {
        let mut dirty = 0;
        let mut running = 0;
        let mut attention = 0;
        let mut prs = 0;
        let mut ci_failed = 0;
        let mut ci_running = 0;
        let mut behind = 0;
        for session in self
            .sessions
            .iter()
            .filter(|session| session.repo_index == repo_index)
        {
            if status_count(&session.status_label, "dirty").is_some() {
                dirty += 1;
            }
            if session.agent_state == AgentState::Running {
                running += 1;
            }
            if matches!(
                session.agent_state,
                AgentState::NeedsInput | AgentState::NeedsRestart | AgentState::ExitedError
            ) || session.unseen_comments
            {
                attention += 1;
            }
            if session.pr.summary.is_some() {
                prs += 1;
            }
            match session
                .pr
                .summary
                .as_ref()
                .map(|summary| summary.check_status.as_str())
            {
                Some("failed") => ci_failed += 1,
                Some("running") => ci_running += 1,
                _ => {}
            }
            if self
                .repos
                .get(repo_index)
                .is_some_and(|repo| repo.config.is_default_branch(&session.branch))
            {
                behind += status_count(&session.status_label, "behind").unwrap_or(0);
            }
        }

        let mut parts = Vec::new();
        if dirty > 0 {
            parts.push(format!("D{dirty}"));
        }
        if running > 0 {
            parts.push(format!("A{running}"));
        }
        if attention > 0 {
            parts.push(format!("!{attention}"));
        }
        if prs > 0 {
            parts.push(format!("PR{prs}"));
        }
        if ci_failed > 0 {
            parts.push(format!("CIx{ci_failed}"));
        } else if ci_running > 0 {
            parts.push(format!("CI~{ci_running}"));
        }
        if behind > 0 {
            parts.push(format!("↓{behind}"));
        }
        if parts.is_empty() {
            "ok".to_string()
        } else {
            parts.join(" ")
        }
    }

    fn status_rows(&self) -> Vec<view::StatusRow> {
        let mut running = 0;
        let mut attention = 0;
        let mut prs = 0;
        let mut ci_failed = 0;
        let mut ci_running = 0;
        let mut dirty = 0;
        let mut behind = 0;
        for session in &self.sessions {
            if status_count(&session.status_label, "dirty").is_some() {
                dirty += 1;
            }
            if session.agent_state == AgentState::Running {
                running += 1;
            }
            if matches!(
                session.agent_state,
                AgentState::NeedsInput | AgentState::NeedsRestart | AgentState::ExitedError
            ) || session.unseen_comments
            {
                attention += 1;
            }
            if session.pr.summary.is_some() {
                prs += 1;
            }
            match session
                .pr
                .summary
                .as_ref()
                .map(|summary| summary.check_status.as_str())
            {
                Some("failed") => ci_failed += 1,
                Some("running") => ci_running += 1,
                _ => {}
            }
            if self
                .repos
                .get(session.repo_index)
                .is_some_and(|repo| repo.config.is_default_branch(&session.branch))
            {
                behind += status_count(&session.status_label, "behind").unwrap_or(0);
            }
        }

        vec![
            view::StatusRow {
                label: "repos".to_string(),
                value: self.repos.len().to_string(),
                attention: false,
            },
            view::StatusRow {
                label: "worktrees".to_string(),
                value: self.sessions.len().to_string(),
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

    fn leader_hint_label(&self) -> Option<&'static str> {
        match (self.leader_hint, self.focused_panel) {
            (Some(LeaderHint::Root), PanelFocus::Status) => {
                Some("g: git  space/enter: focus repos")
            }
            (Some(LeaderHint::Root), PanelFocus::Repos) => {
                Some("g: git  space/enter: focus worktrees")
            }
            (Some(LeaderHint::Root), PanelFocus::Worktrees) => {
                Some("g: git  enter: terminal  space: agent if valid")
            }
            (Some(LeaderHint::Git), PanelFocus::Status) => {
                Some("g: lazygit after focusing repos/worktrees")
            }
            (Some(LeaderHint::Git), PanelFocus::Repos) => Some("p: pull default branch"),
            (Some(LeaderHint::Git), PanelFocus::Worktrees) => Some(
                "g: lazygit  p: pull default  o: open PR  P: push/create PR  M: merge  c: CI fix  f: review fix",
            ),
            (None, _) => None,
        }
    }
}

fn truncate_ansi_dialog_line(text: &str, max_chars: usize) -> String {
    let stripped = strip_ansi(text);
    if stripped.chars().count() <= max_chars {
        return text.to_string();
    }
    let warning_prefix = "\x1b[31m•\x1b[0m ";
    if let Some(rest) = text.strip_prefix(warning_prefix) {
        let visible_prefix = "• ";
        let prefix_width = visible_prefix.chars().count();
        if max_chars > prefix_width {
            return format!(
                "{warning_prefix}{}",
                truncate_line(rest, max_chars - prefix_width)
            );
        }
    }
    truncate_line(&stripped, max_chars)
}

fn tail_chars(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    if max_chars == 1 {
        return "~".to_string();
    }
    let mut out = String::from("~");
    out.extend(text.chars().skip(count - max_chars + 1));
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use crate::agent::AgentState;
    use crate::config::{Checks, Config, EscapeKey, MergeMethod};
    use crate::github::PrCache;
    use crate::repo::Repository;
    use crate::session::Session;
    use crate::view::RepoMainView;

    use super::{ManagedRepo, PanelFocus, Tui, truncate_ansi_dialog_line};

    #[test]
    fn truncated_warning_line_keeps_colored_bullet_prefix() {
        assert_eq!(
            truncate_ansi_dialog_line("\x1b[31m•\x1b[0m dirty worktree", 8),
            "\x1b[31m•\x1b[0m dirty~"
        );
    }

    #[test]
    fn tui_defaults_to_repos_panel_focus() {
        let tui = test_tui();

        assert_eq!(tui.focused_panel, PanelFocus::Repos);
    }

    #[test]
    fn switching_repos_restores_each_repos_selected_worktree() {
        let mut tui = test_tui();

        tui.select_worktree(1);
        tui.select_repo(1);
        tui.select_worktree(3);
        tui.select_repo(0);

        assert_eq!(tui.selected_worktree_index(), Some(1));

        tui.select_repo(1);

        assert_eq!(tui.selected_worktree_index(), Some(3));
    }

    #[test]
    fn worktree_filter_clear_restores_remembered_worktree() {
        let mut tui = test_tui();
        tui.select_worktree(1);

        tui.worktree_filter = "main".to_string();
        tui.restore_selected_worktree_for_repo();

        assert_eq!(tui.selected_worktree_index(), Some(0));

        tui.worktree_filter.clear();
        tui.restore_selected_worktree_for_repo();

        assert_eq!(tui.selected_worktree_index(), Some(1));
    }

    #[test]
    fn horizontal_keys_switch_repo_view_without_changing_focus() {
        let mut tui = test_tui();
        tui.focused_panel = PanelFocus::Repos;

        tui.move_right();

        assert_eq!(tui.focused_panel, PanelFocus::Repos);
        assert_eq!(tui.repo_main_view, RepoMainView::Kanban);

        tui.move_left();

        assert_eq!(tui.focused_panel, PanelFocus::Repos);
        assert_eq!(tui.repo_main_view, RepoMainView::Github);

        tui.focused_panel = PanelFocus::Worktrees;
        tui.move_left();

        assert_eq!(tui.focused_panel, PanelFocus::Worktrees);
        assert_eq!(tui.repo_main_view, RepoMainView::Github);
    }

    #[test]
    fn selected_repo_identity_survives_repo_reordering() {
        let mut tui = test_tui();
        tui.select_repo(1);
        tui.repos.swap(0, 1);
        for session in &mut tui.sessions {
            session.repo_index = 1 - session.repo_index;
        }

        tui.ensure_navigation_valid();

        assert_eq!(tui.current_repo, 0);
        assert_eq!(
            tui.selected_repo_context().unwrap().repo.root,
            PathBuf::from("/repo-two")
        );
    }

    fn test_tui() -> Tui {
        let repos = vec![
            ManagedRepo::new(
                Repository {
                    root: PathBuf::from("/repo-one"),
                },
                test_config(),
                Some('1'),
            ),
            ManagedRepo::new(
                Repository {
                    root: PathBuf::from("/repo-two"),
                },
                test_config(),
                Some('2'),
            ),
        ];
        let sessions = vec![
            test_session(0, "/repo-one", "main"),
            test_session(0, "/repo-one", "feature-one"),
            test_session(1, "/repo-two", "main"),
            test_session(1, "/repo-two", "feature-two"),
        ];
        Tui::new(repos, 0, sessions)
    }

    fn test_session(repo_index: usize, root: &str, branch: &str) -> Session {
        Session {
            repo_index,
            repo_label: format!("repo-{repo_index}"),
            repo_key: None,
            path: PathBuf::from(format!("{root}/{branch}")),
            path_display: format!("{root}/{branch}"),
            branch: branch.to_string(),
            prompt_summary: String::new(),
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
            default_agent: "opencode".to_string(),
            default_base: Some("main".to_string()),
            plan_dir: "plans".to_string(),
            review_packet_dir: ".agent/review".to_string(),
            worktree_command: "wt".to_string(),
            opencode_port_base: 41_000,
            opencode_port_span: 1_000,
            opencode_shutdown_owned_servers: false,
            escape_key: EscapeKey::EscEsc,
            merge_method: MergeMethod::Squash,
            checks: Checks::default(),
            worktree_columns: Vec::new(),
            tools: BTreeMap::new(),
            agent_commands: BTreeMap::new(),
            agent_prompt_modes: BTreeMap::new(),
            prompt_templates: BTreeMap::new(),
            user_path: PathBuf::from("/tmp/prism-user.toml"),
            repo_config_path: PathBuf::from("/tmp/prism-repo.toml"),
        }
    }
}
