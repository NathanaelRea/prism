use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};

use crate::agent::AgentState;
use crate::agent_session::{AgentSessionSlot, AgentSessionWarmupKey, AgentSessionWarmupResult};
use crate::auto_flow::{
    AutoRunStatus, PersistedAutoRun, load_auto_run, load_output_lines as load_auto_output_lines,
    load_recent_active_runs_for_repo, reconcile_stale_auto_run,
};
use crate::config::Config;
use crate::github::{PrCache, PrSummary};
use crate::input::{Key, KeyInput};
use crate::opencode::{OpencodeEvent, OpencodeStatus};
use crate::plan_run::{
    DEFAULT_OUTPUT_LINES_PER_STEP, PersistedPlanRun, PlanRunStatus, PlanStepStatus,
    cleanup_stale_archived_plan_runs, load_output_lines, load_plan_run,
    load_recent_plan_runs_for_repo, reconcile_stale_plan_run,
};
use crate::repo::Repository;
use crate::session::{Session, append_runtime_log};
use crate::terminal::stdin_is_tty;
use crate::tmux::TmuxWindow;
use crate::tui_runtime::{RuntimeEvent, TerminalRuntime};
use crate::util::status_count;
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
    pub(crate) main_focused: bool,
    pub(crate) main_scroll: usize,
    pub(crate) repo_main_view: view::RepoMainView,
    pub(crate) worktree_main_view: view::WorktreeMainView,
    pub(crate) worktree_list_mode: WorktreeListMode,
    ui_state_path: Option<PathBuf>,
    pub(crate) selected_comment: usize,
    pub(crate) selected_worktree_by_repo: BTreeMap<PathBuf, PathBuf>,
    pub(crate) pr_poll_tx: Sender<PrPollResult>,
    pub(crate) pr_poll_rx: Receiver<PrPollResult>,
    pub(crate) pr_polls_in_flight: BTreeSet<PrPollKey>,
    pub(crate) delete_session_tx: Sender<DeleteSessionResult>,
    pub(crate) delete_session_rx: Receiver<DeleteSessionResult>,
    pub(crate) delete_sessions_in_flight: BTreeSet<DeleteSessionKey>,
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
    pub(crate) plan_run_tx: Sender<PlanRunResult>,
    pub(crate) plan_run_rx: Receiver<PlanRunResult>,
    pub(crate) plan_runs: BTreeMap<String, PersistedPlanRun>,
    pub(crate) active_plan_runs: BTreeMap<PathBuf, String>,
    pub(crate) selected_plan_step_by_run: BTreeMap<String, usize>,
    pub(crate) manual_plan_step_selection_by_run: BTreeSet<String>,
    pub(crate) plan_output_state_by_run: BTreeMap<String, view::PlanOutputViewerState>,
    pub(crate) auto_runs: BTreeMap<String, PersistedAutoRun>,
    pub(crate) active_auto_runs: BTreeMap<PathBuf, String>,
    pub(crate) selected_auto_run: Option<String>,
    pub(crate) selected_auto_step_by_run: BTreeMap<String, i64>,
    pub(crate) auto_output_state_by_run: BTreeMap<String, view::AutoOutputViewerState>,
    pub(crate) last_plan_poll: Option<Instant>,
    pub(crate) repo_filter: String,
    pub(crate) worktree_filter: String,
    pub(crate) leader_hint: Option<LeaderHint>,
    pub(crate) dialog: Option<view::DialogModel>,
    status_message: Option<String>,
    status_message_until: Option<Instant>,
    #[cfg(test)]
    pub(crate) prompt_submissions: Option<Vec<(usize, String, u64)>>,
}

const STATUS_MESSAGE_DURATION: Duration = Duration::from_secs(5);
const ARCHIVED_PLAN_RETENTION: Duration = Duration::from_secs(60 * 60 * 24 * 30);
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

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct DeleteSessionKey {
    pub repo_root: PathBuf,
    pub path: PathBuf,
}

pub(crate) struct DeleteSessionResult {
    pub key: DeleteSessionKey,
    pub result: Result<(), String>,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorktreeListMode {
    Repo,
    Global,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OpenTmuxSessionTarget {
    PlanPhaseAgent,
    WorktreeAgent,
    RepoDefaultAgent(usize),
    Blocked(&'static str),
}

#[derive(Clone)]
pub(crate) struct NavigationSnapshot {
    focused_panel: PanelFocus,
    main_focused: bool,
    main_scroll: usize,
    current_repo_root: Option<PathBuf>,
    selected_worktree_path: Option<PathBuf>,
    selected_comment: usize,
    worktree_list_mode: WorktreeListMode,
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

pub(crate) struct PlanRunResult {
    pub repo_root: PathBuf,
    pub run_id: String,
    pub result: Result<(), String>,
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

fn preferred_plan_step(run: &PersistedPlanRun) -> usize {
    run.steps
        .iter()
        .filter(|step| {
            matches!(
                step.status,
                PlanStepStatus::Starting | PlanStepStatus::Running
            )
        })
        .max_by_key(|step| (step.started_unix_ms.unwrap_or(0), step.step))
        .or_else(|| {
            run.steps
                .iter()
                .filter(|step| {
                    !matches!(step.status, PlanStepStatus::Done | PlanStepStatus::Skipped)
                })
                .filter(|step| step.started_unix_ms.is_some() || step.finished_unix_ms.is_some())
                .max_by_key(|step| {
                    (
                        step.started_unix_ms.or(step.finished_unix_ms).unwrap_or(0),
                        step.step,
                    )
                })
        })
        .or_else(|| {
            run.steps
                .iter()
                .filter(|step| {
                    matches!(
                        step.status,
                        PlanStepStatus::Done
                            | PlanStepStatus::Failed
                            | PlanStepStatus::Aborted
                            | PlanStepStatus::Skipped
                    )
                })
                .max_by_key(|step| (step.finished_unix_ms.unwrap_or(0), step.step))
        })
        .or_else(|| {
            run.steps
                .iter()
                .find(|step| step.step == run.run.selected_step)
        })
        .or_else(|| run.steps.iter().max_by_key(|step| step.step))
        .map(|step| step.step)
        .unwrap_or(run.run.selected_step)
}

fn plan_run_status_sort_key(status: PlanRunStatus) -> u8 {
    match status {
        PlanRunStatus::Running => 0,
        PlanRunStatus::Queued => 1,
        PlanRunStatus::Paused => 2,
        PlanRunStatus::Failed => 3,
        PlanRunStatus::Aborted => 4,
        PlanRunStatus::Draft => 5,
        PlanRunStatus::Done => 6,
    }
}

#[derive(Default)]
struct TuiBackgroundChanges {
    tmux: bool,
    worktree_columns: bool,
    default_branch: bool,
    opencode_status: bool,
    opencode_events: bool,
    plan_runs: bool,
    auto_runs: bool,
    pull_requests: bool,
    delete_sessions: bool,
    status_message: bool,
}

impl TuiBackgroundChanges {
    fn any(&self) -> bool {
        self.tmux
            || self.worktree_columns
            || self.default_branch
            || self.opencode_status
            || self.opencode_events
            || self.plan_runs
            || self.auto_runs
            || self.pull_requests
            || self.delete_sessions
            || self.status_message
    }
}

fn plain_key(event: KeyEvent) -> bool {
    event
        .modifiers
        .intersection(KeyModifiers::CONTROL | KeyModifiers::ALT)
        .is_empty()
}

fn ctrl_key(event: KeyEvent) -> bool {
    event.modifiers.contains(KeyModifiers::CONTROL)
}

impl Tui {
    pub fn new(repos: Vec<ManagedRepo>, current_repo: usize, sessions: Vec<Session>) -> Self {
        let (pr_poll_tx, pr_poll_rx) = mpsc::channel();
        let (delete_session_tx, delete_session_rx) = mpsc::channel();
        let (tmux_warmup_tx, tmux_warmup_rx) = mpsc::channel();
        let (wt_poll_tx, wt_poll_rx) = mpsc::channel();
        let (default_branch_poll_tx, default_branch_poll_rx) = mpsc::channel();
        let (opencode_poll_tx, opencode_poll_rx) = mpsc::channel();
        let (opencode_event_tx, opencode_event_rx) = mpsc::channel();
        let (plan_run_tx, plan_run_rx) = mpsc::channel();
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
            main_focused: false,
            main_scroll: 0,
            repo_main_view: view::RepoMainView::Github,
            worktree_main_view: view::WorktreeMainView::Details,
            worktree_list_mode: WorktreeListMode::Repo,
            ui_state_path: None,
            selected_comment: 0,
            selected_worktree_by_repo: BTreeMap::new(),
            pr_poll_tx,
            pr_poll_rx,
            pr_polls_in_flight: BTreeSet::new(),
            delete_session_tx,
            delete_session_rx,
            delete_sessions_in_flight: BTreeSet::new(),
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
            plan_run_tx,
            plan_run_rx,
            plan_runs: BTreeMap::new(),
            active_plan_runs: BTreeMap::new(),
            selected_plan_step_by_run: BTreeMap::new(),
            manual_plan_step_selection_by_run: BTreeSet::new(),
            plan_output_state_by_run: BTreeMap::new(),
            auto_runs: BTreeMap::new(),
            active_auto_runs: BTreeMap::new(),
            selected_auto_run: None,
            selected_auto_step_by_run: BTreeMap::new(),
            auto_output_state_by_run: BTreeMap::new(),
            last_plan_poll: None,
            repo_filter: String::new(),
            worktree_filter: String::new(),
            leader_hint: None,
            dialog: None,
            status_message: None,
            status_message_until: None,
            #[cfg(test)]
            prompt_submissions: None,
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

    pub(crate) fn use_persisted_ui_state(&mut self, path: PathBuf) {
        if let Some(mode) = crate::ui_state::load_from_path(&path) {
            self.worktree_list_mode = mode;
            self.restore_selected_worktree_for_repo();
        }
        self.ui_state_path = Some(path);
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

        let mut runtime = TerminalRuntime::enter()?;
        self.start_tmux_agent_warmup();
        self.start_wt_column_poll();
        self.start_default_branch_status_poll(true);
        self.start_opencode_status_poll(true);
        self.start_opencode_event_listeners();
        self.refresh_plan_runs();
        self.refresh_auto_runs(true);
        self.draw(&mut runtime)?;
        if self.repos.is_empty() {
            match self.add_repository(&mut runtime) {
                Ok(()) => {}
                Err(error) => self.show_error("add repository failed", &error)?,
            }
        }
        let mut key_input = KeyInput::default();
        let mut pending_g = false;
        loop {
            if self.tick_tui_action_jobs().any() {
                self.draw(&mut runtime)?;
            }
            let event = runtime.poll_event(Duration::from_millis(100))?;
            let Some(event) = event else {
                continue;
            };
            let key = match event {
                RuntimeEvent::Key(event) => key_input.map_event(event),
                RuntimeEvent::Mouse(event) => {
                    if matches!(event.kind, MouseEventKind::Down(MouseButton::Left)) {
                        let area = runtime.area()?;
                        self.handle_mouse_click(event.column, event.row, area);
                        self.draw(&mut runtime)?;
                    }
                    continue;
                }
                RuntimeEvent::Resize => {
                    self.draw(&mut runtime)?;
                    continue;
                }
                RuntimeEvent::FocusGained => {
                    self.start_default_branch_status_poll(true);
                    self.poll_pull_requests(true);
                    self.draw(&mut runtime)?;
                    continue;
                }
            };
            let Some(key) = key else {
                continue;
            };

            let mut should_quit = false;
            match key {
                Key::Quit => {
                    self.clear_leader_hint();
                    pending_g = false;
                    should_quit = self.confirm_quit(&mut runtime)?;
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
                Key::FocusPrevious => {
                    self.clear_leader_hint();
                    self.focus_previous_panel();
                    pending_g = false;
                }
                Key::FocusMain => {
                    self.clear_leader_hint();
                    self.focus_main();
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
                Key::PreviousBlock => {
                    self.clear_leader_hint();
                    pending_g = false;
                }
                Key::NextBlock => {
                    self.clear_leader_hint();
                    pending_g = false;
                }
                Key::PreviousView => {
                    self.clear_leader_hint();
                    self.switch_worktree_list_mode(WorktreeListMode::Global);
                    pending_g = false;
                }
                Key::NextView => {
                    self.clear_leader_hint();
                    self.switch_worktree_list_mode(WorktreeListMode::Repo);
                    pending_g = false;
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
                    if self.open_selected_comment_dialog(&mut runtime)? {
                        self.draw(&mut runtime)?;
                        continue;
                    }
                    match self.open_tmux_session_target() {
                        OpenTmuxSessionTarget::RepoDefaultAgent(index) => {
                            self.enter_agent_mode_for_index(&mut runtime, index)?
                        }
                        OpenTmuxSessionTarget::PlanPhaseAgent => {
                            if let Err(error) = self.open_current_plan_tmux_session(&mut runtime) {
                                self.show_error("plan phase tmux failed", &error)?;
                            }
                        }
                        OpenTmuxSessionTarget::WorktreeAgent => {
                            self.enter_agent_mode(&mut runtime)?
                        }
                        OpenTmuxSessionTarget::Blocked(message) => self.show_message(message)?,
                    }
                }
                Key::LazyGit => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if self.focused_panel == PanelFocus::Status {
                        self.show_message("focus repos or worktrees to open lazygit")?;
                    } else if self.focused_panel == PanelFocus::Repos {
                        if let Err(error) = self.open_selected_repo_lazygit(&mut runtime) {
                            self.show_error("repository lazygit failed", &error)?;
                        }
                    } else if let Err(error) =
                        self.open_tmux_window(&mut runtime, TmuxWindow::LazyGit)
                    {
                        self.show_error("lazygit failed", &error)?;
                    }
                }
                Key::AutoFlow => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if self.focused_panel == PanelFocus::Status {
                    } else if self.focused_panel != PanelFocus::Worktrees {
                        self.show_message("focus worktrees to start or focus Auto Flow")?;
                    } else if let Err(error) = self.start_or_focus_selected_auto_run(&mut runtime) {
                        self.show_error("auto flow failed", &error)?;
                    }
                }
                Key::OpenPr => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if self.focused_panel != PanelFocus::Worktrees {
                        self.show_message("focus worktrees to open a PR")?;
                    } else if let Err(error) = self.open_selected_pr(&mut runtime) {
                        self.show_error("open PR failed", &error)?;
                    }
                }
                Key::Terminal => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if self.focused_panel == PanelFocus::Status {
                        self.show_message("focus repos or worktrees to open a terminal")?;
                    } else if self.focused_panel == PanelFocus::Repos {
                        if let Err(error) = self.open_selected_repo_terminal(&mut runtime) {
                            self.show_error("repository terminal failed", &error)?;
                        }
                    } else if let Err(error) =
                        self.open_tmux_window(&mut runtime, TmuxWindow::Terminal)
                    {
                        self.show_error("terminal failed", &error)?;
                    }
                }
                Key::PlanActions => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if let Err(error) = self.show_plan_actions_dialog(&mut runtime) {
                        self.show_error("plan actions failed", &error)?;
                    }
                }
                Key::Help => {
                    self.clear_leader_hint();
                    pending_g = false;
                    self.show_keybindings_dialog(&mut runtime)?;
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
                    self.refresh_plan_runs();
                    self.refresh_auto_runs(false);
                    self.poll_pull_requests(true);
                }
                Key::VisibilityUp => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if self.focused_panel != PanelFocus::Worktrees {
                        self.show_message("focus worktrees to change visibility")?;
                    } else if let Err(error) = self.adjust_selected_visibility(1) {
                        self.show_error("visibility update failed", &error)?;
                    }
                }
                Key::VisibilityDown => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if self.focused_panel != PanelFocus::Worktrees {
                        self.show_message("focus worktrees to change visibility")?;
                    } else if let Err(error) = self.adjust_selected_visibility(-1) {
                        self.show_error("visibility update failed", &error)?;
                    }
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
                        self.show_message("focus worktrees to send a review-fix prompt")?;
                    } else if let Err(error) = self.start_review_fix(&mut runtime) {
                        self.show_error("review fix failed", &error)?;
                    }
                }
                Key::CiFix => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if self.focused_panel != PanelFocus::Worktrees {
                        self.show_message("focus worktrees to send a CI-failure prompt")?;
                    } else if let Err(error) = self.start_ci_fix(&mut runtime) {
                        self.show_error("CI failure prompt failed", &error)?;
                    }
                }
                Key::Push => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if self.focused_panel != PanelFocus::Worktrees {
                        self.show_message("focus worktrees to push a branch")?;
                    } else if let Err(error) = self.push_selected_branch(&mut runtime) {
                        self.show_error("push failed", &error)?;
                    }
                }
                Key::Merge => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if self.focused_panel != PanelFocus::Worktrees {
                        self.show_message("focus worktrees to merge a PR")?;
                    } else if let Err(error) = self.merge_selected_pr(&mut runtime) {
                        self.show_error("merge failed", &error)?;
                    }
                }
                Key::PullDefault => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if self.focused_panel != PanelFocus::Repos {
                        self.show_message("focus repos to pull the default branch")?;
                    } else if let Err(error) = self.pull_default_branch(&mut runtime) {
                        self.show_error("pull failed", &error)?;
                    }
                }
                Key::PlanMode => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if self.focused_panel == PanelFocus::Status {
                    } else if self.focused_panel != PanelFocus::Worktrees {
                        self.show_message("focus worktrees to run plan mode")?;
                    } else if let Err(error) = self.start_selected_worktree_plan_run(&mut runtime) {
                        self.show_error("plan mode failed", &error)?;
                    }
                }
                Key::Create => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if self.focused_panel != PanelFocus::Repos {
                        self.show_message("focus repos to create a worktree session")?;
                    } else {
                        match self.create_session(&mut runtime) {
                            Ok(true) => self.focus_worktrees(),
                            Ok(false) => {}
                            Err(error) => self.show_error("create session failed", &error)?,
                        }
                    }
                }
                Key::AbortOpencode => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if self.focused_panel != PanelFocus::Worktrees {
                        self.show_message("focus worktrees to abort an OpenCode session")?;
                    } else if let Err(error) = self.abort_selected_opencode_session(&mut runtime) {
                        self.show_error("abort failed", &error)?;
                    }
                }
                Key::ManageRepos => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if let Err(error) = self.edit_repositories(&mut runtime) {
                        self.show_error("edit repositories failed", &error)?;
                    }
                }
                Key::OpenRemotePrs => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if self.focused_panel != PanelFocus::Repos {
                        self.show_message("focus repos to open a remote PR worktree")?;
                    } else if let Err(error) = self.open_remote_pr_worktree(&mut runtime) {
                        self.show_error("open remote PR worktree failed", &error)?;
                    }
                }
                Key::Delete => {
                    self.clear_leader_hint();
                    pending_g = false;
                    let handled =
                        self.dismiss_selected_auto_run()? || self.dismiss_selected_plan_run()?;
                    if handled {
                    } else if self.focused_panel == PanelFocus::Status {
                        self.show_message("focus worktrees to delete a worktree/session")?;
                    } else if self.focused_panel == PanelFocus::Repos {
                        self.show_message("repository removal is available from R")?;
                    } else if let Err(error) = self.archive_session(&mut runtime) {
                        self.show_error("archive failed", &error)?;
                    }
                }
                Key::Unarchive => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if self.focused_panel != PanelFocus::Repos {
                        self.show_message("focus repos to unarchive a worktree")?;
                    } else if let Err(error) = self.unarchive_session(&mut runtime) {
                        self.show_error("unarchive failed", &error)?;
                    }
                }
                Key::DeletePermanent => {
                    self.clear_leader_hint();
                    pending_g = false;
                    let handled =
                        self.dismiss_selected_auto_run()? || self.dismiss_selected_plan_run()?;
                    if handled {
                    } else if self.focused_panel != PanelFocus::Worktrees {
                        self.show_message(
                            "focus worktrees to permanently delete a worktree/session",
                        )?;
                    } else if let Err(error) = self.delete_session(&mut runtime) {
                        self.show_error("delete failed", &error)?;
                    }
                }
                Key::EditWorktreeColumns => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if let Err(error) = self.edit_worktree_columns(&mut runtime) {
                        self.show_error("edit worktree columns failed", &error)?;
                    }
                }
                Key::EditConfig => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if let Err(error) = self.edit_config(&mut runtime) {
                        self.show_error("edit config failed", &error)?;
                    }
                }
                Key::EditUserConfig => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if let Err(error) = self.edit_user_config(&mut runtime) {
                        self.show_error("edit user config failed", &error)?;
                    }
                }
                Key::Search => {
                    self.clear_leader_hint();
                    pending_g = false;
                    self.search_sessions(&mut runtime)?;
                }
                Key::Other => {
                    self.clear_leader_hint();
                    pending_g = false;
                }
            }
            if should_quit {
                break;
            }
            self.draw(&mut runtime)?;
        }
        self.shutdown_owned_opencode_servers();
        Ok(())
    }

    fn tick_tui_action_jobs(&mut self) -> TuiBackgroundChanges {
        let changes = TuiBackgroundChanges {
            tmux: self.poll_tmux_agent_warmup(),
            worktree_columns: self.poll_wt_columns(),
            default_branch: self.poll_default_branch_status(),
            opencode_status: self.poll_opencode_status(),
            opencode_events: self.poll_opencode_events(),
            plan_runs: self.poll_plan_runs(),
            auto_runs: self.poll_auto_runs(),
            pull_requests: self.poll_pull_requests(false),
            delete_sessions: self.poll_delete_sessions(),
            status_message: self.expire_status_message(),
        };
        self.start_default_branch_status_poll(false);
        self.start_opencode_status_poll(false);
        self.start_opencode_event_listeners();
        changes
    }

    fn confirm_quit(&mut self, runtime: &mut TerminalRuntime) -> Result<bool, String> {
        if !self.delete_sessions_in_flight.is_empty() {
            self.show_message("delete in progress; wait for it to finish before quitting")?;
            return Ok(false);
        }
        if !self
            .sessions
            .iter()
            .any(|session| session.agent_state == AgentState::Running)
        {
            return Ok(true);
        }
        self.confirm_action_dialog(
            runtime,
            "Quit Prism",
            "Agents are running. Quit Prism?",
            "Quit",
        )
    }

    fn enter_agent_mode(&mut self, runtime: &mut TerminalRuntime) -> Result<(), String> {
        if self.selected_worktree_context().is_none() {
            return Ok(());
        }
        let Some(index) = self.selected_worktree_index() else {
            return Ok(());
        };
        self.enter_agent_mode_for_index(runtime, index)
    }

    fn enter_agent_mode_for_index(
        &mut self,
        runtime: &mut TerminalRuntime,
        index: usize,
    ) -> Result<(), String> {
        let navigation = self.navigation_snapshot();
        runtime.suspend()?;
        let result = self.attach_tmux_session_for_index(index);
        let resume_result = runtime.resume();
        self.refresh_sessions()?;
        self.restore_navigation_snapshot(navigation);
        self.start_tmux_agent_warmup();
        resume_result?;
        if let Err(error) = result {
            self.show_error("tmux session failed", &error)?;
        }
        Ok(())
    }

    fn open_tmux_window(
        &mut self,
        runtime: &mut TerminalRuntime,
        window: TmuxWindow,
    ) -> Result<(), String> {
        if self.selected >= self.sessions.len() {
            return Ok(());
        }
        let navigation = self.navigation_snapshot();
        runtime.suspend()?;
        let result = self.attach_selected_tmux_window(window);
        let resume_result = runtime.resume();
        self.refresh_sessions()?;
        self.restore_navigation_snapshot(navigation);
        self.start_tmux_agent_warmup();
        resume_result?;
        result
    }

    fn show_keybindings_dialog(&mut self, runtime: &mut TerminalRuntime) -> Result<(), String> {
        let items = [
            "1 / 2 / 3    focus status / repos / worktrees sidebars; 3 toggles repo/all worktrees",
            "0            focus main panel for the selected sidebar",
            "Tab / Shift-Tab  move focus between panels",
            "h/l, left/right arrows  repos: switch view; status plan: switch phase",
            "Enter       repos: open default-branch tmux; worktrees: open agent or selected plan phase; main comments: details",
            "Ctrl-/       open tmux window 3: terminal",
            "p            repos: pull default branch",
            "P            worktrees: start or focus a plan run dashboard",
            "j/k          main comments: move comment selection; status dashboard: move plan output or phase selection",
            "A            worktrees: start/focus Auto Flow; choose prompt, plan file, or draft plan",
            "R            edit repositories/order/keys/remove",
            "C            repos: open a worktree for a remote pull request",
            "c            repos: create worktree session in selected repo",
            "+ / -        worktrees: raise/lower visibility sort",
            "x            worktrees: abort selected OpenCode session",
            "e            edit selected repository config, then reload",
            "E            edit user config, then reload",
            "W            repos: edit visible worktree columns in repo config",
            "/            search/filter focused panel",
            "?            show keybindings; / filters this dialog",
            "D            archive non-default worktree/session",
            "U            repos: choose an archived worktree to unarchive",
            "X            permanently delete non-default worktree/session",
            "j/k, up/down move selection",
            "g g / G      top / bottom",
            "r            refresh",
            "q, Ctrl-C    quit",
        ];
        let items = items
            .iter()
            .map(|item| (*item).to_string())
            .collect::<Vec<_>>();
        let mut filter = String::new();
        let mut editing_filter = false;
        let mut scroll = 0usize;
        let info_lines = view::keybinding_info_lines(self.focused_panel, self.config.icon_style);
        self.dialog = Some(view::DialogModel::Help {
            filter: filter.clone(),
            editing_filter,
            info_lines: info_lines.clone(),
            items: items.clone(),
            scroll,
        });
        self.draw(runtime)?;
        loop {
            if self.tick_tui_action_jobs().any() {
                self.draw(runtime)?;
            }
            let Some(event) = runtime.poll_event(Duration::from_millis(100))? else {
                continue;
            };
            let RuntimeEvent::Key(event) = event else {
                self.draw(runtime)?;
                continue;
            };
            if event.kind != KeyEventKind::Press {
                continue;
            }
            let mut close = false;
            match event.code {
                KeyCode::Char('/') if plain_key(event) && !editing_filter => {
                    editing_filter = true;
                    filter.clear();
                    scroll = 0;
                }
                KeyCode::Enter if editing_filter => editing_filter = false,
                KeyCode::Backspace if editing_filter => {
                    filter.pop();
                    scroll = 0;
                }
                KeyCode::Up | KeyCode::Char('k') if !editing_filter => {
                    scroll = scroll.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') if !editing_filter => {
                    scroll = scroll.saturating_add(1);
                }
                KeyCode::Esc => close = true,
                KeyCode::Char('c') if ctrl_key(event) => close = true,
                KeyCode::Char('q') if plain_key(event) => close = true,
                KeyCode::Char(ch) if editing_filter && plain_key(event) && !ch.is_control() => {
                    filter.push(ch);
                    scroll = 0;
                }
                _ if !editing_filter => close = true,
                _ => {}
            }
            if close {
                self.dialog = None;
                self.draw(runtime)?;
                return Ok(());
            }
            self.dialog = Some(view::DialogModel::Help {
                filter: filter.clone(),
                editing_filter,
                info_lines: info_lines.clone(),
                items: items.clone(),
                scroll,
            });
            self.draw(runtime)?;
        }
    }

    pub(crate) fn confirm_archive_dialog(
        &mut self,
        runtime: &mut TerminalRuntime,
        branch: &str,
        path: &str,
        warnings: &[String],
    ) -> Result<bool, String> {
        let mut lines = vec![
            view::DialogLine {
                text: format!("branch: {branch}"),
                attention: false,
            },
            view::DialogLine {
                text: format!("path: {path}"),
                attention: false,
            },
        ];
        if warnings.is_empty() {
            lines.push(view::DialogLine {
                text: "No warnings detected; worktree files stay on disk.".to_string(),
                attention: false,
            });
        } else {
            for warning in warnings {
                lines.push(view::DialogLine {
                    text: warning.clone(),
                    attention: true,
                });
            }
        }
        lines.push(view::DialogLine {
            text: "Archive hides this worktree from normal navigation. Restore with `git worktree list` and remove the archive marker from Prism state if needed.".to_string(),
            attention: false,
        });
        self.confirm_dialog(runtime, "Archive Session", lines, "Archive", "Cancel")
    }

    pub(crate) fn confirm_delete_dialog(
        &mut self,
        runtime: &mut TerminalRuntime,
        branch: &str,
        path: &str,
        warnings: &[String],
    ) -> Result<bool, String> {
        let mut lines = vec![
            view::DialogLine {
                text: format!("branch: {branch}"),
                attention: false,
            },
            view::DialogLine {
                text: format!("path: {path}"),
                attention: false,
            },
        ];
        if warnings.is_empty() {
            lines.push(view::DialogLine {
                text: "No warnings detected.".to_string(),
                attention: false,
            });
        } else {
            for warning in warnings {
                lines.push(view::DialogLine {
                    text: warning.clone(),
                    attention: true,
                });
            }
        }
        self.confirm_dialog(runtime, "Delete Session", lines, "Delete", "Cancel")
    }

    pub(crate) fn prompt_line_dialog(
        &mut self,
        runtime: &mut TerminalRuntime,
        title: &str,
        prompt: &str,
        initial: &str,
    ) -> Result<Option<String>, String> {
        let mut input = initial.to_string();
        self.dialog = Some(view::DialogModel::Prompt {
            title: title.to_string(),
            prompt: prompt.to_string(),
            input: input.clone(),
        });
        self.draw(runtime)?;
        loop {
            if self.tick_tui_action_jobs().any() {
                self.draw(runtime)?;
            }
            let Some(event) = runtime.poll_event(Duration::from_millis(100))? else {
                continue;
            };
            let RuntimeEvent::Key(event) = event else {
                self.draw(runtime)?;
                continue;
            };
            if event.kind != KeyEventKind::Press {
                continue;
            }
            match event.code {
                KeyCode::Enter => {
                    self.dialog = None;
                    self.draw(runtime)?;
                    return Ok(Some(input));
                }
                KeyCode::Esc | KeyCode::Char('c')
                    if event.code == KeyCode::Esc || ctrl_key(event) =>
                {
                    self.dialog = None;
                    self.draw(runtime)?;
                    return Ok(None);
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(ch) if plain_key(event) && !ch.is_control() => {
                    input.push(ch);
                }
                _ => {}
            }
            self.dialog = Some(view::DialogModel::Prompt {
                title: title.to_string(),
                prompt: prompt.to_string(),
                input: input.clone(),
            });
            self.draw(runtime)?;
        }
    }

    pub(crate) fn prompt_choice_dialog(
        &mut self,
        runtime: &mut TerminalRuntime,
        choices: view::ChoiceList,
    ) -> Result<Option<String>, String> {
        self.dialog = Some(view::DialogModel::Choice {
            choices: choices.clone(),
        });
        self.draw(runtime)?;
        loop {
            if self.tick_tui_action_jobs().any() {
                self.draw(runtime)?;
            }
            let Some(event) = runtime.poll_event(Duration::from_millis(100))? else {
                continue;
            };
            let RuntimeEvent::Key(event) = event else {
                self.draw(runtime)?;
                continue;
            };
            if event.kind != KeyEventKind::Press {
                continue;
            }
            match event.code {
                KeyCode::Esc | KeyCode::Char('c')
                    if event.code == KeyCode::Esc || ctrl_key(event) =>
                {
                    self.dialog = None;
                    self.draw(runtime)?;
                    return Ok(None);
                }
                KeyCode::Char(ch) if plain_key(event) && !ch.is_control() => {
                    let normalized = ch.to_string().to_ascii_lowercase();
                    if choices
                        .choices
                        .iter()
                        .any(|option| option.key.eq_ignore_ascii_case(&normalized))
                    {
                        self.dialog = None;
                        self.draw(runtime)?;
                        return Ok(Some(normalized));
                    }
                }
                _ => {}
            }
            self.dialog = Some(view::DialogModel::Choice {
                choices: choices.clone(),
            });
            self.draw(runtime)?;
        }
    }

    pub(crate) fn show_loading_dialog(
        &mut self,
        runtime: &mut TerminalRuntime,
        title: &str,
        message: &str,
    ) -> Result<(), String> {
        self.dialog = Some(view::DialogModel::Progress {
            title: title.to_string(),
            message: message.to_string(),
        });
        self.draw(runtime)?;
        self.dialog = None;
        Ok(())
    }

    pub(crate) fn confirm_dialog(
        &mut self,
        runtime: &mut TerminalRuntime,
        title: &str,
        lines: Vec<view::DialogLine>,
        confirm_label: &str,
        cancel_label: &str,
    ) -> Result<bool, String> {
        self.dialog = Some(view::DialogModel::Confirm {
            title: title.to_string(),
            lines: lines.clone(),
            confirm_label: confirm_label.to_string(),
            cancel_label: cancel_label.to_string(),
        });
        self.draw(runtime)?;
        loop {
            if self.tick_tui_action_jobs().any() {
                self.draw(runtime)?;
            }
            let Some(event) = runtime.poll_event(Duration::from_millis(100))? else {
                continue;
            };
            let RuntimeEvent::Key(event) = event else {
                self.draw(runtime)?;
                continue;
            };
            if event.kind != KeyEventKind::Press {
                continue;
            }
            match event.code {
                KeyCode::Enter | KeyCode::Char('y' | 'Y') if plain_key(event) => {
                    self.dialog = None;
                    self.draw(runtime)?;
                    return Ok(true);
                }
                KeyCode::Esc | KeyCode::Char('n' | 'N')
                    if event.code == KeyCode::Esc || plain_key(event) =>
                {
                    self.dialog = None;
                    self.draw(runtime)?;
                    return Ok(false);
                }
                KeyCode::Char('q') if plain_key(event) => {
                    self.dialog = None;
                    self.draw(runtime)?;
                    return Ok(false);
                }
                KeyCode::Char('c') if ctrl_key(event) => {
                    self.dialog = None;
                    self.draw(runtime)?;
                    return Ok(false);
                }
                _ => {}
            }
        }
    }

    pub(crate) fn confirm_action_dialog(
        &mut self,
        runtime: &mut TerminalRuntime,
        title: &str,
        message: &str,
        confirm_label: &str,
    ) -> Result<bool, String> {
        self.confirm_dialog(
            runtime,
            title,
            vec![view::DialogLine {
                text: message.to_string(),
                attention: false,
            }],
            confirm_label,
            "Cancel",
        )
    }

    pub(crate) fn show_message(&mut self, message: &str) -> Result<(), String> {
        self.status_message = Some(message.to_string());
        self.status_message_until = Some(Instant::now() + STATUS_MESSAGE_DURATION);
        let _ = append_runtime_log(&self.repo, message);
        Ok(())
    }

    fn show_error(&mut self, context: &str, error: &str) -> Result<(), String> {
        let message = format!("{context}: {error}");
        self.show_message(&message)
    }

    fn move_down(&mut self) {
        if self.main_focused {
            if self.move_comment_selection(1) {
                return;
            }
            self.main_scroll = self.main_scroll.saturating_add(1);
            self.move_plan_step_selection(1);
            return;
        }
        match self.focused_panel {
            PanelFocus::Status => {}
            PanelFocus::Repos => self.move_repo_selection(1),
            PanelFocus::Worktrees => self.move_worktree_selection(1),
        }
    }

    fn move_up(&mut self) {
        if self.main_focused {
            if self.move_comment_selection(-1) {
                return;
            }
            self.main_scroll = self.main_scroll.saturating_sub(1);
            self.move_plan_step_selection(-1);
            return;
        }
        match self.focused_panel {
            PanelFocus::Status => {}
            PanelFocus::Repos => self.move_repo_selection(-1),
            PanelFocus::Worktrees => self.move_worktree_selection(-1),
        }
    }

    fn move_left(&mut self) {
        if !self.main_focused {
            return;
        }
        match self.focused_panel {
            PanelFocus::Status => {
                self.move_plan_step_selection(-1);
            }
            PanelFocus::Repos => {
                self.repo_main_view = view::RepoMainView::Github;
            }
            PanelFocus::Worktrees => {}
        }
    }

    fn move_right(&mut self) {
        if !self.main_focused {
            return;
        }
        match self.focused_panel {
            PanelFocus::Status => {
                self.move_plan_step_selection(1);
            }
            PanelFocus::Repos => {
                self.repo_main_view = view::RepoMainView::Kanban;
            }
            PanelFocus::Worktrees => {}
        }
    }

    fn focus_next_panel(&mut self) {
        self.main_scroll = 0;
        self.focused_panel = match self.focused_panel {
            PanelFocus::Status => PanelFocus::Repos,
            PanelFocus::Repos => PanelFocus::Worktrees,
            PanelFocus::Worktrees => PanelFocus::Status,
        };
        self.main_focused = false;
    }

    fn focus_previous_panel(&mut self) {
        self.main_scroll = 0;
        self.focused_panel = match self.focused_panel {
            PanelFocus::Status => PanelFocus::Worktrees,
            PanelFocus::Repos => PanelFocus::Status,
            PanelFocus::Worktrees => PanelFocus::Repos,
        };
        self.main_focused = false;
    }

    pub(crate) fn focus_status(&mut self) {
        self.main_scroll = 0;
        self.focused_panel = PanelFocus::Status;
        self.main_focused = false;
    }

    fn focus_repos(&mut self) {
        self.main_scroll = 0;
        self.focused_panel = PanelFocus::Repos;
        self.main_focused = false;
    }

    pub(crate) fn focus_worktrees(&mut self) {
        self.main_scroll = 0;
        self.focused_panel = PanelFocus::Worktrees;
        self.main_focused = false;
        if self.worktree_list_mode == WorktreeListMode::Repo {
            self.restore_selected_worktree_for_repo();
        }
    }

    fn switch_worktree_list_mode(&mut self, mode: WorktreeListMode) {
        if self.focused_panel != PanelFocus::Worktrees || self.worktree_list_mode == mode {
            return;
        }
        self.worktree_list_mode = mode;
        self.persist_worktree_list_mode();
        if mode == WorktreeListMode::Repo {
            self.restore_selected_worktree_for_repo();
        }
    }

    fn persist_worktree_list_mode(&self) {
        let Some(path) = self.ui_state_path.as_deref() else {
            return;
        };
        if let Err(error) = crate::ui_state::save_to_path(path, self.worktree_list_mode) {
            let _ = append_runtime_log(&self.repo, &format!("UI state save failed: {error}"));
        }
    }

    fn focus_main(&mut self) {
        self.main_focused = true;
    }

    fn open_tmux_session_target(&self) -> OpenTmuxSessionTarget {
        match self.focused_panel {
            PanelFocus::Status => OpenTmuxSessionTarget::Blocked("status has no Enter action"),
            PanelFocus::Repos => {
                if let Some(index) = self.selected_repo_default_session_index() {
                    OpenTmuxSessionTarget::RepoDefaultAgent(index)
                } else {
                    OpenTmuxSessionTarget::Blocked("selected repository has no default worktree")
                }
            }
            PanelFocus::Worktrees => {
                if self.main_focused && self.current_plan_dashboard().is_some() {
                    return OpenTmuxSessionTarget::PlanPhaseAgent;
                }
                if self.selected_worktree_context().is_none() {
                    return OpenTmuxSessionTarget::Blocked(
                        "selected repository has no visible worktrees",
                    );
                }
                OpenTmuxSessionTarget::WorktreeAgent
            }
        }
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
        if self.main_focused {
            return;
        }
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
        if self.main_focused {
            return;
        }
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
        let mut indices = self
            .sessions
            .iter()
            .enumerate()
            .filter_map(|(index, session)| {
                (!session.hidden
                    && (self.worktree_list_mode == WorktreeListMode::Global
                        || session.repo_index == self.current_repo)
                    && !self
                        .repos
                        .get(session.repo_index)
                        .is_some_and(|repo| repo.config.is_default_branch(&session.branch))
                    && (filter.is_empty()
                        || session.branch.to_ascii_lowercase().contains(&filter)
                        || session.repo_label.to_ascii_lowercase().contains(&filter)
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
            .collect::<Vec<_>>();
        indices.sort_by_key(|index| self.worktree_sort_key(*index));
        indices
    }

    fn worktree_sort_key(&self, index: usize) -> (u8, String, String) {
        let Some(session) = self.sessions.get(index) else {
            return (1, String::new(), String::new());
        };
        (
            worktree_priority_rank(session.visibility),
            session.repo_label.clone(),
            worktree_sort_name(session),
        )
    }

    fn mark_selected_seen(&mut self) {
        if let Some(session) = self.sessions.get_mut(self.selected) {
            session.unseen_comments = false;
        }
    }

    pub(crate) fn select_worktree(&mut self, index: usize) {
        self.main_scroll = 0;
        let Some(session) = self.sessions.get(index) else {
            return;
        };
        let repo_index = session.repo_index;
        let path = session.path.clone();
        self.selected = index;
        self.selected_comment = 0;
        if let Some(repo) = self.repos.get(repo_index) {
            let repo_root = repo.repo.root.clone();
            self.current_repo = repo_index;
            self.selected_repo_root = Some(repo_root.clone());
            self.sync_selected_repo_context();
            self.selected_worktree_by_repo.insert(repo_root, path);
        }
        self.mark_selected_seen();
    }

    pub(crate) fn navigation_snapshot(&self) -> NavigationSnapshot {
        NavigationSnapshot {
            focused_panel: self.focused_panel,
            main_focused: self.main_focused,
            main_scroll: self.main_scroll,
            current_repo_root: self
                .repos
                .get(self.current_repo)
                .map(|repo| repo.repo.root.clone()),
            selected_worktree_path: self
                .selected_worktree_index()
                .and_then(|index| self.sessions.get(index))
                .map(|session| session.path.clone()),
            selected_comment: self.selected_comment,
            worktree_list_mode: self.worktree_list_mode,
        }
    }

    pub(crate) fn restore_navigation_snapshot(&mut self, snapshot: NavigationSnapshot) {
        self.worktree_list_mode = snapshot.worktree_list_mode;
        if let Some(root) = snapshot.current_repo_root.as_ref()
            && let Some(index) = self.repos.iter().position(|repo| repo.repo.root == *root)
        {
            self.current_repo = index;
            self.selected_repo_root = Some(root.clone());
            self.sync_selected_repo_context();
        }
        if let Some(path) = snapshot.selected_worktree_path.as_ref()
            && let Some(index) = self
                .sessions
                .iter()
                .position(|session| session.path == *path)
        {
            self.selected = index;
            if let Some(session) = self.sessions.get(index)
                && let Some(repo) = self.repos.get(session.repo_index)
            {
                self.selected_worktree_by_repo
                    .insert(repo.repo.root.clone(), session.path.clone());
            }
        } else if self.selected_worktree_index().is_none() {
            self.restore_selected_worktree_for_repo();
        }
        self.selected_comment = snapshot.selected_comment;
        self.focused_panel = snapshot.focused_panel;
        self.main_focused = snapshot.main_focused;
        self.main_scroll = snapshot.main_scroll;
    }

    fn selected_repo_default_session_index(&self) -> Option<usize> {
        let config = self.repos.get(self.current_repo).map(|repo| &repo.config)?;
        self.sessions
            .iter()
            .enumerate()
            .find_map(|(index, session)| {
                (session.repo_index == self.current_repo
                    && config.is_default_branch(&session.branch))
                .then_some(index)
            })
    }

    fn adjust_selected_visibility(&mut self, delta: i16) -> Result<(), String> {
        let Some(index) = self.selected_worktree_index() else {
            return Ok(());
        };
        let Some(session) = self.sessions.get(index) else {
            return Ok(());
        };
        let Some(managed) = self.repos.get(session.repo_index) else {
            return Ok(());
        };
        let visibility = session.visibility.saturating_add(delta).clamp(-9, 9);
        crate::session::set_worktree_visibility(&managed.repo, session, visibility)?;
        if let Some(session) = self.sessions.get_mut(index) {
            session.visibility = visibility;
        }
        Ok(())
    }

    fn selected_comment_rows(&self) -> Vec<view::PrCommentDisplayRow> {
        let Some(index) = self.selected_worktree_index() else {
            return Vec::new();
        };
        self.sessions
            .get(index)
            .and_then(|session| session.pr.details.as_ref())
            .map(view::pr_comment_rows)
            .unwrap_or_default()
    }

    fn move_comment_selection(&mut self, direction: isize) -> bool {
        if self.focused_panel != PanelFocus::Worktrees {
            return false;
        }
        let rows = self.selected_comment_rows();
        if rows.is_empty() {
            self.selected_comment = 0;
            return false;
        }
        let current = self.selected_comment.min(rows.len().saturating_sub(1));
        let next = current as isize + direction;
        self.selected_comment = if next < 0 {
            0
        } else {
            (next as usize).min(rows.len().saturating_sub(1))
        };
        true
    }

    fn open_selected_comment_dialog(
        &mut self,
        runtime: &mut TerminalRuntime,
    ) -> Result<bool, String> {
        if !self.main_focused || self.focused_panel != PanelFocus::Worktrees {
            return Ok(false);
        }
        let rows = self.selected_comment_rows();
        let Some(row) = rows.get(self.selected_comment) else {
            return Ok(false);
        };
        let mut lines = vec![
            view::DialogLine {
                text: format!("kind: {}", row.kind),
                attention: false,
            },
            view::DialogLine {
                text: format!("author: {}", row.author),
                attention: false,
            },
            view::DialogLine {
                text: format!("resolved: {}", row.resolved),
                attention: row.resolved.eq_ignore_ascii_case("no"),
            },
        ];
        if !row.context.trim().is_empty() {
            lines.push(view::DialogLine {
                text: format!("context: {}", row.context),
                attention: false,
            });
        }
        lines.push(view::DialogLine {
            text: String::new(),
            attention: false,
        });
        lines.push(view::DialogLine {
            text: row.body.clone(),
            attention: false,
        });
        self.confirm_dialog(runtime, "Comment Details", lines, "Close", "Close")?;
        Ok(true)
    }

    pub(crate) fn selected_worktree_index(&self) -> Option<usize> {
        self.visible_session_indices()
            .contains(&self.selected)
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
            self.selected_comment = 0;
            return;
        }
        self.selected = indices
            .iter()
            .copied()
            .find(|index| {
                self.sessions
                    .get(*index)
                    .is_some_and(|session| session.repo_index == self.current_repo)
            })
            .or_else(|| indices.first().copied())
            .unwrap_or(self.sessions.len());
        self.selected_comment = 0;
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
        self.main_scroll = 0;
        self.current_repo = repo_index.min(self.repos.len().saturating_sub(1));
        self.selected_repo_root = self
            .repos
            .get(self.current_repo)
            .map(|repo| repo.repo.root.clone());
        self.sync_selected_repo_context();
    }

    fn clear_leader_hint(&mut self) {
        self.leader_hint = None;
    }

    fn search_sessions(&mut self, runtime: &mut TerminalRuntime) -> Result<(), String> {
        match self.focused_panel {
            PanelFocus::Status => {
                self.show_message("status panel has no filter")?;
            }
            PanelFocus::Repos => {
                let initial = self.repo_filter.clone();
                let Some(input) = self.prompt_line_dialog(
                    runtime,
                    "Search Repositories",
                    "Filter (empty clears): ",
                    &initial,
                )?
                else {
                    return Ok(());
                };
                self.repo_filter = input;
                self.ensure_navigation_valid();
            }
            PanelFocus::Worktrees => {
                let initial = self.worktree_filter.clone();
                let Some(input) = self.prompt_line_dialog(
                    runtime,
                    "Search Worktrees",
                    "Filter (empty clears): ",
                    &initial,
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

    fn handle_mouse_click(&mut self, x: u16, y: u16, area: Rect) {
        let body_height = area.height.saturating_sub(1);
        if x >= view::sidebar_width_for(area.width, self.config.layout.sidebar_width)
            || y >= body_height
        {
            return;
        }
        let sidebar = Rect::new(
            0,
            0,
            view::sidebar_width_for(area.width, self.config.layout.sidebar_width),
            body_height,
        );
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(6),
                Constraint::Percentage(40),
                Constraint::Percentage(60),
            ])
            .split(sidebar);
        if point_in_rect(x, y, chunks[1]) {
            let row = y.saturating_sub(chunks[1].y).saturating_sub(1) as usize;
            if let Some(index) = self.visible_repo_indices().get(row).copied() {
                self.select_repo(index);
                self.focus_repos();
            }
            return;
        }
        if point_in_rect(x, y, chunks[2]) {
            let row = y.saturating_sub(chunks[2].y).saturating_sub(2) as usize;
            if let Some(index) = self.visible_session_indices().get(row).copied() {
                self.select_worktree(index);
                self.focus_worktrees();
            }
        }
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

    pub(crate) fn draw(&self, runtime: &mut TerminalRuntime) -> Result<(), String> {
        let model = self.frame_model();
        runtime.draw(&model)
    }

    fn poll_plan_runs(&mut self) -> bool {
        let mut changed = false;
        while let Ok(result) = self.plan_run_rx.try_recv() {
            changed = true;
            self.load_plan_run_snapshot(&result.repo_root, &result.run_id);
            match result.result {
                Ok(()) => {
                    self.status_message = Some("plan run completed".to_string());
                    self.status_message_until = Some(Instant::now() + STATUS_MESSAGE_DURATION);
                }
                Err(error) => {
                    self.status_message = Some(format!("plan run failed: {error}"));
                    self.status_message_until = Some(Instant::now() + STATUS_MESSAGE_DURATION);
                }
            }
        }
        let should_poll = self
            .last_plan_poll
            .is_none_or(|last| last.elapsed() >= Duration::from_secs(1));
        if should_poll {
            changed |= self.refresh_plan_runs();
            self.last_plan_poll = Some(Instant::now());
        }
        changed
    }

    fn refresh_plan_runs(&mut self) -> bool {
        let mut changed = false;
        let repos = self
            .repos
            .iter()
            .map(|managed| managed.repo.clone())
            .collect::<Vec<_>>();
        for repo in repos {
            let loaded = crate::observability::with_writable_db(&repo, |conn| {
                let _ = cleanup_stale_archived_plan_runs(
                    conn,
                    ARCHIVED_PLAN_RETENTION.as_millis() as u64,
                );
                let mut runs = load_recent_plan_runs_for_repo(conn, &repo.root, 8)?;
                for run in &mut runs {
                    let _ = reconcile_stale_plan_run(conn, run, DEFAULT_OUTPUT_LINES_PER_STEP);
                }
                Ok(runs)
            });
            let Ok(runs) = loaded else {
                continue;
            };
            for run in runs {
                changed |= self.remember_plan_run(run);
            }
        }
        changed
    }

    pub(crate) fn load_plan_run_snapshot(&mut self, repo_root: &Path, run_id: &str) {
        let repo = Repository {
            root: repo_root.to_path_buf(),
        };
        if let Ok(Some(run)) =
            crate::observability::with_writable_db(&repo, |conn| load_plan_run(conn, run_id))
        {
            self.remember_plan_run(run);
        }
    }

    pub(crate) fn remember_plan_run(&mut self, run: PersistedPlanRun) -> bool {
        let run_id = run.run.id.clone();
        let scope_path = run.run.scope_path.clone();
        let selected_step = self.resolved_plan_step_selection(&run);
        self.selected_plan_step_by_run
            .insert(run_id.clone(), selected_step);
        let selected_run_is_known = self
            .active_plan_runs
            .get(&scope_path)
            .is_some_and(|selected| selected == &run_id || self.plan_runs.contains_key(selected));
        if !selected_run_is_known {
            self.active_plan_runs.insert(scope_path, run_id.clone());
        }
        let changed = self.plan_runs.get(&run_id) != Some(&run);
        self.plan_runs.insert(run_id, run);
        changed
    }

    pub(crate) fn current_plan_dashboard(&self) -> Option<view::PlanDashboard> {
        if self.focused_panel != PanelFocus::Worktrees {
            return None;
        }
        let (repo, run_id) = self.selected_plan_run_id()?;
        let mut run = self.plan_runs.get(&run_id)?.clone();
        let run_scope_path = run.run.scope_path.clone();
        run.run.selected_step = self.resolved_plan_step_selection(&run);
        let output_lines = crate::observability::with_writable_db(&repo, |conn| {
            load_output_lines(conn, &run.run.id, run.run.selected_step)
        })
        .unwrap_or_default();
        let mut output_state = self
            .plan_output_state_by_run
            .get(&run.run.id)
            .cloned()
            .unwrap_or_else(|| view::PlanOutputViewerState {
                cursor: output_lines.len().saturating_sub(1),
                follow: true,
                expanded_blocks: BTreeSet::new(),
            });
        if output_state.follow {
            output_state.cursor = output_lines.len().saturating_sub(1);
        } else if !output_lines.is_empty() {
            output_state.cursor = output_state
                .cursor
                .min(output_lines.len().saturating_sub(1));
        }
        Some(view::PlanDashboard {
            run,
            runs: self.plan_run_summaries_for_scope(&repo.root, &run_scope_path, Some(&run_id)),
            output_lines,
            output_state,
        })
    }

    fn selected_plan_run_id(&self) -> Option<(Repository, String)> {
        let (repo, scope_path) = self.selected_plan_scope()?;
        let run_ids = self.plan_run_ids_for_scope(&repo.root, &scope_path);
        let selected = self
            .active_plan_runs
            .get(&scope_path)
            .filter(|run_id| run_ids.iter().any(|candidate| candidate == *run_id))
            .cloned()
            .or_else(|| run_ids.first().cloned())?;
        Some((repo, selected))
    }

    fn plan_run_ids_for_scope(&self, repo_root: &Path, scope_path: &Path) -> Vec<String> {
        let repo_root = repo_root.display().to_string();
        let mut runs = self
            .plan_runs
            .values()
            .filter(|run| {
                run.run.repo_root == repo_root
                    && run.run.scope_path == scope_path
                    && run.run.archived_unix_ms.is_none()
            })
            .collect::<Vec<_>>();
        runs.sort_by_key(|run| {
            (
                plan_run_status_sort_key(run.run.status),
                std::cmp::Reverse(run.run.updated_unix_ms),
            )
        });
        runs.into_iter().map(|run| run.run.id.clone()).collect()
    }

    fn plan_run_summaries_for_scope(
        &self,
        repo_root: &Path,
        scope_path: &Path,
        selected_run_id: Option<&str>,
    ) -> Vec<view::PlanRunSummary> {
        let selected = self.active_plan_runs.get(scope_path);
        self.plan_run_ids_for_scope(repo_root, scope_path)
            .into_iter()
            .filter_map(|run_id| {
                let run = self.plan_runs.get(&run_id)?;
                Some(view::PlanRunSummary {
                    id: run.run.id.clone(),
                    plan_display: run.run.plan_display.clone(),
                    scope_path: run.run.scope_path.display().to_string(),
                    status: run.run.status,
                    updated_unix_ms: run.run.updated_unix_ms,
                    selected: selected_run_id
                        .map(|selected| selected == run_id.as_str())
                        .unwrap_or(selected == Some(&run_id)),
                })
            })
            .collect()
    }

    pub(crate) fn move_plan_run_selection(&mut self, direction: isize) -> bool {
        let Some((repo, selected_run_id)) = self.selected_plan_run_id() else {
            return false;
        };
        let Some(selected_run) = self.plan_runs.get(&selected_run_id) else {
            return false;
        };
        let scope_path = selected_run.run.scope_path.clone();
        let run_ids = self.plan_run_ids_for_scope(&repo.root, &scope_path);
        if run_ids.len() < 2 {
            return false;
        }
        let current = run_ids
            .iter()
            .position(|run_id| run_id == &selected_run_id)
            .unwrap_or(0);
        let next = if direction < 0 {
            if current == 0 {
                run_ids.len() - 1
            } else {
                current.saturating_sub(direction.unsigned_abs())
            }
        } else {
            (current + direction as usize) % run_ids.len()
        };
        self.active_plan_runs
            .insert(scope_path, run_ids[next].clone());
        true
    }

    fn poll_auto_runs(&mut self) -> bool {
        self.refresh_auto_runs(false)
    }

    fn refresh_auto_runs(&mut self, reconcile_stale: bool) -> bool {
        let mut changed = false;
        let repos = self
            .repos
            .iter()
            .map(|managed| managed.repo.clone())
            .collect::<Vec<_>>();
        for repo in repos {
            let loaded = crate::observability::with_writable_db(&repo, |conn| {
                let mut runs = load_recent_active_runs_for_repo(conn, &repo.root, 8)?;
                if reconcile_stale {
                    for run in &mut runs {
                        let _ = reconcile_stale_auto_run(conn, run);
                    }
                }
                Ok(runs)
            });
            let Ok(runs) = loaded else {
                continue;
            };
            for run in runs {
                changed |= self.remember_auto_run(run);
            }
        }
        changed
    }

    pub(crate) fn load_auto_run_snapshot(&mut self, repo_root: &Path, run_id: &str) {
        let repo = Repository {
            root: repo_root.to_path_buf(),
        };
        if let Ok(Some(run)) =
            crate::observability::with_writable_db(&repo, |conn| load_auto_run(conn, run_id))
        {
            self.remember_auto_run(run);
        }
    }

    pub(crate) fn remember_auto_run(&mut self, run: PersistedAutoRun) -> bool {
        let run_id = run.run.id.clone();
        let selected_step = self
            .selected_auto_step_by_run
            .get(&run_id)
            .copied()
            .or(run.run.selected_step_run_id)
            .or_else(|| run.steps.first().and_then(|step| step.id));
        if let Some(selected_step) = selected_step {
            self.selected_auto_step_by_run
                .insert(run_id.clone(), selected_step);
        }
        self.active_auto_runs
            .insert(run.run.worktree_path.clone(), run_id.clone());
        if self.selected_auto_run.is_none() {
            self.selected_auto_run = Some(run_id.clone());
        }
        let changed = self.auto_runs.get(&run_id) != Some(&run);
        self.auto_runs.insert(run_id, run);
        changed
    }

    pub(crate) fn current_auto_dashboard(&self) -> Option<view::AutoDashboard> {
        let (repo, worktree_path) = self.selected_auto_scope()?;
        let run_id = self.active_auto_runs.get(&worktree_path)?;
        let mut run = self.auto_runs.get(run_id)?.clone();
        if let Some(selected_step) = self.selected_auto_step_by_run.get(run_id).copied() {
            run.run.selected_step_run_id = Some(selected_step);
        }
        let selected_step_run_id = run
            .run
            .selected_step_run_id
            .or_else(|| run.steps.first().and_then(|step| step.id));
        let output_lines = selected_step_run_id
            .and_then(|step_run_id| {
                crate::observability::with_writable_db(&repo, |conn| {
                    load_auto_output_lines(conn, step_run_id)
                })
                .ok()
            })
            .unwrap_or_default();
        let mut output_state = self
            .auto_output_state_by_run
            .get(&run.run.id)
            .cloned()
            .unwrap_or_else(|| view::AutoOutputViewerState {
                cursor: output_lines.len().saturating_sub(1),
                follow: true,
            });
        if output_state.follow {
            output_state.cursor = output_lines.len().saturating_sub(1);
        } else if !output_lines.is_empty() {
            output_state.cursor = output_state
                .cursor
                .min(output_lines.len().saturating_sub(1));
        }
        let linked_plan_dashboard = run
            .steps
            .iter()
            .find(|step| step.id == selected_step_run_id)
            .and_then(|step| step.plan_run_id.as_deref())
            .and_then(|plan_run_id| self.linked_plan_dashboard(&repo, plan_run_id));
        Some(view::AutoDashboard {
            run,
            linked_plan_dashboard,
            output_lines,
            output_state,
        })
    }

    fn linked_plan_dashboard(
        &self,
        repo: &Repository,
        plan_run_id: &str,
    ) -> Option<view::PlanDashboard> {
        let mut run = self.plan_runs.get(plan_run_id).cloned().or_else(|| {
            crate::observability::with_writable_db(repo, |conn| load_plan_run(conn, plan_run_id))
                .ok()
                .flatten()
        })?;
        let run_scope_path = run.run.scope_path.clone();
        run.run.selected_step = self.resolved_plan_step_selection(&run);
        let output_lines = crate::observability::with_writable_db(repo, |conn| {
            load_output_lines(conn, &run.run.id, run.run.selected_step)
        })
        .unwrap_or_default();
        let mut output_state = self
            .plan_output_state_by_run
            .get(&run.run.id)
            .cloned()
            .unwrap_or_else(|| view::PlanOutputViewerState {
                cursor: output_lines.len().saturating_sub(1),
                follow: true,
                expanded_blocks: BTreeSet::new(),
            });
        if output_state.follow {
            output_state.cursor = output_lines.len().saturating_sub(1);
        } else if !output_lines.is_empty() {
            output_state.cursor = output_state
                .cursor
                .min(output_lines.len().saturating_sub(1));
        }
        Some(view::PlanDashboard {
            run,
            runs: self.plan_run_summaries_for_scope(&repo.root, &run_scope_path, Some(plan_run_id)),
            output_lines,
            output_state,
        })
    }

    fn resolved_plan_step_selection(&self, run: &PersistedPlanRun) -> usize {
        if self.manual_plan_step_selection_by_run.contains(&run.run.id) {
            return self
                .selected_plan_step_by_run
                .get(&run.run.id)
                .copied()
                .filter(|selected| run.steps.iter().any(|step| step.step == *selected))
                .unwrap_or_else(|| preferred_plan_step(run));
        }
        preferred_plan_step(run)
    }

    fn selected_auto_scope(&self) -> Option<(Repository, PathBuf)> {
        match self.focused_panel {
            PanelFocus::Worktrees => {
                let context = self.selected_worktree_context()?;
                Some((
                    context.repo,
                    self.sessions.get(context.session_index)?.path.clone(),
                ))
            }
            PanelFocus::Status => {
                let run_id = self.selected_status_auto_run_id()?;
                let run = self.auto_runs.get(run_id)?;
                Some((
                    Repository {
                        root: PathBuf::from(&run.run.repo_root),
                    },
                    run.run.worktree_path.clone(),
                ))
            }
            PanelFocus::Repos => None,
        }
    }

    fn selected_status_auto_run_id(&self) -> Option<&str> {
        if let Some(run_id) = self.selected_auto_run.as_deref()
            && self.auto_runs.contains_key(run_id)
            && self
                .active_auto_runs
                .values()
                .any(|active| active == run_id)
        {
            return Some(run_id);
        }

        self.active_auto_runs
            .values()
            .filter_map(|run_id| {
                self.auto_runs
                    .get(run_id)
                    .map(|run| (run_id.as_str(), run.run.updated_unix_ms))
            })
            .max_by_key(|(_, updated_unix_ms)| *updated_unix_ms)
            .map(|(run_id, _)| run_id)
    }

    fn selected_plan_scope(&self) -> Option<(Repository, PathBuf)> {
        match self.focused_panel {
            PanelFocus::Worktrees => {
                let context = self.selected_worktree_context()?;
                Some((
                    context.repo,
                    self.sessions.get(context.session_index)?.path.clone(),
                ))
            }
            PanelFocus::Status | PanelFocus::Repos => None,
        }
    }

    fn move_plan_step_selection(&mut self, direction: isize) -> bool {
        let Some(dashboard) = self.current_plan_dashboard() else {
            return false;
        };
        let run_id = dashboard.run.run.id.clone();
        let steps = dashboard
            .run
            .steps
            .iter()
            .map(|step| step.step)
            .collect::<Vec<_>>();
        let current_step = self
            .selected_plan_step_by_run
            .get(&run_id)
            .copied()
            .unwrap_or(dashboard.run.run.selected_step);
        let current = steps
            .iter()
            .position(|step| *step == current_step)
            .unwrap_or(0);
        self.manual_plan_step_selection_by_run
            .insert(run_id.clone());
        let next = current as isize + direction;
        if next < 0 {
            return true;
        }
        if let Some(step) = steps.get(next as usize).copied() {
            self.selected_plan_step_by_run.insert(run_id, step);
        }
        true
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
                let repo_label = self
                    .repos
                    .get(session.repo_index)
                    .map(|repo| repo.label.clone())
                    .unwrap_or_else(|| session.repo_label.clone());
                let auto_status = self
                    .active_auto_runs
                    .get(&session.path)
                    .and_then(|run_id| self.auto_runs.get(run_id))
                    .map(|run| run.run.status);
                let plan_status = self
                    .active_plan_runs
                    .get(&session.path)
                    .and_then(|run_id| self.plan_runs.get(run_id))
                    .map(|run| run.run.status);
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
                    status_label: session.status_label.clone(),
                    pr: session.pr.clone(),
                    wt_columns: session.wt_columns.clone(),
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
            dialog: self.dialog.clone(),
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

        let parts = [
            (view::RepoHealthKind::Dirty, dirty),
            (view::RepoHealthKind::Agents, running),
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

    fn status_rows(&self) -> Vec<view::StatusRow> {
        let mut running = 0;
        let mut attention = 0;
        let mut prs = 0;
        let mut ci_failed = 0;
        let mut ci_running = 0;
        let mut dirty = 0;
        let mut behind = 0;
        let mut active_plans = 0;
        let mut failed_plans = 0;
        let mut active_auto = 0;
        let mut failed_auto = 0;
        for run in self.auto_runs.values() {
            match run.run.status {
                AutoRunStatus::Queued | AutoRunStatus::Running | AutoRunStatus::Paused => {
                    active_auto += 1
                }
                AutoRunStatus::Failed | AutoRunStatus::Aborted => failed_auto += 1,
                AutoRunStatus::Done => {}
            }
        }
        for run in self.plan_runs.values() {
            match run.run.status {
                PlanRunStatus::Queued | PlanRunStatus::Running | PlanRunStatus::Paused => {
                    active_plans += 1
                }
                PlanRunStatus::Failed | PlanRunStatus::Aborted => failed_plans += 1,
                PlanRunStatus::Draft | PlanRunStatus::Done => {}
            }
        }
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

    fn leader_hint_model(&self) -> Option<view::LeaderHintModel> {
        match (self.leader_hint, self.focused_panel) {
            (Some(LeaderHint::Root), PanelFocus::Status) => Some(choice_list(
                "Shortcuts",
                &[
                    ("g", "git actions"),
                    ("p", "plan actions"),
                    ("0", "focus main"),
                ],
            )),
            (Some(LeaderHint::Root), PanelFocus::Repos) => Some(choice_list(
                "Shortcuts",
                &[
                    ("g", "git actions"),
                    ("C", "open remote PR"),
                    ("W", "worktree columns"),
                    ("0", "focus main"),
                    ("space/enter", "open default tmux"),
                ],
            )),
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
            (Some(LeaderHint::Git), PanelFocus::Status) => Some(choice_list(
                "Git Actions",
                &[("g", "lazygit after focusing repos/worktrees")],
            )),
            (Some(LeaderHint::Git), PanelFocus::Repos) => Some(choice_list(
                "Git Actions",
                &[("g", "lazygit"), ("p", "pull default branch")],
            )),
            (Some(LeaderHint::Git), PanelFocus::Worktrees) => Some(choice_list(
                "Git Actions",
                &[
                    ("a", "auto flow"),
                    ("g", "lazygit"),
                    ("o", "open PR"),
                    ("P", "push/create PR"),
                    ("M", "merge"),
                    ("c", "copy CI prompt"),
                    ("f", "review fix"),
                ],
            )),
            (None, _) => None,
        }
    }
}

fn choice_list(title: &str, choices: &[(&str, &str)]) -> view::ChoiceList {
    view::ChoiceList {
        title: title.to_string(),
        choices: choices
            .iter()
            .map(|(key, label)| view::KeyChoice {
                key: (*key).to_string(),
                label: (*label).to_string(),
            })
            .collect(),
    }
}

fn worktree_sort_name(session: &Session) -> String {
    session
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(&session.branch)
        .to_ascii_lowercase()
}

fn worktree_priority_rank(visibility: i16) -> u8 {
    match visibility.cmp(&0) {
        std::cmp::Ordering::Greater => 0,
        std::cmp::Ordering::Equal => 1,
        std::cmp::Ordering::Less => 2,
    }
}

fn worktree_updated_label(session: &Session) -> String {
    if let Some(label) = session.pr.last_refreshed.as_deref() {
        return label.to_string();
    }
    if let Some(summary) = &session.pr.summary {
        return summary.updated_at.chars().take(10).collect();
    }
    "-".to_string()
}

fn point_in_rect(x: u16, y: u16, rect: Rect) -> bool {
    x >= rect.x
        && x < rect.x.saturating_add(rect.width)
        && y >= rect.y
        && y < rect.y.saturating_add(rect.height)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::agent::AgentState;
    use crate::auto_flow::{
        AutoImplementationSource, AutoRun, AutoRunMode, AutoRunStatus, PersistedAutoRun,
    };
    use crate::config::{Checks, Config, EscapeKey, MergeMethod};
    use crate::github::PrCache;
    use crate::plan_run::{
        PersistedPlanRun, PlanRun, PlanRunMode, PlanRunStatus, PlanStepRun, PlanStepStatus,
    };
    use crate::repo::Repository;
    use crate::session::Session;
    use crate::view::{RepoMainView, WorktreeMainView};

    use super::{ManagedRepo, OpenTmuxSessionTarget, PanelFocus, Tui, WorktreeListMode};

    #[test]
    fn tui_defaults_to_repos_panel_focus() {
        let tui = test_tui();

        assert_eq!(tui.focused_panel, PanelFocus::Repos);
    }

    #[test]
    fn switching_repos_does_not_change_worktree_selection_until_worktrees_focus() {
        let mut tui = test_tui();

        tui.select_worktree(1);
        tui.select_repo(1);

        assert_eq!(tui.selected, 1);

        tui.focus_worktrees();

        assert_eq!(tui.selected_worktree_index(), Some(3));
    }

    #[test]
    fn repeated_worktree_focus_does_not_change_list_mode() {
        let mut tui = test_tui();
        tui.focus_worktrees();

        assert_eq!(tui.worktree_list_mode, WorktreeListMode::Repo);
        assert_eq!(tui.visible_session_indices(), vec![1]);

        tui.focus_worktrees();

        assert_eq!(tui.worktree_list_mode, WorktreeListMode::Repo);
        assert_eq!(tui.visible_session_indices(), vec![1]);
    }

    #[test]
    fn persisted_worktree_list_mode_loads_and_updates_on_switch() {
        let temp = unique_temp_dir("prism-tui-ui-state-test");
        let path = temp.join("ui-state.toml");
        crate::ui_state::save_to_path(&path, WorktreeListMode::Global).unwrap();
        let mut tui = test_tui();

        tui.use_persisted_ui_state(path.clone());

        assert_eq!(tui.worktree_list_mode, WorktreeListMode::Global);

        tui.focus_worktrees();
        tui.switch_worktree_list_mode(WorktreeListMode::Repo);

        assert_eq!(tui.worktree_list_mode, WorktreeListMode::Repo);
        assert_eq!(
            crate::ui_state::load_from_path(&path),
            Some(WorktreeListMode::Repo)
        );

        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn worktree_filter_clear_restores_remembered_worktree() {
        let mut tui = test_tui();
        tui.select_worktree(1);

        tui.worktree_filter = "main".to_string();
        tui.restore_selected_worktree_for_repo();

        assert_eq!(tui.selected_worktree_index(), None);

        tui.worktree_filter.clear();
        tui.restore_selected_worktree_for_repo();

        assert_eq!(tui.selected_worktree_index(), Some(1));
    }

    #[test]
    fn hidden_sessions_are_not_visible_in_normal_worktree_list() {
        let mut tui = test_tui();
        tui.sessions[1].hidden = true;
        tui.selected = 1;

        assert!(!tui.visible_session_indices().contains(&1));
        assert_eq!(tui.selected_worktree_index(), None);
    }

    #[test]
    fn horizontal_keys_switch_repo_view_without_changing_focus() {
        let mut tui = test_tui();
        tui.focused_panel = PanelFocus::Repos;

        tui.move_right();

        assert_eq!(tui.focused_panel, PanelFocus::Repos);
        assert_eq!(tui.repo_main_view, RepoMainView::Github);

        tui.focus_main();
        tui.move_right();

        assert_eq!(tui.focused_panel, PanelFocus::Repos);
        assert_eq!(tui.repo_main_view, RepoMainView::Kanban);

        tui.move_left();

        assert_eq!(tui.focused_panel, PanelFocus::Repos);
        assert_eq!(tui.repo_main_view, RepoMainView::Github);

        tui.focused_panel = PanelFocus::Worktrees;
        tui.main_focused = false;
        tui.move_left();

        assert_eq!(tui.focused_panel, PanelFocus::Worktrees);
        assert_eq!(tui.repo_main_view, RepoMainView::Github);
    }

    #[test]
    fn worktree_plan_dashboard_is_not_gated_by_horizontal_keys() {
        let mut tui = test_tui();
        tui.focused_panel = PanelFocus::Worktrees;
        tui.select_worktree(1);
        tui.remember_plan_run(test_plan_run("plan", "/repo-one/feature-one"));

        assert_eq!(tui.worktree_main_view, WorktreeMainView::Details);
        assert!(tui.current_plan_dashboard().is_some());

        tui.move_left();

        assert_eq!(tui.focused_panel, PanelFocus::Worktrees);
        assert_eq!(tui.worktree_main_view, WorktreeMainView::Details);
        assert!(tui.current_plan_dashboard().is_some());

        tui.focus_main();
        tui.move_right();

        assert_eq!(tui.focused_panel, PanelFocus::Worktrees);
        assert_eq!(tui.worktree_main_view, WorktreeMainView::Details);
        assert!(tui.current_plan_dashboard().is_some());

        tui.move_left();

        assert_eq!(tui.focused_panel, PanelFocus::Worktrees);
        assert_eq!(tui.worktree_main_view, WorktreeMainView::Details);
        assert!(tui.current_plan_dashboard().is_some());
    }

    #[test]
    fn plan_runs_for_same_worktree_keep_independent_selection_history() {
        let mut tui = test_tui();
        tui.focused_panel = PanelFocus::Worktrees;
        tui.select_worktree(1);
        tui.worktree_main_view = WorktreeMainView::Plan;
        let mut first = test_plan_run("plan-a", "/repo-one/feature-one");
        first.run.updated_unix_ms = 10;
        let mut second = test_plan_run("plan-b", "/repo-one/feature-one");
        second.run.updated_unix_ms = 20;

        tui.remember_plan_run(first);
        tui.remember_plan_run(second);

        let dashboard = tui.current_plan_dashboard().unwrap();
        assert_eq!(dashboard.run.run.id, "plan-a");
        assert_eq!(dashboard.runs.len(), 2);

        assert!(tui.move_plan_run_selection(1));

        let dashboard = tui.current_plan_dashboard().unwrap();
        assert_eq!(dashboard.run.run.id, "plan-b");
        assert_eq!(dashboard.runs.iter().filter(|run| run.selected).count(), 1);
    }

    #[test]
    fn open_tmux_session_target_blocks_status_enter() {
        let mut tui = test_tui();
        tui.focused_panel = PanelFocus::Status;

        assert_eq!(
            tui.open_tmux_session_target(),
            OpenTmuxSessionTarget::Blocked("status has no Enter action")
        );
    }

    #[test]
    fn open_tmux_session_target_blocks_status_enter_with_auto_run() {
        let mut tui = test_tui();
        tui.focused_panel = PanelFocus::Status;
        tui.remember_auto_run(test_auto_run("auto", "/repo-one/feature-one", 20));

        assert_eq!(
            tui.open_tmux_session_target(),
            OpenTmuxSessionTarget::Blocked("status has no Enter action")
        );
    }

    #[test]
    fn open_tmux_session_target_opens_repo_default_from_repos() {
        let mut tui = test_tui();
        tui.focused_panel = PanelFocus::Repos;

        assert_eq!(
            tui.open_tmux_session_target(),
            OpenTmuxSessionTarget::RepoDefaultAgent(0)
        );
    }

    #[test]
    fn open_tmux_session_target_ignores_worktree_filter_for_repo_default() {
        let mut tui = test_tui();
        tui.focused_panel = PanelFocus::Repos;
        tui.worktree_filter = "missing".to_string();

        assert_eq!(
            tui.open_tmux_session_target(),
            OpenTmuxSessionTarget::RepoDefaultAgent(0)
        );
    }

    #[test]
    fn open_tmux_session_target_opens_feature_worktree_agent() {
        let mut tui = test_tui();
        tui.focused_panel = PanelFocus::Worktrees;
        tui.select_worktree(1);

        assert_eq!(
            tui.open_tmux_session_target(),
            OpenTmuxSessionTarget::WorktreeAgent
        );
    }

    #[test]
    fn open_tmux_session_target_opens_selected_plan_phase_from_main() {
        let mut tui = test_tui();
        tui.focused_panel = PanelFocus::Worktrees;
        tui.select_worktree(1);
        tui.focus_main();
        tui.remember_plan_run(test_plan_run_with_steps("plan", "/repo-one/feature-one", 1));

        assert_eq!(
            tui.open_tmux_session_target(),
            OpenTmuxSessionTarget::PlanPhaseAgent
        );
    }

    #[test]
    fn open_tmux_session_target_blocks_default_branch_in_worktree_panel() {
        let mut tui = test_tui();
        tui.focused_panel = PanelFocus::Worktrees;
        tui.select_worktree(0);

        assert_eq!(
            tui.open_tmux_session_target(),
            OpenTmuxSessionTarget::Blocked("selected repository has no visible worktrees")
        );
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

    #[test]
    fn status_auto_dashboard_uses_selected_run() {
        let mut tui = test_tui();
        tui.focused_panel = PanelFocus::Status;
        tui.remember_auto_run(test_auto_run("run-a", "/repo-one/a-worktree", 10));
        tui.remember_auto_run(test_auto_run("run-b", "/repo-one/z-worktree", 20));
        tui.selected_auto_run = Some("run-b".to_string());

        let dashboard = tui.current_auto_dashboard().unwrap();

        assert_eq!(dashboard.run.run.id, "run-b");
        assert_eq!(
            dashboard.run.run.worktree_path,
            PathBuf::from("/repo-one/z-worktree")
        );
    }

    #[test]
    fn standalone_plan_dashboard_is_hidden_outside_worktrees() {
        let mut tui = test_tui();
        tui.focused_panel = PanelFocus::Status;
        tui.remember_plan_run(test_plan_run("plan", "/repo-one"));

        assert!(tui.current_plan_dashboard().is_none());

        tui.focused_panel = PanelFocus::Repos;

        assert!(tui.current_plan_dashboard().is_none());
    }

    #[test]
    fn plan_step_selection_follows_persisted_active_step_until_manual_navigation() {
        let mut tui = test_tui();
        let mut run = test_plan_run_with_steps("plan", "/repo-one/feature-one", 1);

        tui.remember_plan_run(run.clone());
        assert_eq!(tui.selected_plan_step_by_run.get("plan"), Some(&1));

        run.run.selected_step = 2;
        run.steps[0].status = PlanStepStatus::Done;
        run.steps[0].finished_unix_ms = Some(20);
        run.steps[1].status = PlanStepStatus::Running;
        run.steps[1].started_unix_ms = Some(30);
        tui.remember_plan_run(run.clone());
        assert_eq!(tui.selected_plan_step_by_run.get("plan"), Some(&2));

        tui.focused_panel = PanelFocus::Worktrees;
        tui.select_worktree(1);
        tui.worktree_main_view = WorktreeMainView::Plan;
        tui.move_plan_step_selection(-1);
        assert_eq!(tui.selected_plan_step_by_run.get("plan"), Some(&1));

        run.run.selected_step = 3;
        run.steps[1].status = PlanStepStatus::Done;
        run.steps[1].finished_unix_ms = Some(40);
        run.steps[2].status = PlanStepStatus::Running;
        run.steps[2].started_unix_ms = Some(50);
        tui.remember_plan_run(run);
        assert_eq!(tui.selected_plan_step_by_run.get("plan"), Some(&1));
    }

    #[test]
    fn plan_step_selection_prefers_latest_finished_step_after_completion() {
        let mut tui = test_tui();
        let mut run = test_plan_run_with_steps("plan", "/repo-one", 1);
        run.run.status = PlanRunStatus::Done;
        run.run.selected_step = 1;
        for (index, step) in run.steps.iter_mut().enumerate() {
            step.status = PlanStepStatus::Done;
            step.finished_unix_ms = Some(10 + index as u64);
        }

        tui.remember_plan_run(run);

        assert_eq!(tui.selected_plan_step_by_run.get("plan"), Some(&3));
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

    fn test_auto_run(id: &str, worktree_path: &str, updated_unix_ms: u64) -> PersistedAutoRun {
        PersistedAutoRun {
            run: AutoRun {
                id: id.to_string(),
                repo_root: "/repo-one".to_string(),
                worktree_path: PathBuf::from(worktree_path),
                branch: "feature".to_string(),
                mode: AutoRunMode::Standard,
                implementation_source: AutoImplementationSource::Prompt,
                plan_path: None,
                plan_run_mode: PlanRunMode::Sequential,
                variant: "default".to_string(),
                agent_profile: None,
                prompt_summary: id.to_string(),
                initial_prompt: String::new(),
                status: AutoRunStatus::Running,
                pause_requested: false,
                selected_step_run_id: None,
                pr_number: None,
                pr_url: None,
                current_head_sha: None,
                review_baseline_json: None,
                stabilization_status: None,
                stabilization_blocker: None,
                stabilization_next_work: None,
                pending_push: None,
                created_unix_ms: 1,
                updated_unix_ms,
                archived_unix_ms: None,
            },
            steps: Vec::new(),
        }
    }

    fn test_plan_run(id: &str, scope_path: &str) -> PersistedPlanRun {
        PersistedPlanRun {
            run: PlanRun {
                id: id.to_string(),
                repo_root: "/repo-one".to_string(),
                scope_path: PathBuf::from(scope_path),
                plan_path: PathBuf::from("plan.md"),
                plan_display: "plan.md".to_string(),
                step_name: "phase".to_string(),
                start_step: 1,
                total_steps: 1,
                mode: PlanRunMode::Sequential,
                status: PlanRunStatus::Running,
                pause_requested: false,
                selected_step: 1,
                created_unix_ms: 1,
                updated_unix_ms: 1,
                archived_unix_ms: None,
            },
            steps: Vec::new(),
        }
    }

    fn test_plan_run_with_steps(
        id: &str,
        scope_path: &str,
        selected_step: usize,
    ) -> PersistedPlanRun {
        let mut run = test_plan_run(id, scope_path);
        run.run.total_steps = 3;
        run.run.selected_step = selected_step;
        run.steps = (1..=3)
            .map(|step| PlanStepRun {
                run_id: id.to_string(),
                step,
                prompt: format!("phase {step}"),
                status: if step == selected_step {
                    PlanStepStatus::Running
                } else {
                    PlanStepStatus::Queued
                },
                opencode_state: None,
                opencode_server_url: None,
                opencode_session_id: None,
                process_id: None,
                agent_variant: None,
                started_unix_ms: (step == selected_step).then_some(step as u64),
                finished_unix_ms: None,
                exit_code: None,
                latest_message: None,
                active_tool: None,
                todos: Vec::new(),
                summary: None,
                error: None,
            })
            .collect();
        run
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
            classification: crate::session::SessionClassification::Work,
            visibility: 0,
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
            user_path: PathBuf::from("/tmp/prism-user.toml"),
            repo_config_path: PathBuf::from("/tmp/prism-repo.toml"),
        }
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{unique}", std::process::id()))
    }
}
