use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::layout::Rect;
use ratatui::text::Line;

use crate::agent::AgentState;
use crate::agent_session::{AgentSessionSlot, AgentSessionWarmupKey, AgentSessionWarmupResult};
use crate::auto_flow::{
    AutoOutputLine, AutoRunStatus, PersistedAutoRun, load_auto_run_snapshot,
    load_output_lines as load_auto_output_lines, load_recent_active_run_snapshots_for_repo,
};
use crate::config::Config;
use crate::github::{PrCache, PrSummary};
use crate::input::{Key, KeyInput};
use crate::opencode::{OpencodeEvent, OpencodeStatus};
use crate::plan_run::{
    PersistedPlanRun, PlanOutputLine, PlanRunStatus, PlanStepStatus, load_output_lines,
    load_plan_run, load_recent_plan_runs_for_repo,
};
use crate::repo::Repository;
use crate::session::{Session, WorktreeRepositoryKey, WorktreeSessionKey};
use crate::terminal::stdin_is_tty;
use crate::tmux::TmuxWindow;
use crate::tui_jobs::{
    JobContext, JobMessage, JobMetadata, JobOutcome, JobRegistry, LatestReceiver, LatestSender,
    latest_channel,
};
use crate::tui_runtime::{RuntimeEvent, TerminalRuntime};
use crate::tui_signal::{ShutdownNotification, ShutdownSignal};
use crate::util::status_count;
use crate::view;

pub struct Tui {
    pub(crate) repo: Repository,
    pub(crate) config: Config,
    pub(crate) repos: Vec<ManagedRepo>,
    pub(crate) current_repo: usize,
    pub(crate) sessions: Vec<Session>,
    pub(crate) session_repository_identities: BTreeMap<usize, WorktreeRepositoryKey>,
    pub(crate) worktree_generations: BTreeMap<WorktreeSessionKey, u64>,
    pub(crate) worktree_harness_configs: BTreeMap<WorktreeSessionKey, Config>,
    pub(crate) session_refresh_tx: LatestSender<u64, SessionRefreshResult>,
    pub(crate) session_refresh_rx: LatestReceiver<u64, SessionRefreshResult>,
    pub(crate) session_refresh_in_flight: bool,
    pub(crate) session_refresh_pending: bool,
    pub(crate) session_inventory_generation: u64,
    agent_state_persistence_in_flight: BTreeSet<WorktreeSessionKey>,
    agent_state_persistence_pending: BTreeMap<WorktreeSessionKey, AgentStatePersistenceRequest>,
    workflow_maintenance_tx: LatestSender<(), ()>,
    workflow_maintenance_rx: LatestReceiver<(), ()>,
    workflow_maintenance_in_flight: bool,
    workflow_maintenance_due: bool,
    workflow_maintenance_last_started: Instant,
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
    pub(crate) selected_pr_by_repo: BTreeMap<PathBuf, u64>,
    pub(crate) pr_poll_tx: LatestSender<PrDeliveryKey, PrPollResult>,
    pub(crate) pr_poll_rx: LatestReceiver<PrDeliveryKey, PrPollResult>,
    pub(crate) pr_polls_in_flight: BTreeSet<PrPollKey>,
    pub(crate) pr_persistence_in_flight: BTreeSet<PrPollKey>,
    pub(crate) pr_persistence_pending: BTreeMap<PrPollKey, PrPersistenceRequest>,
    pub(crate) pr_persistence_versions: BTreeMap<PrPollKey, u64>,
    pub(crate) delete_session_tx: LatestSender<(DeleteSessionKey, u64), DeleteSessionResult>,
    pub(crate) delete_session_rx: LatestReceiver<(DeleteSessionKey, u64), DeleteSessionResult>,
    pub(crate) delete_sessions_in_flight: BTreeSet<DeleteSessionKey>,
    pub(crate) tmux_warmup_tx: LatestSender<AgentSessionWarmupKey, AgentSessionWarmupResult>,
    pub(crate) tmux_warmup_rx: LatestReceiver<AgentSessionWarmupKey, AgentSessionWarmupResult>,
    pub(crate) tmux_warmups_in_flight: BTreeSet<AgentSessionWarmupKey>,
    pub(crate) tmux_generations: BTreeMap<AgentSessionSlot, u64>,
    pub(crate) tmux_portal_tx: LatestSender<AgentSessionWarmupKey, TmuxPortalResult>,
    pub(crate) tmux_portal_rx: LatestReceiver<AgentSessionWarmupKey, TmuxPortalResult>,
    pub(crate) tmux_portal_polls_in_flight: BTreeMap<AgentSessionWarmupKey, Instant>,
    pub(crate) tmux_portal_last_polled: BTreeMap<AgentSessionWarmupKey, Instant>,
    pub(crate) tmux_portal: Option<TmuxPortalSnapshot>,
    pub(crate) tmux_portal_size: Option<(u16, u16)>,
    pub(crate) tmux_portal_resized: Option<(AgentSessionWarmupKey, (u16, u16))>,
    pub(crate) wt_poll_tx: LatestSender<WorktreeRepositoryKey, WtPollResult>,
    pub(crate) wt_poll_rx: LatestReceiver<WorktreeRepositoryKey, WtPollResult>,
    pub(crate) default_branch_poll_tx: LatestSender<WorktreeSessionKey, DefaultBranchPollResult>,
    pub(crate) default_branch_poll_rx: LatestReceiver<WorktreeSessionKey, DefaultBranchPollResult>,
    pub(crate) opencode_poll_tx: LatestSender<OpencodePollKey, OpencodePollResult>,
    pub(crate) opencode_poll_rx: LatestReceiver<OpencodePollKey, OpencodePollResult>,
    pub(crate) opencode_polls_in_flight: BTreeSet<OpencodePollKey>,
    pub(crate) opencode_last_polled: BTreeMap<OpencodePollKey, Instant>,
    pub(crate) opencode_last_state_event: BTreeMap<OpencodePollKey, Instant>,
    pub(crate) opencode_reconcile_requested: BTreeMap<WorktreeSessionKey, Instant>,
    pub(crate) opencode_event_watermarks: BTreeMap<WorktreeSessionKey, Instant>,
    #[cfg(test)]
    pub(crate) opencode_event_tx: LatestSender<Instant, OpencodeEventResult>,
    #[cfg(test)]
    pub(crate) opencode_event_rx: LatestReceiver<Instant, OpencodeEventResult>,
    pub(crate) opencode_listeners: BTreeSet<OpencodeListenerKey>,
    pub(crate) opencode_listener_last_scanned: Option<Instant>,
    pub(crate) jobs: JobRegistry<TuiJobKind, TuiJobKey, TuiJobPayload>,
    pub(crate) opencode_events_changed: bool,
    pub(crate) tui_tick_active: bool,
    pub(crate) routing_tui_jobs: bool,
    scheduling_stopped: bool,
    flight_recorder_servers: Vec<crate::flight_recorder::ServerGuard>,
    pub(crate) plan_runs: BTreeMap<String, PersistedPlanRun>,
    pub(crate) active_plan_runs: BTreeMap<PathBuf, String>,
    pub(crate) selected_plan_step_by_run: BTreeMap<String, usize>,
    pub(crate) manual_plan_step_selection_by_run: BTreeSet<String>,
    pub(crate) plan_output_state_by_run: BTreeMap<String, view::PlanOutputViewerState>,
    pub(crate) plan_output_cache: RefCell<BTreeMap<(String, usize), Vec<PlanOutputLine>>>,
    pub(crate) auto_runs: BTreeMap<String, PersistedAutoRun>,
    pub(crate) active_auto_runs: BTreeMap<PathBuf, String>,
    pub(crate) selected_auto_run: Option<String>,
    pub(crate) selected_auto_step_by_run: BTreeMap<String, i64>,
    pub(crate) auto_output_state_by_run: BTreeMap<String, view::AutoOutputViewerState>,
    pub(crate) auto_output_cache:
        RefCell<BTreeMap<(WorktreeRepositoryKey, i64), Vec<AutoOutputLine>>>,
    workflow_poll_tx: LatestSender<WorktreeRepositoryKey, WorkflowPollResult>,
    workflow_poll_rx: LatestReceiver<WorktreeRepositoryKey, WorkflowPollResult>,
    workflow_polls_in_flight: BTreeSet<WorktreeRepositoryKey>,
    workflow_last_polled: BTreeMap<WorktreeRepositoryKey, Instant>,
    workflow_revision: u64,
    linked_plan_runs: BTreeMap<String, PersistedPlanRun>,
    dashboard_output_tx: LatestSender<DashboardOutputKey, DashboardOutputResult>,
    dashboard_output_rx: LatestReceiver<DashboardOutputKey, DashboardOutputResult>,
    dashboard_outputs_in_flight: BTreeSet<DashboardOutputKey>,
    dashboard_output_last_polled: BTreeMap<DashboardOutputKey, Instant>,
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
const WORKFLOW_MAINTENANCE_INTERVAL: Duration = Duration::from_secs(60 * 60);
pub(crate) const TUI_ACTION_JOB_TIMEOUT: Duration = Duration::from_secs(120);
const TUI_JOB_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
const TUI_TICK_ITEM_BUDGET: usize = 32;
const TUI_TICK_TIME_BUDGET: Duration = Duration::from_millis(8);
#[derive(Clone, Debug)]
pub(crate) struct ManagedRepo {
    pub repo: Repository,
    pub config: Config,
    pub label: String,
    pub key: Option<char>,
    pub identity: WorktreeRepositoryKey,
    pub pr_summary_poll_in_flight: bool,
    pub pr_summary_last_polled: Option<std::time::Instant>,
    pub pr_summaries: Vec<PrSummary>,
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

pub(crate) struct TmuxPortalResult {
    pub key: AgentSessionWarmupKey,
    pub started_at: Instant,
    pub capture: Result<Vec<Line<'static>>, String>,
    pub resized_size: Option<(u16, u16)>,
}

pub(crate) struct TmuxPortalTarget {
    pub key: AgentSessionWarmupKey,
    pub repo: Repository,
    pub config: Config,
    pub size: (u16, u16),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TmuxPortalSnapshot {
    pub key: AgentSessionWarmupKey,
    pub capture: Option<TmuxPortalCapture>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TmuxPortalCapture {
    pub key: AgentSessionWarmupKey,
    pub result: Result<Vec<Line<'static>>, String>,
}

pub(crate) fn load_worktree_harness_configs(
    repos: &[ManagedRepo],
    sessions: &[Session],
) -> BTreeMap<WorktreeSessionKey, Config> {
    sessions
        .iter()
        .filter_map(|session| {
            let managed = repos.get(session.repo_index)?;
            let association = crate::session::worktree_harness(&managed.repo, session).ok()?;
            let config = managed.config.for_harness(&association.harness_id).ok()?;
            Some((session.identity_key(&managed.identity), config))
        })
        .collect()
}

pub(crate) fn maintain_workflow_storage(repo: &Repository) -> Result<(), String> {
    crate::observability::with_writable_db_named(repo, "workflow.maintenance", |conn| {
        crate::plan_run::cleanup_stale_archived_plan_runs(
            conn,
            crate::plan_run::ARCHIVED_PLAN_RETENTION_MS,
        )?;
        crate::auto_flow::load_recent_active_runs_for_repo(conn, &repo.root, usize::MAX)?;
        Ok(())
    })
}

impl ManagedRepo {
    pub(crate) fn new(repo: Repository, config: Config, key: Option<char>) -> Self {
        let label = crate::workspace::label_for_root(&repo.root);
        Self {
            identity: WorktreeRepositoryKey::new(repo.root.clone()),
            repo,
            config,
            label,
            key,
            pr_summary_poll_in_flight: false,
            pr_summary_last_polled: None,
            pr_summaries: Vec::new(),
            wt_poll_in_flight: false,
            default_branch_poll_in_flight: false,
            default_branch_last_polled: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct PrPollKey {
    pub worktree: WorktreeSessionKey,
    pub generation: u64,
}

pub(crate) enum PrPollResult {
    Summary {
        repository: WorktreeRepositoryKey,
        sessions: Vec<WorktreeSessionKey>,
        github_remote_configured: bool,
        summaries: Result<Vec<PrSummary>, String>,
        observations: Result<Vec<PrSummarySessionResult>, String>,
        refreshed: String,
        poll_started_at: Instant,
    },
    Details {
        key: PrPollKey,
        cache: Box<PrCache>,
    },
    Persistence {
        key: PrPollKey,
        version: u64,
        details: bool,
        result: Result<(), String>,
    },
}

pub(crate) struct PrSummarySessionResult {
    pub key: WorktreeSessionKey,
    pub summary: Option<PrSummary>,
}

pub(crate) struct PrPersistenceRequest {
    pub(crate) key: PrPollKey,
    pub(crate) version: u64,
    pub(crate) details: bool,
    pub(crate) repo: Repository,
    pub(crate) branch: String,
    pub(crate) cache: PrCache,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum PrDeliveryKey {
    Summary(WorktreeRepositoryKey),
    Details(PrPollKey),
    Persistence(PrPollKey),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum DashboardOutputKey {
    Plan {
        repository: WorktreeRepositoryKey,
        run_id: String,
        step: usize,
    },
    Auto {
        repository: WorktreeRepositoryKey,
        step_run_id: i64,
    },
}

pub(crate) struct WorkflowPollSnapshot {
    plan_runs: Result<Vec<PersistedPlanRun>, String>,
    auto_runs: Result<Vec<PersistedAutoRun>, String>,
    linked_plan_runs: Result<Vec<PersistedPlanRun>, String>,
}

pub(crate) struct WorkflowPollResult {
    repository: WorktreeRepositoryKey,
    revision: u64,
    snapshot: Result<WorkflowPollSnapshot, String>,
}

pub(crate) enum DashboardOutputLines {
    Plan(Vec<PlanOutputLine>),
    Auto(Vec<AutoOutputLine>),
}

pub(crate) struct DashboardOutputResult {
    key: DashboardOutputKey,
    revision: u64,
    lines: Result<DashboardOutputLines, String>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct DeleteSessionKey {
    pub worktree: WorktreeSessionKey,
    pub generation: u64,
}

pub(crate) struct DeleteSessionResult {
    pub key: DeleteSessionKey,
    pub delivery_id: u64,
    pub result: Result<crate::session::DeleteWorktreeOutcome, String>,
}

impl PrPollKey {
    pub(crate) fn for_repository_session_generation(
        repository: &WorktreeRepositoryKey,
        session: &Session,
        generation: u64,
    ) -> Self {
        Self {
            worktree: session.identity_key(repository),
            generation,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LeaderHint {
    Root,
    Git,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GitAction {
    LazyGit,
    OpenPr,
    SubmitReview,
    Push,
    Merge,
    CiFix,
    ReviewFix,
    ResolveAllComments,
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
    RepoPr,
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
    pub repository: WorktreeRepositoryKey,
    pub columns: Result<BTreeMap<WorktreeSessionKey, BTreeMap<String, String>>, String>,
}

pub(crate) struct DefaultBranchPollResult {
    pub key: WorktreeSessionKey,
    pub status_label: Result<String, String>,
}

pub(crate) struct SessionRefreshResult {
    pub base_generation: u64,
    pub result: Result<SessionRefreshSnapshot, String>,
}

pub(crate) struct SessionRefreshSnapshot {
    pub repository_identities: BTreeMap<usize, WorktreeRepositoryKey>,
    pub configs: BTreeMap<WorktreeRepositoryKey, Config>,
    pub baseline_sessions: BTreeMap<WorktreeSessionKey, Session>,
    pub sessions: Vec<Session>,
    pub worktree_harness_configs: BTreeMap<WorktreeSessionKey, Config>,
    pub tmux_generations: BTreeMap<AgentSessionSlot, u64>,
}

struct AgentStatePersistenceRequest {
    generation: u64,
    repo: Repository,
    branch: String,
    state: Option<AgentState>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct OpencodePollKey {
    pub worktree: WorktreeSessionKey,
    pub generation: u64,
}

pub(crate) struct OpencodePollResult {
    pub key: OpencodePollKey,
    pub started_at: Instant,
    pub status: Result<OpencodeStatus, String>,
}

pub(crate) struct OpencodeEventResult {
    pub stream: OpencodeListenerKey,
    pub received_at: Instant,
    pub event: Result<OpencodeEvent, String>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct OpencodeListenerKey {
    pub worktree: WorktreeSessionKey,
    pub generation: u64,
    pub session_id: String,
    pub server_url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum TuiJobKind {
    SessionRefresh,
    AgentStatePersistence,
    WorkflowMaintenance,
    PrSummary,
    PrDetails,
    PrPersistence,
    WorkflowPoll,
    DashboardOutput,
    DeleteSession,
    TmuxWarmup,
    TmuxPortal,
    WorktreeColumns,
    DefaultBranch,
    OpencodePoll,
    OpencodeListener,
}

impl TuiJobKind {
    const fn label(&self) -> &'static str {
        match self {
            Self::SessionRefresh => "session_refresh",
            Self::AgentStatePersistence => "agent_state_persistence",
            Self::WorkflowMaintenance => "workflow_maintenance",
            Self::PrSummary => "pr_summary",
            Self::PrDetails => "pr_details",
            Self::PrPersistence => "pr_persistence",
            Self::WorkflowPoll => "workflow_poll",
            Self::DashboardOutput => "dashboard_output",
            Self::DeleteSession => "delete_session",
            Self::TmuxWarmup => "tmux_warmup",
            Self::TmuxPortal => "tmux_portal",
            Self::WorktreeColumns => "worktree_columns",
            Self::DefaultBranch => "default_branch",
            Self::OpencodePoll => "opencode_poll",
            Self::OpencodeListener => "opencode_listener",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum TuiJobKey {
    None,
    Repository(WorktreeRepositoryKey),
    WorkflowRepository(WorktreeRepositoryKey),
    DashboardOutput(DashboardOutputKey),
    Worktree(WorktreeSessionKey),
    AgentStatePersistence(WorktreeSessionKey),
    Pr(PrPollKey),
    PrPersistence(PrPollKey),
    Delete(DeleteSessionKey),
    Tmux(AgentSessionWarmupKey),
    Opencode(OpencodePollKey),
    OpencodeListener(OpencodeListenerKey),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShutdownReason {
    UserQuit,
    Sigint,
    Sigterm,
    RunError,
    Panic,
}

impl ShutdownReason {
    const fn label(self) -> &'static str {
        match self {
            Self::UserQuit => "user_quit",
            Self::Sigint => "sigint",
            Self::Sigterm => "sigterm",
            Self::RunError => "run_error",
            Self::Panic => "panic",
        }
    }
}

pub(crate) enum TuiJobPayload {
    SessionRefresh(SessionRefreshResult),
    WorkflowMaintenance,
    PrPoll(PrPollResult),
    WorkflowPoll(WorkflowPollResult),
    DashboardOutput(DashboardOutputResult),
    DeleteSession(DeleteSessionResult),
    TmuxWarmup(AgentSessionWarmupResult),
    TmuxPortal(TmuxPortalResult),
    WorktreeColumns(WtPollResult),
    DefaultBranch(DefaultBranchPollResult),
    OpencodePoll(OpencodePollResult),
    OpencodeEvent(OpencodeEventResult),
}

impl OpencodePollKey {
    #[cfg(test)]
    pub(crate) fn for_repository_session(
        repository: &WorktreeRepositoryKey,
        session: &Session,
    ) -> Self {
        Self::for_repository_session_generation(repository, session, 0)
    }

    pub(crate) fn for_repository_session_generation(
        repository: &WorktreeRepositoryKey,
        session: &Session,
        generation: u64,
    ) -> Self {
        Self {
            worktree: session.identity_key(repository),
            generation,
        }
    }
}

fn pr_delivery_key(result: &PrPollResult) -> PrDeliveryKey {
    match result {
        PrPollResult::Summary { repository, .. } => PrDeliveryKey::Summary(repository.clone()),
        PrPollResult::Details { key, .. } => PrDeliveryKey::Details(key.clone()),
        PrPollResult::Persistence { key, .. } => PrDeliveryKey::Persistence(key.clone()),
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
    sessions: bool,
    tmux: bool,
    tmux_portal: bool,
    worktree_columns: bool,
    default_branch: bool,
    opencode_status: bool,
    opencode_events: bool,
    workflows: bool,
    dashboard_output: bool,
    pull_requests: bool,
    delete_sessions: bool,
    status_message: bool,
}

impl TuiBackgroundChanges {
    fn any(&self) -> bool {
        self.tmux
            || self.sessions
            || self.tmux_portal
            || self.worktree_columns
            || self.default_branch
            || self.opencode_status
            || self.opencode_events
            || self.workflows
            || self.dashboard_output
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

fn requested_shutdown(notification: &ShutdownNotification) -> Option<ShutdownReason> {
    match notification.signal()? {
        ShutdownSignal::Sigint => Some(ShutdownReason::Sigint),
        ShutdownSignal::Sigterm => Some(ShutdownReason::Sigterm),
    }
}

fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> &str {
    if let Some(message) = payload.downcast_ref::<&str>() {
        message
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.as_str()
    } else {
        "non-string panic payload"
    }
}

fn ctrl_key(event: KeyEvent) -> bool {
    event.modifiers.contains(KeyModifiers::CONTROL)
}

fn confirmation_result(input: &str, default: bool) -> Option<bool> {
    match input.trim().to_ascii_lowercase().as_str() {
        "" => Some(default),
        "y" => Some(true),
        "n" => Some(false),
        _ => None,
    }
}

fn toggle_ordered_item(items: &mut Vec<view::OrderedToggleItem>, selected: &mut usize) {
    if items.is_empty() || *selected >= items.len() {
        return;
    }
    let mut item = items.remove(*selected);
    item.enabled = !item.enabled;
    let insert_at = if item.enabled {
        items.iter().take_while(|item| item.enabled).count()
    } else {
        items.len()
    };
    items.insert(insert_at, item);
    *selected = insert_at;
}

fn toggle_item_in_place(items: &mut [view::OrderedToggleItem], selected: usize) {
    if let Some(item) = items.get_mut(selected) {
        item.enabled = !item.enabled;
    }
}

fn move_enabled_ordered_item(
    items: &mut [view::OrderedToggleItem],
    selected: &mut usize,
    direction: isize,
) {
    if items.is_empty() || *selected >= items.len() || !items[*selected].enabled {
        return;
    }
    let target = if direction < 0 {
        (0..*selected).rev().find(|index| items[*index].enabled)
    } else {
        (*selected + 1..items.len()).find(|index| items[*index].enabled)
    };
    if let Some(target) = target {
        items.swap(*selected, target);
        *selected = target;
    }
}

impl Tui {
    pub fn new(repos: Vec<ManagedRepo>, current_repo: usize, sessions: Vec<Session>) -> Self {
        let (pr_poll_tx, pr_poll_rx) = latest_channel(pr_delivery_key);
        let (workflow_poll_tx, workflow_poll_rx) =
            latest_channel(|result: &WorkflowPollResult| result.repository.clone());
        let (dashboard_output_tx, dashboard_output_rx) =
            latest_channel(|result: &DashboardOutputResult| result.key.clone());
        let (session_refresh_tx, session_refresh_rx) =
            latest_channel(|result: &SessionRefreshResult| result.base_generation);
        let (workflow_maintenance_tx, workflow_maintenance_rx) = latest_channel(|_| ());
        let (delete_session_tx, delete_session_rx) =
            latest_channel(|result: &DeleteSessionResult| (result.key.clone(), result.delivery_id));
        let (tmux_warmup_tx, tmux_warmup_rx) =
            latest_channel(|result: &AgentSessionWarmupResult| result.key.clone());
        let (tmux_portal_tx, tmux_portal_rx) =
            latest_channel(|result: &TmuxPortalResult| result.key.clone());
        let (wt_poll_tx, wt_poll_rx) =
            latest_channel(|result: &WtPollResult| result.repository.clone());
        let (default_branch_poll_tx, default_branch_poll_rx) =
            latest_channel(|result: &DefaultBranchPollResult| result.key.clone());
        let (opencode_poll_tx, opencode_poll_rx) =
            latest_channel(|result: &OpencodePollResult| result.key.clone());
        #[cfg(test)]
        let (opencode_event_tx, opencode_event_rx) =
            latest_channel(|result: &OpencodeEventResult| result.received_at);
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
        let session_repository_identities = repos
            .iter()
            .enumerate()
            .map(|(index, repo)| (index, repo.identity.clone()))
            .collect();
        let worktree_generations = sessions
            .iter()
            .filter_map(|session| {
                let repo = repos.get(session.repo_index)?;
                Some((session.identity_key(&repo.identity), 0))
            })
            .collect();
        let mut tui = Self {
            repo,
            config,
            repos,
            current_repo,
            sessions,
            session_repository_identities,
            worktree_generations,
            worktree_harness_configs: BTreeMap::new(),
            session_refresh_tx,
            session_refresh_rx,
            session_refresh_in_flight: false,
            session_refresh_pending: false,
            session_inventory_generation: 0,
            agent_state_persistence_in_flight: BTreeSet::new(),
            agent_state_persistence_pending: BTreeMap::new(),
            workflow_maintenance_tx,
            workflow_maintenance_rx,
            workflow_maintenance_in_flight: false,
            workflow_maintenance_due: false,
            workflow_maintenance_last_started: Instant::now(),
            selected: 0,
            selected_repo_root: None,
            focused_panel: PanelFocus::Repos,
            main_focused: false,
            main_scroll: 0,
            repo_main_view: view::RepoMainView::ChangeRequests,
            worktree_main_view: view::WorktreeMainView::Details,
            worktree_list_mode: WorktreeListMode::Repo,
            ui_state_path: None,
            selected_comment: 0,
            selected_worktree_by_repo: BTreeMap::new(),
            selected_pr_by_repo: BTreeMap::new(),
            pr_poll_tx,
            pr_poll_rx,
            pr_polls_in_flight: BTreeSet::new(),
            pr_persistence_in_flight: BTreeSet::new(),
            pr_persistence_pending: BTreeMap::new(),
            pr_persistence_versions: BTreeMap::new(),
            delete_session_tx,
            delete_session_rx,
            delete_sessions_in_flight: BTreeSet::new(),
            tmux_warmup_tx,
            tmux_warmup_rx,
            tmux_warmups_in_flight: BTreeSet::new(),
            tmux_generations: BTreeMap::new(),
            tmux_portal_tx,
            tmux_portal_rx,
            tmux_portal_polls_in_flight: BTreeMap::new(),
            tmux_portal_last_polled: BTreeMap::new(),
            tmux_portal: None,
            tmux_portal_size: None,
            tmux_portal_resized: None,
            wt_poll_tx,
            wt_poll_rx,
            default_branch_poll_tx,
            default_branch_poll_rx,
            opencode_poll_tx,
            opencode_poll_rx,
            opencode_polls_in_flight: BTreeSet::new(),
            opencode_last_polled: BTreeMap::new(),
            opencode_last_state_event: BTreeMap::new(),
            opencode_reconcile_requested: BTreeMap::new(),
            opencode_event_watermarks: BTreeMap::new(),
            #[cfg(test)]
            opencode_event_tx,
            #[cfg(test)]
            opencode_event_rx,
            opencode_listeners: BTreeSet::new(),
            opencode_listener_last_scanned: None,
            jobs: JobRegistry::default(),
            opencode_events_changed: false,
            tui_tick_active: false,
            routing_tui_jobs: false,
            scheduling_stopped: false,
            flight_recorder_servers: Vec::new(),
            plan_runs: BTreeMap::new(),
            active_plan_runs: BTreeMap::new(),
            selected_plan_step_by_run: BTreeMap::new(),
            manual_plan_step_selection_by_run: BTreeSet::new(),
            plan_output_state_by_run: BTreeMap::new(),
            plan_output_cache: RefCell::new(BTreeMap::new()),
            auto_runs: BTreeMap::new(),
            active_auto_runs: BTreeMap::new(),
            selected_auto_run: None,
            selected_auto_step_by_run: BTreeMap::new(),
            auto_output_state_by_run: BTreeMap::new(),
            auto_output_cache: RefCell::new(BTreeMap::new()),
            workflow_poll_tx,
            workflow_poll_rx,
            workflow_polls_in_flight: BTreeSet::new(),
            workflow_last_polled: BTreeMap::new(),
            workflow_revision: 0,
            linked_plan_runs: BTreeMap::new(),
            dashboard_output_tx,
            dashboard_output_rx,
            dashboard_outputs_in_flight: BTreeSet::new(),
            dashboard_output_last_polled: BTreeMap::new(),
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

    pub(crate) fn use_persisted_ui_state(&mut self, path: PathBuf) -> Result<(), String> {
        match crate::ui_state::load_from_path(&path) {
            Ok(Some(mode)) => {
                self.worktree_list_mode = mode;
                self.restore_selected_worktree_for_repo();
            }
            Ok(None) => {}
            Err(error) => {
                self.show_message(&format!(
                    "UI state was not loaded; keeping the current mode: {error}"
                ))?;
            }
        }
        self.ui_state_path = Some(path);
        Ok(())
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

    fn git_action_enabled(&self, action: GitAction) -> bool {
        if action == GitAction::LazyGit {
            let program = match self.focused_panel {
                PanelFocus::Status => None,
                PanelFocus::Repos => self
                    .selected_repo_context()
                    .map(|context| context.config.tool("lazygit")),
                PanelFocus::Worktrees => self
                    .selected_worktree_context()
                    .map(|context| context.config.tool("lazygit")),
            };
            return program.is_some_and(|program| crate::process::command_exists(&program));
        }
        if action == GitAction::SubmitReview {
            return self
                .selected_repo_context()
                .is_some_and(|context| crate::process::command_exists(&context.config.tool("gh")))
                && self.focused_panel == PanelFocus::Repos
                && self.main_focused
                && self.selected_repo_pr_summary().is_some_and(|summary| {
                    summary
                        .change_request_identity
                        .as_ref()
                        .is_none_or(|identity| {
                            identity.provider() == crate::remote::ProviderKind::GitHub
                        })
                });
        }
        if self.focused_panel != PanelFocus::Worktrees {
            return false;
        }
        let Some(context) = self.selected_worktree_context() else {
            return false;
        };
        let Some(session) = self.sessions.get(context.session_index) else {
            return false;
        };
        if !session.is_task_branch(&context.config) {
            return false;
        }
        if action == GitAction::Push {
            return true;
        }
        let Some(summary) = session.pr.summary() else {
            return false;
        };
        let capabilities = crate::remote::dispatcher::capabilities_for_summary(summary);
        if action == GitAction::OpenPr {
            return capabilities.fetch_change_request != crate::remote::SupportLevel::Unsupported;
        }
        if summary.merged || !summary.state.eq_ignore_ascii_case("OPEN") {
            return false;
        }
        if action == GitAction::ResolveAllComments {
            return capabilities.resolve_review_thread == crate::remote::SupportLevel::Supported
                && self.main_focused
                && session.pr.trusted_details().is_ok_and(|details| {
                    details.is_some_and(|details| {
                        details.review_comments.iter().any(|comment| {
                            !comment.resolved && !comment.thread_id.trim().is_empty()
                        })
                    })
                });
        }
        if matches!(action, GitAction::CiFix | GitAction::ReviewFix) {
            let supported = match action {
                GitAction::CiFix => capabilities.ci_logs,
                GitAction::ReviewFix => capabilities.review_threads,
                _ => unreachable!(),
            };
            let has_input = session.pr.trusted_details().is_ok_and(|details| {
                details.is_some_and(|details| match action {
                    GitAction::CiFix => !details.ci_failures.is_empty(),
                    GitAction::ReviewFix => {
                        !details.reviews.is_empty() || !details.review_comments.is_empty()
                    }
                    _ => unreachable!(),
                })
            });
            return supported != crate::remote::SupportLevel::Unsupported
                && has_input
                && context
                    .config
                    .selected_harness()
                    .is_ok_and(|harness| harness.describe().headless);
        }
        if action == GitAction::Merge {
            return capabilities.guarded_merge != crate::remote::SupportLevel::Unsupported;
        }
        true
    }

    fn git_choice(&self, action: GitAction, key: &str, label: &str) -> view::KeyChoice {
        if self.git_action_enabled(action) {
            view::KeyChoice::new(key, label)
        } else {
            view::KeyChoice::disabled(key, label)
        }
    }

    pub fn run(&mut self) -> Result<(), String> {
        if !stdin_is_tty() {
            return Err("TUI requires an interactive terminal".to_string());
        }

        crate::flight_recorder::mark_ui_thread();
        self.start_flight_recorder_servers();
        let shutdown = ShutdownNotification::install()?;
        let mut runtime = TerminalRuntime::enter()?;
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::process::with_cancellation(shutdown.cancellation(), || {
                self.run_inner(&mut runtime, &shutdown)
            })
        }));
        let result = self.finish_run(outcome, shutdown.signal());
        crate::flight_recorder::finish_pending_input_without_frame();
        crate::flight_recorder::end_idle("tui_exit");
        crate::flight_recorder::stop_all_servers();
        result
    }

    fn finish_run(
        &mut self,
        outcome: std::thread::Result<Result<ShutdownReason, String>>,
        signal: Option<ShutdownSignal>,
    ) -> Result<(), String> {
        let shutdown_reason = match &outcome {
            Err(_) => ShutdownReason::Panic,
            _ if signal == Some(ShutdownSignal::Sigint) => ShutdownReason::Sigint,
            _ if signal == Some(ShutdownSignal::Sigterm) => ShutdownReason::Sigterm,
            Ok(Ok(reason)) => *reason,
            Ok(Err(_)) => ShutdownReason::RunError,
        };
        let cleanup = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.cleanup_tui_jobs(shutdown_reason)
        }))
        .unwrap_or_else(|payload| {
            Err(format!(
                "TUI cleanup panicked: {}",
                panic_payload_message(payload.as_ref())
            ))
        });
        match outcome {
            Ok(_) if signal.is_some() => cleanup,
            Ok(Ok(_)) => cleanup,
            Ok(Err(error)) => {
                if let Err(cleanup_error) = cleanup {
                    self.record_tui_cleanup_failure(&cleanup_error);
                }
                Err(error)
            }
            Err(payload) => {
                if let Err(cleanup_error) = cleanup {
                    self.record_tui_cleanup_failure(&cleanup_error);
                }
                std::panic::resume_unwind(payload)
            }
        }
    }

    fn start_flight_recorder_servers(&mut self) {
        let server = crate::flight_recorder::serve_repositories(
            self.repos.iter().map(|managed| &managed.repo),
        );
        if !server.is_empty() {
            self.flight_recorder_servers.push(server);
        }
    }

    fn run_inner(
        &mut self,
        runtime: &mut TerminalRuntime,
        shutdown: &ShutdownNotification,
    ) -> Result<ShutdownReason, String> {
        crate::worker::ensure_running()?;
        self.offer_interrupted_run_recovery(runtime)?;
        self.refresh_sessions_after_tmux()?;
        self.poll_tmux_portal();
        self.draw(runtime)?;
        if self.repos.is_empty() {
            match self.add_repository(runtime) {
                Ok(()) => {}
                Err(error) => self.show_error("add repository failed", &error)?,
            }
        }
        let mut key_input = KeyInput::default();
        let mut pending_g = false;
        let shutdown_reason = loop {
            if let Some(reason) = requested_shutdown(shutdown) {
                break reason;
            }
            if self.tick_tui_action_jobs().any() {
                self.draw(runtime)?;
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
                        self.draw(runtime)?;
                    } else {
                        crate::flight_recorder::finish_pending_input_without_frame();
                    }
                    continue;
                }
                RuntimeEvent::Resize => {
                    self.draw(runtime)?;
                    continue;
                }
                RuntimeEvent::FocusGained => {
                    self.start_default_branch_status_poll(true);
                    self.poll_pull_requests(true);
                    self.draw(runtime)?;
                    continue;
                }
                RuntimeEvent::FocusLost => {
                    crate::flight_recorder::finish_pending_input_without_frame();
                    continue;
                }
            };
            let Some(key) = key else {
                crate::flight_recorder::finish_pending_input_without_frame();
                continue;
            };

            let mut should_quit = false;
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
                    if self.open_selected_comment_dialog(runtime)? {
                        self.draw(runtime)?;
                        continue;
                    }
                    match self.open_tmux_session_target() {
                        OpenTmuxSessionTarget::RepoDefaultAgent(index) => {
                            self.enter_agent_mode_for_index(runtime, index)?
                        }
                        OpenTmuxSessionTarget::PlanPhaseAgent => {
                            if let Err(error) = self.open_current_plan_tmux_session(runtime) {
                                self.show_error("plan phase tmux failed", &error)?;
                            }
                        }
                        OpenTmuxSessionTarget::WorktreeAgent => self.enter_agent_mode(runtime)?,
                        OpenTmuxSessionTarget::RepoPr => {
                            self.open_selected_repo_pr_agent(runtime)?
                        }
                        OpenTmuxSessionTarget::Blocked(message) => self.show_message(message)?,
                    }
                }
                Key::LazyGit => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if self.git_action_enabled(GitAction::LazyGit) {
                        if self.focused_panel == PanelFocus::Repos {
                            if let Err(error) = self.open_selected_repo_lazygit(runtime) {
                                self.show_error("repository lazygit failed", &error)?;
                            }
                        } else if let Err(error) =
                            self.open_tmux_window(runtime, TmuxWindow::LazyGit)
                        {
                            self.show_error("lazygit failed", &error)?;
                        }
                    }
                }
                Key::AutoFlow => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if self.focused_panel == PanelFocus::Status {
                    } else if self.focused_panel != PanelFocus::Worktrees {
                        self.show_message("focus worktrees to start or focus Auto Flow")?;
                    } else if let Err(error) = self.start_or_focus_selected_auto_run(runtime) {
                        self.show_error("auto flow failed", &error)?;
                    }
                }
                Key::OpenPr => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if self.git_action_enabled(GitAction::OpenPr)
                        && let Err(error) = self.open_selected_pr(runtime)
                    {
                        self.show_error("open PR failed", &error)?;
                    }
                }
                Key::SubmitReview => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if self.git_action_enabled(GitAction::SubmitReview)
                        && let Err(error) = self.submit_selected_repo_pr_review(runtime)
                    {
                        self.show_error("submit review failed", &error)?;
                    }
                }
                Key::Terminal => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if self.focused_panel == PanelFocus::Status {
                        self.show_message("focus repos or worktrees to open a terminal")?;
                    } else if self.focused_panel == PanelFocus::Repos {
                        if let Err(error) = self.open_selected_repo_terminal(runtime) {
                            self.show_error("repository terminal failed", &error)?;
                        }
                    } else if let Err(error) = self.open_tmux_window(runtime, TmuxWindow::Terminal)
                    {
                        self.show_error("terminal failed", &error)?;
                    }
                }
                Key::PlanActions => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if let Err(error) = self.show_plan_actions_dialog(runtime) {
                        self.show_error("plan actions failed", &error)?;
                    }
                }
                Key::Help => {
                    self.clear_leader_hint();
                    pending_g = false;
                    self.show_keybindings_dialog(runtime)?;
                }
                Key::Refresh => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if self.focused_panel == PanelFocus::Repos && !self.main_focused {
                        if let Err(error) = self.reorder_repositories(runtime) {
                            self.show_error("reorder repositories failed", &error)?;
                        }
                    } else {
                        self.refresh_sessions_after_tmux()?;
                    }
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
                    if self.git_action_enabled(GitAction::ReviewFix)
                        && let Err(error) = self.start_review_fix(runtime)
                    {
                        self.show_error("review fix failed", &error)?;
                    }
                }
                Key::CiFix => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if self.git_action_enabled(GitAction::CiFix)
                        && let Err(error) = self.start_ci_fix(runtime)
                    {
                        self.show_error("CI failure prompt failed", &error)?;
                    }
                }
                Key::ResolveAllComments => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if self.git_action_enabled(GitAction::ResolveAllComments)
                        && let Err(error) = self.resolve_review_comments(runtime)
                    {
                        self.show_error("resolve review comments failed", &error)?;
                    }
                }
                Key::Push => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if self.git_action_enabled(GitAction::Push)
                        && let Err(error) = self.push_selected_branch(runtime)
                    {
                        self.show_error("push failed", &error)?;
                    }
                }
                Key::Merge => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if self.git_action_enabled(GitAction::Merge)
                        && let Err(error) = self.merge_selected_pr(runtime)
                    {
                        self.show_error("merge failed", &error)?;
                    }
                }
                Key::PullDefault => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if self.focused_panel != PanelFocus::Repos {
                        self.show_message("focus repos to pull the default branch")?;
                    } else if let Err(error) = self.pull_default_branch(runtime) {
                        self.show_error("pull failed", &error)?;
                    }
                }
                Key::PlanMode => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if self.focused_panel == PanelFocus::Status {
                    } else if self.focused_panel != PanelFocus::Worktrees {
                        self.show_message("focus worktrees to run plan mode")?;
                    } else if let Err(error) = self.start_selected_worktree_plan_run(runtime) {
                        self.show_error("plan mode failed", &error)?;
                    }
                }
                Key::Create => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if self.focused_panel != PanelFocus::Repos {
                        self.show_message("focus repos to create a worktree session")?;
                    } else {
                        match self.create_session(runtime) {
                            Ok(true) => self.focus_worktrees(),
                            Ok(false) => {}
                            Err(error) => self.show_error("create session failed", &error)?,
                        }
                    }
                }
                Key::MigrateHarness => {
                    if self.focused_panel != PanelFocus::Worktrees {
                        self.show_message("focus worktrees to migrate an agent harness")?;
                    } else if let Some(index) = self.selected_worktree_index() {
                        self.migrate_worktree_harness(index)?;
                    }
                }
                Key::AbortOpencode => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if self.focused_panel != PanelFocus::Worktrees {
                        self.show_message("focus worktrees to abort an agent session")?;
                    } else if let Err(error) = self.abort_selected_opencode_session(runtime) {
                        self.show_error("abort failed", &error)?;
                    }
                }
                Key::ManageRepos => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if let Err(error) = self.edit_repositories(runtime) {
                        self.show_error("edit repositories failed", &error)?;
                    }
                }
                Key::OpenRemotePrs => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if self.focused_panel != PanelFocus::Repos {
                        self.show_message("focus repos to open a remote PR worktree")?;
                    } else if let Err(error) = self.open_remote_pr_worktree(runtime) {
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
                        self.show_message("repository removal is available from r")?;
                    } else if let Err(error) = self.archive_session(runtime) {
                        self.show_error("archive failed", &error)?;
                    }
                }
                Key::Unarchive => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if self.focused_panel != PanelFocus::Repos {
                        self.show_message("focus repos to unarchive a worktree")?;
                    } else if let Err(error) = self.unarchive_session(runtime) {
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
                    } else if let Err(error) = self.delete_session(runtime) {
                        self.show_error("delete failed", &error)?;
                    }
                }
                Key::EditWorktreeColumns => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if let Err(error) = self.edit_worktree_columns(runtime) {
                        self.show_error("edit worktree columns failed", &error)?;
                    }
                }
                Key::EditConfig => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if let Err(error) = self.edit_config(runtime) {
                        self.show_error("edit config failed", &error)?;
                    }
                }
                Key::EditUserConfig => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if let Err(error) = self.edit_user_config(runtime) {
                        self.show_error("edit user config failed", &error)?;
                    }
                }
                Key::SelectHarness => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if let Err(error) = self.select_default_harness(runtime) {
                        self.show_error("select harness failed", &error)?;
                    }
                }
                Key::Search => {
                    self.clear_leader_hint();
                    pending_g = false;
                    self.search_sessions(runtime)?;
                }
                Key::Other => {
                    self.clear_leader_hint();
                    pending_g = false;
                }
            }
            if should_quit {
                crate::flight_recorder::finish_pending_input_without_frame();
                break ShutdownReason::UserQuit;
            }
            self.draw(runtime)?;
        };
        Ok(shutdown_reason)
    }

    fn tick_tui_action_jobs(&mut self) -> TuiBackgroundChanges {
        let started = Instant::now();
        self.tui_tick_active = true;
        let routed = self.route_tui_job_messages();
        let changes = TuiBackgroundChanges {
            sessions: self.poll_session_refresh(),
            tmux: self.poll_tmux_agent_warmup(),
            tmux_portal: self.poll_tmux_portal(),
            worktree_columns: self.poll_wt_columns(),
            default_branch: self.poll_default_branch_status(),
            opencode_status: self.poll_opencode_status(),
            opencode_events: self.poll_opencode_events(),
            workflows: self.poll_workflow_runs(),
            dashboard_output: self.poll_dashboard_outputs(),
            pull_requests: self.poll_pull_requests(false),
            delete_sessions: self.poll_delete_sessions(),
            status_message: self.expire_status_message(),
        };
        self.start_default_branch_status_poll(false);
        self.start_opencode_status_poll(false);
        self.start_opencode_event_listeners();
        self.poll_workflow_maintenance();
        self.start_agent_state_persistence_jobs();
        self.tui_tick_active = false;
        crate::flight_recorder::record(
            "tui",
            "tick",
            Some(started.elapsed()),
            vec![
                crate::flight_recorder::unsigned("routed_jobs", routed),
                crate::flight_recorder::boolean("changed", changes.any()),
                crate::flight_recorder::unsigned(
                    "idle_us",
                    crate::flight_recorder::idle_for()
                        .unwrap_or_default()
                        .as_micros(),
                ),
            ],
        );
        changes
    }

    fn confirm_quit(&mut self) -> Result<bool, String> {
        if !self.delete_sessions_in_flight.is_empty() {
            self.show_message("delete in progress; wait for it to finish before quitting")?;
            return Ok(false);
        }
        Ok(true)
    }

    pub(crate) fn request_workflow_maintenance(&mut self) {
        self.workflow_maintenance_due = true;
    }

    fn poll_workflow_maintenance(&mut self) {
        while self.workflow_maintenance_rx.try_recv().is_ok() {}
        let due = self.workflow_maintenance_due
            || self.workflow_maintenance_last_started.elapsed() >= WORKFLOW_MAINTENANCE_INTERVAL;
        if self.workflow_maintenance_in_flight || !due {
            return;
        }
        let repos = self
            .repos
            .iter()
            .map(|managed| managed.repo.clone())
            .collect::<Vec<_>>();
        self.workflow_maintenance_in_flight = true;
        self.workflow_maintenance_due = false;
        self.workflow_maintenance_last_started = Instant::now();
        self.spawn_tui_job(
            TuiJobKind::WorkflowMaintenance,
            TuiJobKey::None,
            0,
            Some(TUI_ACTION_JOB_TIMEOUT),
            "prism-tui-maintenance".to_string(),
            move |_| {
                for repo in &repos {
                    if let Err(error) = maintain_workflow_storage(repo) {
                        let _ = crate::observability::append_runtime_message(
                            repo,
                            &format!("workflow maintenance failed: {error}"),
                        );
                    }
                }
                Ok(Some(TuiJobPayload::WorkflowMaintenance))
            },
        );
    }

    pub(crate) fn spawn_tui_job<F>(
        &mut self,
        kind: TuiJobKind,
        key: TuiJobKey,
        generation: u64,
        timeout: Option<Duration>,
        name: String,
        job: F,
    ) where
        F: FnOnce(
                JobContext<TuiJobKind, TuiJobKey, TuiJobPayload>,
            ) -> Result<Option<TuiJobPayload>, String>
            + Send
            + 'static,
    {
        if kind == TuiJobKind::DeleteSession {
            let label = kind.label();
            self.jobs.spawn_reliable_diagnostic(
                kind,
                key,
                generation,
                name,
                crate::tui_jobs::JobDiagnostic {
                    timeout,
                    kind: label,
                },
                job,
            );
        } else {
            let label = kind.label();
            self.jobs.spawn_diagnostic(
                kind,
                key,
                generation,
                name,
                crate::tui_jobs::JobDiagnostic {
                    timeout,
                    kind: label,
                },
                job,
            );
        }
    }

    pub(crate) fn route_tui_job_messages(&mut self) -> usize {
        if self.routing_tui_jobs {
            return 0;
        }
        self.routing_tui_jobs = true;
        let deadline = Instant::now() + TUI_TICK_TIME_BUDGET;
        let processed = self.route_tui_job_messages_with_budget(TUI_TICK_ITEM_BUDGET, deadline);
        self.routing_tui_jobs = false;
        processed
    }

    fn route_tui_job_messages_with_budget(&mut self, limit: usize, deadline: Instant) -> usize {
        for metadata in self.jobs.active_metadata() {
            if !self.job_generation_is_current(&metadata) {
                self.jobs.cancel(metadata.id);
            }
        }
        let mut processed = 0;
        let mut restart_session_refresh = false;
        while processed < limit && (processed == 0 || Instant::now() < deadline) {
            let Some(message) = self.jobs.drain_terminals(1).into_iter().next() else {
                break;
            };
            let JobMessage::Terminal { metadata, outcome } = message else {
                unreachable!();
            };
            processed += 1;
            self.clear_tui_job_in_flight(&metadata);
            self.record_tui_job_terminal(&metadata, &outcome);
            match outcome {
                JobOutcome::Completed | JobOutcome::Canceled => {}
                JobOutcome::Failed(_) | JobOutcome::SpawnFailed(_) => {
                    self.recover_failed_tui_job(&metadata);
                }
                JobOutcome::Panicked(_) => {
                    self.recover_failed_tui_job(&metadata);
                }
                JobOutcome::DeadlineExceeded => {
                    self.recover_failed_tui_job(&metadata);
                }
            }
            if metadata.kind == TuiJobKind::SessionRefresh && self.session_refresh_pending {
                restart_session_refresh = true;
                self.session_refresh_pending = false;
            }
        }

        let selected = self.selected_worktree_context().and_then(|context| {
            let session = self.sessions.get(context.session_index)?;
            let repo = self.repos.get(session.repo_index)?;
            Some(session.identity_key(&repo.identity))
        });
        let priority = |metadata: &JobMetadata<TuiJobKind, TuiJobKey>| {
            if metadata.kind == TuiJobKind::DeleteSession {
                return 0;
            }
            let selected_job = selected
                .as_ref()
                .is_some_and(|selected| match &metadata.key {
                    TuiJobKey::Worktree(key) => key == selected,
                    TuiJobKey::AgentStatePersistence(key) => key == selected,
                    TuiJobKey::Tmux(key) => &key.slot.worktree == selected,
                    TuiJobKey::Pr(key) => &key.worktree == selected,
                    TuiJobKey::PrPersistence(key) => &key.worktree == selected,
                    TuiJobKey::Opencode(key) => &key.worktree == selected,
                    TuiJobKey::OpencodeListener(stream) => &stream.worktree == selected,
                    TuiJobKey::None
                    | TuiJobKey::Repository(_)
                    | TuiJobKey::WorkflowRepository(_)
                    | TuiJobKey::DashboardOutput(_)
                    | TuiJobKey::Delete(_) => false,
                });
            if selected_job { 1 } else { 3 }
        };

        while processed < limit
            && Instant::now() < deadline
            && self
                .jobs
                .latest_min_priority(priority)
                .is_some_and(|value| value <= 1)
        {
            let Some(JobMessage::Payload { metadata, payload }) =
                self.jobs.take_latest_by(priority)
            else {
                break;
            };
            processed += 1;
            if self.job_generation_is_current(&metadata) {
                self.route_tui_job_payload(payload);
            }
        }

        while processed < limit && Instant::now() < deadline {
            let Some(message) = self.jobs.take_stream_event() else {
                break;
            };
            let JobMessage::Payload { metadata, payload } = message else {
                unreachable!();
            };
            processed += 1;
            if self.job_generation_is_current(&metadata)
                && let TuiJobPayload::OpencodeEvent(result) = payload
            {
                self.opencode_events_changed |= self.apply_opencode_event_result(result);
            }
        }

        while processed < limit && Instant::now() < deadline {
            let Some(JobMessage::Payload { metadata, payload }) =
                self.jobs.take_latest_by(priority)
            else {
                break;
            };
            processed += 1;
            if self.job_generation_is_current(&metadata) {
                self.route_tui_job_payload(payload);
            }
        }

        for metadata in self.jobs.take_dirty_jobs() {
            if self.job_generation_is_current(&metadata)
                && let TuiJobKey::OpencodeListener(stream) = metadata.key
            {
                self.request_opencode_reconciliation_for(stream.worktree);
            }
        }
        let stats = self.jobs.queue_stats();
        let idle = crate::flight_recorder::idle_for();
        crate::flight_recorder::record(
            "queue",
            "snapshot",
            None,
            vec![
                crate::flight_recorder::unsigned("processed", processed),
                crate::flight_recorder::unsigned("event_depth", stats.event_depth),
                crate::flight_recorder::unsigned("event_capacity", stats.event_capacity),
                crate::flight_recorder::unsigned("coalesced_depth", stats.coalesced_depth),
                crate::flight_recorder::unsigned("latest_depth", stats.latest_depth),
                crate::flight_recorder::unsigned("terminal_depth", stats.terminal_depth),
                crate::flight_recorder::unsigned("overflow_delta", stats.overflow_delta),
                crate::flight_recorder::unsigned("overflow_total", stats.overflow_total),
                crate::flight_recorder::unsigned("coalesced_delta", stats.coalesced_delta),
                crate::flight_recorder::unsigned("coalesced_total", stats.coalesced_total),
                crate::flight_recorder::boolean("dirty", stats.dirty),
                crate::flight_recorder::unsigned("idle_us", idle.unwrap_or_default().as_micros()),
            ],
        );
        if processed > 0
            && let Some(idle) = idle
        {
            crate::flight_recorder::record(
                "jobs",
                "completion_burst_after_idle",
                Some(idle),
                vec![crate::flight_recorder::unsigned("processed", processed)],
            );
        }
        if stats.overflow_delta > 0 || stats.coalesced_delta > 0 {
            self.record_tui_queue_stats(stats);
        }
        if restart_session_refresh && !self.scheduling_stopped {
            let _ = self.refresh_sessions_after_tmux();
        }
        processed
    }

    fn route_tui_job_payload(&self, payload: TuiJobPayload) {
        match payload {
            TuiJobPayload::SessionRefresh(result) => {
                let _ = self.session_refresh_tx.send(result);
            }
            TuiJobPayload::WorkflowMaintenance => {
                let _ = self.workflow_maintenance_tx.send(());
            }
            TuiJobPayload::PrPoll(result) => {
                let _ = self.pr_poll_tx.send(result);
            }
            TuiJobPayload::WorkflowPoll(result) => {
                let _ = self.workflow_poll_tx.send(result);
            }
            TuiJobPayload::DashboardOutput(result) => {
                let _ = self.dashboard_output_tx.send(result);
            }
            TuiJobPayload::DeleteSession(result) => {
                let _ = self.delete_session_tx.send(result);
            }
            TuiJobPayload::TmuxWarmup(result) => {
                let _ = self.tmux_warmup_tx.send(result);
            }
            TuiJobPayload::TmuxPortal(result) => {
                let _ = self.tmux_portal_tx.send(result);
            }
            TuiJobPayload::WorktreeColumns(result) => {
                let _ = self.wt_poll_tx.send(result);
            }
            TuiJobPayload::DefaultBranch(result) => {
                let _ = self.default_branch_poll_tx.send(result);
            }
            TuiJobPayload::OpencodePoll(result) => {
                let _ = self.opencode_poll_tx.send(result);
            }
            TuiJobPayload::OpencodeEvent(result) => {
                #[cfg(test)]
                let _ = self.opencode_event_tx.send(result);
                #[cfg(not(test))]
                let _ = result;
            }
        }
    }

    fn clear_tui_job_in_flight(&mut self, metadata: &JobMetadata<TuiJobKind, TuiJobKey>) {
        match (&metadata.kind, &metadata.key) {
            (TuiJobKind::SessionRefresh, _) => self.session_refresh_in_flight = false,
            (TuiJobKind::AgentStatePersistence, TuiJobKey::AgentStatePersistence(key)) => {
                self.agent_state_persistence_in_flight.remove(key);
            }
            (TuiJobKind::WorkflowMaintenance, _) => self.workflow_maintenance_in_flight = false,
            (TuiJobKind::PrSummary, TuiJobKey::Repository(repository)) => {
                if let Some(repo) = self
                    .repos
                    .iter_mut()
                    .find(|repo| &repo.identity == repository)
                {
                    repo.pr_summary_poll_in_flight = false;
                }
            }
            (TuiJobKind::PrDetails, TuiJobKey::Pr(key)) => {
                self.pr_polls_in_flight.remove(key);
            }
            (TuiJobKind::PrPersistence, TuiJobKey::PrPersistence(key)) => {
                self.pr_persistence_in_flight.remove(key);
            }
            (TuiJobKind::WorkflowPoll, TuiJobKey::WorkflowRepository(repository)) => {
                self.workflow_polls_in_flight.remove(repository);
            }
            (TuiJobKind::DashboardOutput, TuiJobKey::DashboardOutput(key)) => {
                self.dashboard_outputs_in_flight.remove(key);
            }
            (TuiJobKind::DeleteSession, TuiJobKey::Delete(key)) => {
                self.delete_sessions_in_flight.remove(key);
            }
            (TuiJobKind::TmuxWarmup, TuiJobKey::Tmux(key)) => {
                self.tmux_warmups_in_flight.remove(key);
            }
            (TuiJobKind::TmuxPortal, TuiJobKey::Tmux(key)) => {
                self.tmux_portal_polls_in_flight.remove(key);
            }
            (TuiJobKind::WorktreeColumns, TuiJobKey::Repository(repository)) => {
                if let Some(repo) = self
                    .repos
                    .iter_mut()
                    .find(|repo| &repo.identity == repository)
                {
                    repo.wt_poll_in_flight = false;
                }
            }
            (TuiJobKind::DefaultBranch, TuiJobKey::Worktree(key)) => {
                if let Some(repo) = self
                    .repos
                    .iter_mut()
                    .find(|repo| repo.identity == key.repository)
                {
                    repo.default_branch_poll_in_flight = false;
                }
            }
            (TuiJobKind::OpencodePoll, TuiJobKey::Opencode(key)) => {
                self.opencode_polls_in_flight.remove(key);
            }
            (TuiJobKind::OpencodeListener, TuiJobKey::OpencodeListener(stream)) => {
                self.opencode_listeners.remove(stream);
            }
            _ => {}
        }
    }

    fn job_generation_is_current(&self, metadata: &JobMetadata<TuiJobKind, TuiJobKey>) -> bool {
        match &metadata.key {
            TuiJobKey::None => {
                metadata.kind == TuiJobKind::WorkflowMaintenance
                    || metadata.generation == self.session_inventory_generation
            }
            TuiJobKey::Repository(_) => metadata.generation == self.session_inventory_generation,
            TuiJobKey::WorkflowRepository(_) | TuiJobKey::DashboardOutput(_) => {
                metadata.generation == self.workflow_revision
            }
            TuiJobKey::Worktree(key) => {
                self.worktree_generation_is_current(key, metadata.generation)
            }
            TuiJobKey::AgentStatePersistence(key) => {
                self.agent_state_persistence_in_flight.contains(key)
            }
            TuiJobKey::Pr(key) => {
                key.generation == metadata.generation
                    && self.worktree_generation_is_current(&key.worktree, metadata.generation)
            }
            TuiJobKey::PrPersistence(key) => {
                key.generation == metadata.generation
                    && self.pr_persistence_versions.contains_key(key)
            }
            TuiJobKey::Delete(key) => {
                key.generation == metadata.generation
                    && self.worktree_generation_is_current(&key.worktree, metadata.generation)
            }
            TuiJobKey::Tmux(key) => {
                key.generation == metadata.generation
                    && crate::agent_session::key_is_current(&self.tmux_generations, key)
            }
            TuiJobKey::Opencode(key) => {
                key.generation == metadata.generation
                    && self.worktree_generation_is_current(&key.worktree, metadata.generation)
            }
            TuiJobKey::OpencodeListener(stream) => self
                .worktree_generations
                .get(&stream.worktree)
                .is_some_and(|generation| {
                    *generation == metadata.generation
                        && stream.generation == metadata.generation
                        && self.sessions.iter().enumerate().any(|(index, session)| {
                            self.visible_session_indices().contains(&index)
                                && self.repos.get(session.repo_index).is_some_and(|managed| {
                                    session.identity_key(&managed.identity) == stream.worktree
                                })
                                && session.opencode_status.as_ref().is_some_and(|status| {
                                    status.server_url.as_deref() == Some(stream.server_url.as_str())
                                        && status.session_id.as_deref()
                                            == Some(stream.session_id.as_str())
                                })
                        })
                }),
        }
    }

    fn worktree_generation_is_current(
        &self,
        worktree: &WorktreeSessionKey,
        generation: u64,
    ) -> bool {
        self.worktree_generations.get(worktree).copied() == Some(generation)
    }

    pub(crate) fn queue_agent_state_persistence(&mut self, session_index: usize) {
        let Some(session) = self.sessions.get(session_index) else {
            return;
        };
        let Some(managed) = self.repos.get(session.repo_index) else {
            return;
        };
        let worktree = session.identity_key(&managed.identity);
        let generation = self
            .worktree_generations
            .get(&worktree)
            .copied()
            .unwrap_or_default();
        self.agent_state_persistence_pending.insert(
            worktree.clone(),
            AgentStatePersistenceRequest {
                generation,
                repo: managed.repo.clone(),
                branch: session.branch.clone(),
                state: Some(session.agent_state),
            },
        );
        self.start_agent_state_persistence_jobs();
    }

    pub(crate) fn queue_agent_state_removal(&mut self, session_index: usize) {
        let Some(session) = self.sessions.get(session_index) else {
            return;
        };
        let Some(managed) = self.repos.get(session.repo_index) else {
            return;
        };
        let worktree = session.identity_key(&managed.identity);
        let generation = self
            .worktree_generations
            .get(&worktree)
            .copied()
            .unwrap_or_default();
        self.agent_state_persistence_pending.insert(
            worktree,
            AgentStatePersistenceRequest {
                generation,
                repo: managed.repo.clone(),
                branch: session.branch.clone(),
                state: None,
            },
        );
        self.start_agent_state_persistence_jobs();
    }

    pub(crate) fn retain_agent_state_persistence_for(
        &mut self,
        live: &BTreeSet<WorktreeSessionKey>,
    ) {
        self.agent_state_persistence_pending
            .retain(|key, request| live.contains(key) || request.state.is_none());
    }

    fn start_agent_state_persistence_jobs(&mut self) {
        let keys = self
            .agent_state_persistence_pending
            .keys()
            .filter(|key| !self.agent_state_persistence_in_flight.contains(*key))
            .cloned()
            .collect::<Vec<_>>();
        for key in keys {
            let Some(request) = self.agent_state_persistence_pending.remove(&key) else {
                continue;
            };
            self.agent_state_persistence_in_flight.insert(key.clone());
            self.spawn_tui_job(
                TuiJobKind::AgentStatePersistence,
                TuiJobKey::AgentStatePersistence(key),
                request.generation,
                Some(TUI_ACTION_JOB_TIMEOUT),
                "prism-agent-state-persistence".to_string(),
                move |_| {
                    match request.state {
                        Some(state) => {
                            crate::session::save_agent_state(&request.repo, &request.branch, state)?
                        }
                        None => crate::observability::with_writable_db(&request.repo, |conn| {
                            crate::agent_session::remove_state_with_conn(conn, &request.branch)
                        })?,
                    }
                    Ok(None)
                },
            );
        }
    }

    fn record_tui_job_terminal(
        &self,
        metadata: &JobMetadata<TuiJobKind, TuiJobKey>,
        outcome: &JobOutcome,
    ) {
        let outcome_kind = outcome.kind();
        let error = outcome.error_message();
        let key = format!("{:?}", metadata.key);
        let deadline_ms = metadata.deadline.map(|deadline| {
            deadline
                .saturating_duration_since(metadata.started_at)
                .as_millis() as i64
        });
        crate::observability::emit_deferred(crate::observability::EventInput {
            level: match outcome_kind {
                crate::tui_jobs::JobOutcomeKind::Failed
                | crate::tui_jobs::JobOutcomeKind::SpawnFailed
                | crate::tui_jobs::JobOutcomeKind::Panicked
                | crate::tui_jobs::JobOutcomeKind::DeadlineExceeded => {
                    crate::observability::LogLevel::Error
                }
                crate::tui_jobs::JobOutcomeKind::Completed
                | crate::tui_jobs::JobOutcomeKind::Canceled => {
                    crate::observability::LogLevel::Debug
                }
            },
            target: "tui_job",
            action: "terminal",
            operation_id: None,
            parent_operation_id: None,
            branch: None,
            session: None,
            message: format!(
                "TUI job {} #{} finished with {}",
                metadata.kind.label(),
                metadata.id,
                outcome_kind.label()
            ),
            data_json: Some(crate::observability::job_data_json(
                crate::observability::JobObservation {
                    job_id: metadata.id,
                    kind: metadata.kind.label(),
                    key: &key,
                    generation: metadata.generation,
                    outcome: outcome_kind.label(),
                    elapsed_ms: metadata.started_at.elapsed().as_millis() as i64,
                    deadline_ms,
                    error: error.as_deref(),
                },
            )),
        });
    }

    fn record_tui_queue_stats(&self, stats: crate::tui_jobs::QueueStats) {
        crate::observability::emit_deferred(crate::observability::EventInput {
            level: if stats.overflow_delta > 0 {
                crate::observability::LogLevel::Warn
            } else {
                crate::observability::LogLevel::Debug
            },
            target: "tui",
            action: "queue_pressure",
            operation_id: None,
            parent_operation_id: None,
            branch: None,
            session: None,
            message: format!(
                "TUI delivery pressure: {} overflowed, {} coalesced",
                stats.overflow_delta, stats.coalesced_delta
            ),
            data_json: Some(format!(
                "{{\"event_depth\":{},\"event_capacity\":{},\"coalesced_depth\":{},\"coalesced_capacity\":{},\"latest_depth\":{},\"terminal_depth\":{},\"overflow_count\":{},\"overflow_total\":{},\"coalesced_count\":{},\"coalesced_total\":{},\"stream_dirty\":{}}}",
                stats.event_depth,
                stats.event_capacity,
                stats.coalesced_depth,
                stats.coalesced_capacity,
                stats.latest_depth,
                stats.terminal_depth,
                stats.overflow_delta,
                stats.overflow_total,
                stats.coalesced_delta,
                stats.coalesced_total,
                stats.dirty,
            )),
        });
    }

    fn recover_failed_tui_job(&mut self, metadata: &JobMetadata<TuiJobKind, TuiJobKey>) {
        if metadata.kind == TuiJobKind::SessionRefresh {
            self.session_refresh_pending = true;
        }
        if let (TuiJobKind::DeleteSession, TuiJobKey::Delete(key)) = (&metadata.kind, &metadata.key)
            && let Some(session) = self.sessions.iter_mut().find(|session| {
                self.repos
                    .get(session.repo_index)
                    .is_some_and(|repo| session.identity_key(&repo.identity) == key.worktree)
            })
        {
            session.hidden = false;
            self.ensure_navigation_valid();
        }
    }

    fn cleanup_tui_jobs(&mut self, reason: ShutdownReason) -> Result<(), String> {
        let mut errors = Vec::new();
        let started = Instant::now();
        while (!self.pr_persistence_in_flight.is_empty()
            || !self.pr_persistence_pending.is_empty()
            || !self.agent_state_persistence_in_flight.is_empty()
            || !self.agent_state_persistence_pending.is_empty())
            && started.elapsed() < TUI_JOB_SHUTDOWN_GRACE
        {
            self.route_tui_job_messages();
            self.drain_pr_poll_results();
            self.start_agent_state_persistence_jobs();
            std::thread::sleep(Duration::from_millis(5));
        }
        let active_jobs = self.jobs.active_metadata().len();
        let shutdown_started = Instant::now();
        self.scheduling_stopped = true;
        self.jobs.stop_accepting();
        self.jobs.cancel_all();
        if let Err(error) = self.shutdown_owned_opencode_servers() {
            errors.push(error);
        }
        while self.jobs.has_jobs() && shutdown_started.elapsed() < TUI_JOB_SHUTDOWN_GRACE {
            self.route_tui_job_messages();
            if self.jobs.has_jobs() {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        self.route_tui_job_messages();
        let unfinished = self.jobs.abandon_unfinished();
        if unfinished > 0 {
            errors.push(format!(
                "detached {unfinished} uncooperative job(s) after shutdown grace period"
            ));
        }
        crate::observability::emit_deferred(crate::observability::EventInput {
            level: if errors.is_empty() {
                crate::observability::LogLevel::Info
            } else {
                crate::observability::LogLevel::Warn
            },
            target: "tui",
            action: "shutdown_cleanup",
            operation_id: None,
            parent_operation_id: None,
            branch: None,
            session: None,
            message: format!(
                "TUI shutdown cleanup finished: reason={}, active_jobs={}, unfinished_jobs={unfinished}",
                reason.label(),
                active_jobs
            ),
            data_json: Some(crate::observability::shutdown_data_json(
                reason.label(),
                active_jobs,
                unfinished,
                started.elapsed().as_millis() as i64,
            )),
        });
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    fn record_tui_cleanup_failure(&self, error: &str) {
        let message = format!("TUI cleanup failed: {error}");
        if crate::observability::append_runtime_message(&self.repo, &message).is_err() {
            eprintln!("prism: {message}");
        }
    }

    pub(crate) fn refresh_worktree_harness_configs(&mut self) {
        let live = self
            .sessions
            .iter()
            .filter_map(|session| {
                let managed = self.repos.get(session.repo_index)?;
                Some(session.identity_key(&managed.identity))
            })
            .collect::<BTreeSet<_>>();
        self.worktree_harness_configs
            .retain(|key, _| live.contains(key));
        for session in &self.sessions {
            let Some(managed) = self.repos.get(session.repo_index) else {
                continue;
            };
            let key = session.identity_key(&managed.identity);
            if self.worktree_harness_configs.contains_key(&key) {
                continue;
            }
            let config = crate::session::worktree_harness(&managed.repo, session)
                .ok()
                .and_then(|association| managed.config.for_harness(&association.harness_id).ok())
                .or_else(|| test_default_worktree_harness_config(&managed.config));
            let Some(config) = config else { continue };
            self.worktree_harness_configs.insert(key, config);
        }
    }

    pub(crate) fn reload_worktree_harness_config(&mut self, session_index: usize) {
        let Some(session) = self.sessions.get(session_index) else {
            return;
        };
        let Some(managed) = self.repos.get(session.repo_index) else {
            return;
        };
        let key = session.identity_key(&managed.identity);
        self.worktree_harness_configs.remove(&key);
        self.refresh_worktree_harness_configs();
        self.session_inventory_generation = self.session_inventory_generation.saturating_add(1);
        if self.session_refresh_in_flight {
            self.session_refresh_pending = true;
        }
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

    pub(crate) fn enter_agent_mode_for_index(
        &mut self,
        runtime: &mut TerminalRuntime,
        index: usize,
    ) -> Result<(), String> {
        self.prepare_worktree_harness_for_open(runtime, index)?;
        let navigation = self.navigation_snapshot();
        let terminal_area = runtime.area()?;
        self.prepare_tmux_session_for_attach(
            index,
            (terminal_area.width, terminal_area.height.saturating_sub(1)),
        )?;
        let result = runtime.suspend_for(|| self.attach_tmux_session_for_index(index));
        let refresh_started = Instant::now();
        self.refresh_sessions_after_tmux()?;
        crate::flight_recorder::record(
            "attach",
            "post_resume_refresh",
            Some(refresh_started.elapsed()),
            Vec::new(),
        );
        self.restore_navigation_snapshot(navigation);
        self.start_tmux_agent_warmup();
        if let Err(error) = result {
            self.show_error("tmux session failed", &error)?;
        }
        Ok(())
    }

    fn prepare_worktree_harness_for_open(
        &mut self,
        runtime: &mut TerminalRuntime,
        index: usize,
    ) -> Result<(), String> {
        let Some(session) = self
            .sessions
            .get(index)
            .map(Session::background_job_snapshot)
        else {
            return Ok(());
        };
        let Some(managed) = self.repos.get(session.repo_index) else {
            return Ok(());
        };
        let repo = managed.repo.clone();
        let target = managed.config.default_harness.clone();
        let association = crate::session::worktree_harness(&repo, &session)?;
        if association.harness_id == target || association.keep {
            return Ok(());
        }
        let choices = view::ChoiceList {
            title: "Worktree Harness Changed".to_string(),
            choices: vec![
                view::KeyChoice::new("m", format!("Migrate to {target}")),
                view::KeyChoice::new(
                    "l",
                    format!("Later; open {} and ask next time", association.harness_id),
                ),
                view::KeyChoice::new("k", format!("Keep {}; stop asking", association.harness_id)),
            ],
        };
        match self.prompt_choice_dialog(runtime, choices)?.as_deref() {
            Some("m") => {
                let use_ = crate::agent_session::session_use(
                    &self.repos,
                    &mut self.tmux_generations,
                    &session,
                );
                self.finish_tmux_warmup_for_key(&use_.warmup_key);
                if let Some(managed) = self.repos.get(session.repo_index) {
                    crate::agent_session::retire_generation(
                        &repo,
                        &managed.config,
                        &session.branch,
                        use_.generation,
                    );
                }
                crate::session::set_worktree_harness(&repo, &session, &target, false)?;
                self.reload_worktree_harness_config(index);
                crate::agent_session::rotate_generation(
                    &self.repos,
                    &mut self.tmux_generations,
                    use_.slot,
                );
            }
            Some("k") => crate::session::set_worktree_harness(
                &repo,
                &session,
                &association.harness_id,
                true,
            )?,
            _ => {}
        }
        Ok(())
    }

    fn migrate_worktree_harness(&mut self, index: usize) -> Result<(), String> {
        let Some(session) = self
            .sessions
            .get(index)
            .map(Session::background_job_snapshot)
        else {
            return Ok(());
        };
        let Some(managed) = self.repos.get(session.repo_index) else {
            return Ok(());
        };
        let repo = managed.repo.clone();
        let target = managed.config.default_harness.clone();
        let repository_config = managed.config.clone();
        let association = crate::session::worktree_harness(&repo, &session)?;
        if association.harness_id == target && !association.keep {
            self.show_message(&format!("worktree already uses harness '{target}'"))?;
            return Ok(());
        }
        let use_ =
            crate::agent_session::session_use(&self.repos, &mut self.tmux_generations, &session);
        self.finish_tmux_warmup_for_key(&use_.warmup_key);
        crate::agent_session::retire_generation(
            &repo,
            &repository_config,
            &session.branch,
            use_.generation,
        );
        crate::session::set_worktree_harness(&repo, &session, &target, false)?;
        self.reload_worktree_harness_config(index);
        crate::agent_session::rotate_generation(&self.repos, &mut self.tmux_generations, use_.slot);
        self.show_message(&format!("migrated worktree to harness '{target}'"))?;
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
        let result = runtime.suspend_for(|| self.attach_selected_tmux_window(window));
        self.refresh_sessions_after_tmux()?;
        self.restore_navigation_snapshot(navigation);
        self.start_tmux_agent_warmup();
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
            "Space g R    main comments: resolve all inline review conversations",
            "r            repos: reorder or remove repositories",
            "R            edit repositories/order/keys/remove in repos.toml",
            "C            repos: open a worktree for a remote pull request",
            "c            repos: create worktree session in selected repo",
            "+ / -        worktrees: raise/lower visibility sort",
            "x            worktrees: abort selected agent session when supported",
            "M            worktrees: migrate selected worktree to the default harness",
            "H            choose the global default harness or add a generic harness",
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
            "r            refresh outside the repos sidebar",
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
        self.confirm_dialog(
            runtime,
            "Archive Session",
            lines,
            "Archive this session?",
            false,
        )
    }

    pub(crate) fn confirm_delete_dialog(
        &mut self,
        runtime: &mut TerminalRuntime,
        branch: &str,
        path: &str,
        warnings: &[String],
        default: bool,
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
        self.confirm_dialog(
            runtime,
            "Delete Session",
            lines,
            "Delete this session?",
            default,
        )
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
                    if selectable_choice_key(&choices, &normalized).is_some() {
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

    pub(crate) fn ordered_toggle_dialog(
        &mut self,
        runtime: &mut TerminalRuntime,
        title: &str,
        mut items: Vec<view::OrderedToggleItem>,
    ) -> Result<Option<Vec<String>>, String> {
        items.sort_by_key(|item| !item.enabled);
        let mut selected = 0usize;
        loop {
            self.dialog = Some(view::DialogModel::OrderedToggle {
                title: title.to_string(),
                items: items.clone(),
                selected,
                reorderable: true,
            });
            self.draw(runtime)?;
            if self.tick_tui_action_jobs().any() {
                self.draw(runtime)?;
            }
            let Some(event) = runtime.poll_event(Duration::from_millis(100))? else {
                continue;
            };
            let RuntimeEvent::Key(event) = event else {
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
                KeyCode::Enter if plain_key(event) => {
                    self.dialog = None;
                    self.draw(runtime)?;
                    return Ok(Some(
                        items
                            .iter()
                            .filter(|item| item.enabled)
                            .map(|item| item.id.clone())
                            .collect(),
                    ));
                }
                KeyCode::Up | KeyCode::Char('k') if plain_key(event) => {
                    selected = selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') if plain_key(event) => {
                    selected = selected
                        .saturating_add(1)
                        .min(items.len().saturating_sub(1));
                }
                KeyCode::Char(' ') if plain_key(event) => {
                    toggle_ordered_item(&mut items, &mut selected);
                }
                KeyCode::Char('K') if plain_key(event) => {
                    move_enabled_ordered_item(&mut items, &mut selected, -1);
                }
                KeyCode::Char('J') if plain_key(event) => {
                    move_enabled_ordered_item(&mut items, &mut selected, 1);
                }
                _ => {}
            }
        }
    }

    fn recovery_selection_dialog(
        &mut self,
        runtime: &mut TerminalRuntime,
        mut items: Vec<view::OrderedToggleItem>,
    ) -> Result<Option<Vec<String>>, String> {
        let mut selected = 0usize;
        loop {
            self.dialog = Some(view::DialogModel::OrderedToggle {
                title: "Restart interrupted work".to_string(),
                items: items.clone(),
                selected,
                reorderable: false,
            });
            self.draw(runtime)?;
            let Some(event) = runtime.poll_event(Duration::from_millis(100))? else {
                continue;
            };
            let RuntimeEvent::Key(event) = event else {
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
                    return Ok(None);
                }
                KeyCode::Enter if plain_key(event) => {
                    self.dialog = None;
                    return Ok(Some(
                        items
                            .iter()
                            .filter(|item| item.enabled)
                            .map(|item| item.id.clone())
                            .collect(),
                    ));
                }
                KeyCode::Up | KeyCode::Char('k') if plain_key(event) => {
                    selected = selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') if plain_key(event) => {
                    selected = selected
                        .saturating_add(1)
                        .min(items.len().saturating_sub(1));
                }
                KeyCode::Char(' ') if plain_key(event) => {
                    toggle_item_in_place(&mut items, selected);
                }
                _ => {}
            }
        }
    }

    fn offer_interrupted_run_recovery(
        &mut self,
        runtime: &mut TerminalRuntime,
    ) -> Result<(), String> {
        let mut candidates = Vec::new();
        for (repo_index, managed) in self.repos.iter().enumerate() {
            let repo_candidates = crate::observability::with_writable_db(&managed.repo, |conn| {
                crate::execution::recovery_candidates(conn)
            })?;
            candidates.extend(
                repo_candidates
                    .into_iter()
                    .map(|candidate| (repo_index, candidate)),
            );
        }
        if candidates.is_empty() {
            return Ok(());
        }
        let now = crate::execution::now_ms();
        let items = candidates
            .iter()
            .enumerate()
            .map(|(index, (repo_index, candidate))| {
                let repo = &self.repos[*repo_index];
                let age_ms = candidate
                    .last_heartbeat_unix_ms
                    .map(|heartbeat| now.saturating_sub(heartbeat))
                    .unwrap_or(0);
                let age = if age_ms >= 60_000 {
                    format!("{}m ago", age_ms / 60_000)
                } else {
                    format!("{}s ago", age_ms / 1_000)
                };
                let kind = match candidate.workflow.kind {
                    crate::execution::WorkflowKind::Auto => "Auto Flow",
                    crate::execution::WorkflowKind::Plan => "Plan",
                };
                let worktree = match candidate.workflow.kind {
                    crate::execution::WorkflowKind::Auto => candidate.branch.as_str(),
                    crate::execution::WorkflowKind::Plan => candidate
                        .worktree
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or_else(|| candidate.worktree.to_str().unwrap_or("worktree")),
                };
                view::OrderedToggleItem {
                    id: index.to_string(),
                    label: format!(
                        "{} / {}  {}  {}  {}",
                        repo.label, worktree, kind, candidate.active_step, age
                    ),
                    enabled: false,
                }
            })
            .collect();
        let Some(selected) = self.recovery_selection_dialog(runtime, items)? else {
            return Ok(());
        };
        let selected = selected.into_iter().collect::<BTreeSet<_>>();
        for (index, (repo_index, candidate)) in candidates.iter().enumerate() {
            if !selected.contains(&index.to_string()) {
                continue;
            }
            let managed = &self.repos[*repo_index];
            if crate::worker::legacy_worker_running(
                &managed.repo,
                &managed.config,
                &candidate.workflow,
            )? {
                return Err(format!(
                    "legacy {} worker for run {} is still active; try recovery after it exits",
                    candidate.workflow.kind.label(),
                    candidate.workflow.run_id
                ));
            }
        }
        for (repo_index, managed) in self.repos.iter().enumerate() {
            let decisions = candidates
                .iter()
                .enumerate()
                .filter(|(_, (candidate_repo, _))| *candidate_repo == repo_index)
                .map(|(index, (_, candidate))| {
                    (
                        candidate.workflow.clone(),
                        candidate.interruption_generation,
                        selected.contains(&index.to_string()),
                    )
                })
                .collect::<Vec<_>>();
            if !decisions.is_empty() {
                crate::observability::with_writable_db_mut(&managed.repo, |conn| {
                    crate::execution::apply_recovery_decision(conn, &decisions)
                })?;
            }
        }
        if !selected.is_empty() {
            crate::worker::wake()?;
        }
        Ok(())
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
        prompt: &str,
        default: bool,
    ) -> Result<bool, String> {
        let mut input = String::new();
        let mut invalid = false;
        self.dialog = Some(view::DialogModel::Confirm {
            title: title.to_string(),
            lines: lines.clone(),
            prompt: prompt.to_string(),
            input: input.clone(),
            default,
            invalid,
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
                KeyCode::Enter if plain_key(event) => {
                    if let Some(result) = confirmation_result(&input, default) {
                        self.dialog = None;
                        self.draw(runtime)?;
                        return Ok(result);
                    }
                    input.clear();
                    invalid = true;
                }
                KeyCode::Esc | KeyCode::Char('c')
                    if event.code == KeyCode::Esc || ctrl_key(event) =>
                {
                    self.dialog = None;
                    self.draw(runtime)?;
                    return Ok(default);
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(ch) if plain_key(event) && !ch.is_control() => {
                    input.push(ch);
                }
                _ => {}
            }
            self.dialog = Some(view::DialogModel::Confirm {
                title: title.to_string(),
                lines: lines.clone(),
                prompt: prompt.to_string(),
                input: input.clone(),
                default,
                invalid,
            });
            self.draw(runtime)?;
        }
    }

    pub(crate) fn confirm_action_dialog(
        &mut self,
        runtime: &mut TerminalRuntime,
        title: &str,
        message: &str,
        default: bool,
    ) -> Result<bool, String> {
        self.confirm_dialog(runtime, title, vec![], message, default)
    }

    pub(crate) fn notice_dialog(
        &mut self,
        runtime: &mut TerminalRuntime,
        title: &str,
        lines: Vec<view::DialogLine>,
    ) -> Result<(), String> {
        self.dialog = Some(view::DialogModel::Notice {
            title: title.to_string(),
            lines,
        });
        self.draw(runtime)?;
        loop {
            let Some(event) = runtime.poll_event(Duration::from_millis(100))? else {
                continue;
            };
            if matches!(event, RuntimeEvent::Key(event) if event.kind == KeyEventKind::Press) {
                self.dialog = None;
                self.draw(runtime)?;
                return Ok(());
            }
        }
    }

    pub(crate) fn show_message(&mut self, message: &str) -> Result<(), String> {
        self.status_message = Some(message.to_string());
        self.status_message_until = Some(Instant::now() + STATUS_MESSAGE_DURATION);
        let _ = crate::observability::append_runtime_message(&self.repo, message);
        Ok(())
    }

    fn show_error(&mut self, context: &str, error: &str) -> Result<(), String> {
        let message = format!("{context}: {error}");
        self.show_message(&message)
    }

    fn move_down(&mut self) {
        if self.main_focused {
            if self.move_repo_pr_selection(1) {
                return;
            }
            let moved_comment = self.move_comment_selection(1);
            self.main_scroll = self.main_scroll.saturating_add(1);
            if !moved_comment {
                self.move_plan_step_selection(1);
            }
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
            if self.move_repo_pr_selection(-1) {
                return;
            }
            let moved_comment = self.move_comment_selection(-1);
            self.main_scroll = self.main_scroll.saturating_sub(1);
            if !moved_comment {
                self.move_plan_step_selection(-1);
            }
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
                self.repo_main_view = view::RepoMainView::ChangeRequests;
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
        let selected = self.selected_worktree_index();
        self.worktree_list_mode = mode;
        self.persist_worktree_list_mode();
        if mode == WorktreeListMode::Repo {
            if let Some(index) = selected {
                self.select_worktree(index);
            } else {
                self.restore_selected_worktree_for_repo();
            }
        }
    }

    fn persist_worktree_list_mode(&self) {
        let Some(path) = self.ui_state_path.as_deref() else {
            return;
        };
        if let Err(error) = crate::ui_state::save_to_path(path, self.worktree_list_mode) {
            let _ = crate::observability::append_runtime_message(
                &self.repo,
                &format!("UI state save failed: {error}"),
            );
        }
    }

    fn focus_main(&mut self) {
        self.main_focused = true;
        self.ensure_selected_repo_pr();
    }

    fn open_tmux_session_target(&self) -> OpenTmuxSessionTarget {
        match self.focused_panel {
            PanelFocus::Status => OpenTmuxSessionTarget::Blocked("status has no Enter action"),
            PanelFocus::Repos => {
                if self.main_focused && self.selected_repo_pr_summary().is_some() {
                    return OpenTmuxSessionTarget::RepoPr;
                }
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

    fn move_repo_pr_selection(&mut self, direction: isize) -> bool {
        if self.focused_panel != PanelFocus::Repos
            || self.repo_main_view != view::RepoMainView::ChangeRequests
        {
            return false;
        }
        let prs = self.current_repo_open_pr_summaries();
        if prs.is_empty() {
            return false;
        }
        let current_number = self.selected_repo_pr_number();
        let current = current_number
            .and_then(|number| prs.iter().position(|summary| summary.number == number))
            .unwrap_or(0);
        let next = current as isize + direction;
        if next < 0 {
            return true;
        }
        if let Some(summary) = prs.get(next as usize)
            && let Some(repo) = self.repos.get(self.current_repo)
        {
            self.selected_pr_by_repo
                .insert(repo.repo.root.clone(), summary.number);
        }
        true
    }

    fn current_repo_open_pr_summaries(&self) -> Vec<crate::github::PrSummary> {
        self.repos
            .get(self.current_repo)
            .map(|managed| {
                managed
                    .pr_summaries
                    .iter()
                    .filter(|summary| !summary.merged && summary.state.eq_ignore_ascii_case("OPEN"))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    fn selected_repo_pr_number(&self) -> Option<u64> {
        let root = &self.repos.get(self.current_repo)?.repo.root;
        self.selected_pr_by_repo.get(root).copied()
    }

    pub(crate) fn selected_repo_pr_summary(&self) -> Option<crate::github::PrSummary> {
        let prs = self.current_repo_open_pr_summaries();
        let selected = self.selected_repo_pr_number();
        selected
            .and_then(|number| prs.iter().find(|summary| summary.number == number).cloned())
            .or_else(|| prs.first().cloned())
    }

    fn ensure_selected_repo_pr(&mut self) {
        let prs = self.current_repo_open_pr_summaries();
        let Some(first) = prs.first() else {
            return;
        };
        let selected = self.selected_repo_pr_number();
        if selected.is_some_and(|number| prs.iter().any(|summary| summary.number == number)) {
            return;
        }
        if let Some(repo) = self.repos.get(self.current_repo) {
            self.selected_pr_by_repo
                .insert(repo.repo.root.clone(), first.number);
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
            .and_then(|session| session.pr.details())
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
        self.notice_dialog(runtime, "Comment Details", lines)?;
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
        self.ensure_selected_repo_pr();
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
        self.ensure_selected_repo_pr();
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
        let (_, repos, worktrees) = view::sidebar_areas(sidebar);
        if point_in_rect(x, y, repos) {
            let row = y.saturating_sub(repos.y).saturating_sub(1) as usize;
            if let Some(index) = self.visible_repo_indices().get(row).copied() {
                self.select_repo(index);
                self.focus_repos();
            }
            return;
        }
        if point_in_rect(x, y, worktrees) {
            let row = y.saturating_sub(worktrees.y).saturating_sub(2) as usize;
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

    fn poll_workflow_runs(&mut self) -> bool {
        let mut changed = false;
        while let Ok(result) = self.workflow_poll_rx.try_recv() {
            if result.revision != self.workflow_revision {
                continue;
            }
            let Ok(snapshot) = result.snapshot else {
                continue;
            };
            if let Ok(runs) = snapshot.plan_runs {
                for run in runs {
                    changed |= self.remember_plan_run_snapshot(run);
                }
            }
            if let Ok(runs) = snapshot.auto_runs {
                for run in runs {
                    changed |= self.remember_auto_run_snapshot(run);
                }
            }
            if let Ok(runs) = snapshot.linked_plan_runs {
                for run in runs {
                    let run_id = run.run.id.clone();
                    changed |= self.linked_plan_runs.get(&run_id) != Some(&run);
                    self.linked_plan_runs.insert(run_id, run);
                }
            }
        }
        self.start_workflow_polls(false);
        changed
    }

    pub(crate) fn start_workflow_polls(&mut self, force: bool) {
        let revision = self.workflow_revision;
        let requests = self
            .repos
            .iter()
            .filter(|managed| {
                !self.workflow_polls_in_flight.contains(&managed.identity)
                    && (force
                        || self
                            .workflow_last_polled
                            .get(&managed.identity)
                            .is_none_or(|last| last.elapsed() >= Duration::from_secs(1)))
            })
            .map(|managed| (managed.repo.clone(), managed.identity.clone()))
            .collect::<Vec<_>>();
        for (repo, repository) in requests {
            self.workflow_polls_in_flight.insert(repository.clone());
            self.workflow_last_polled
                .insert(repository.clone(), Instant::now());
            let job_repository = repository.clone();
            self.spawn_tui_job(
                TuiJobKind::WorkflowPoll,
                TuiJobKey::WorkflowRepository(repository),
                revision,
                Some(TUI_ACTION_JOB_TIMEOUT),
                "prism-workflow-poll".to_string(),
                move |_| {
                    let snapshot = crate::observability::with_nonblocking_read_db_named(
                        &repo,
                        "tui.workflow.refresh",
                        |conn| {
                            let plan_runs = load_recent_plan_runs_for_repo(conn, &repo.root, 8);
                            let auto_runs =
                                load_recent_active_run_snapshots_for_repo(conn, &repo.root, 8);
                            let linked_plan_runs = match &auto_runs {
                                Ok(runs) => {
                                    let plan_ids = runs
                                        .iter()
                                        .flat_map(|run| &run.steps)
                                        .filter_map(|step| step.plan_run_id.as_ref())
                                        .collect::<BTreeSet<_>>();
                                    plan_ids
                                        .into_iter()
                                        .filter_map(|run_id| {
                                            load_plan_run(conn, run_id).transpose()
                                        })
                                        .collect::<Result<Vec<_>, _>>()
                                }
                                Err(_) => Ok(Vec::new()),
                            };
                            Ok(WorkflowPollSnapshot {
                                plan_runs,
                                auto_runs,
                                linked_plan_runs,
                            })
                        },
                    );
                    Ok(Some(TuiJobPayload::WorkflowPoll(WorkflowPollResult {
                        repository: job_repository,
                        revision,
                        snapshot,
                    })))
                },
            );
        }
    }

    pub(crate) fn load_plan_run_snapshot(&mut self, repo_root: &Path, run_id: &str) {
        let repo = Repository {
            root: repo_root.to_path_buf(),
        };
        if let Ok(Some(run)) = crate::observability::with_nonblocking_read_db_named(
            &repo,
            "tui.plan_run.snapshot",
            |conn| load_plan_run(conn, run_id),
        ) {
            self.remember_plan_run(run);
        }
    }

    pub(crate) fn remember_plan_run(&mut self, run: PersistedPlanRun) -> bool {
        let changed = self.remember_plan_run_snapshot(run);
        if changed {
            self.workflow_revision = self.workflow_revision.saturating_add(1);
        }
        changed
    }

    pub(crate) fn invalidate_workflow_snapshots(&mut self) {
        self.workflow_revision = self.workflow_revision.saturating_add(1);
    }

    fn remember_plan_run_snapshot(&mut self, run: PersistedPlanRun) -> bool {
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
        let output_lines = self.plan_output_snapshot(&repo, &run.run.id, run.run.selected_step);
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

    pub(crate) fn load_auto_run_snapshot(&mut self, repo_root: &Path, run_id: &str) {
        let repo = Repository {
            root: repo_root.to_path_buf(),
        };
        if let Ok(Some(run)) = crate::observability::with_nonblocking_read_db_named(
            &repo,
            "tui.auto_run.snapshot",
            |conn| load_auto_run_snapshot(conn, run_id),
        ) {
            self.remember_auto_run(run);
        }
    }

    pub(crate) fn remember_auto_run(&mut self, run: PersistedAutoRun) -> bool {
        let changed = self.remember_auto_run_snapshot(run);
        if changed {
            self.workflow_revision = self.workflow_revision.saturating_add(1);
        }
        changed
    }

    fn remember_auto_run_snapshot(&mut self, run: PersistedAutoRun) -> bool {
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

    fn poll_dashboard_outputs(&mut self) -> bool {
        let mut changed = false;
        while let Ok(result) = self.dashboard_output_rx.try_recv() {
            if result.revision != self.workflow_revision {
                continue;
            }
            let Ok(lines) = result.lines else {
                continue;
            };
            match (&result.key, lines) {
                (
                    DashboardOutputKey::Plan { run_id, step, .. },
                    DashboardOutputLines::Plan(lines),
                ) => {
                    let key = (run_id.clone(), *step);
                    changed |= self.plan_output_cache.borrow().get(&key) != Some(&lines);
                    self.plan_output_cache.borrow_mut().insert(key, lines);
                }
                (
                    DashboardOutputKey::Auto {
                        repository,
                        step_run_id,
                    },
                    DashboardOutputLines::Auto(lines),
                ) => {
                    let key = (repository.clone(), *step_run_id);
                    changed |= self.auto_output_cache.borrow().get(&key) != Some(&lines);
                    self.auto_output_cache.borrow_mut().insert(key, lines);
                }
                _ => {}
            }
        }

        let revision = self.workflow_revision;
        let requests = self.dashboard_output_requests();
        for (key, repo) in requests {
            if self.dashboard_outputs_in_flight.contains(&key)
                || self
                    .dashboard_output_last_polled
                    .get(&key)
                    .is_some_and(|last| last.elapsed() < Duration::from_secs(1))
            {
                continue;
            }
            self.dashboard_outputs_in_flight.insert(key.clone());
            self.dashboard_output_last_polled
                .insert(key.clone(), Instant::now());
            let job_key = key.clone();
            self.spawn_tui_job(
                TuiJobKind::DashboardOutput,
                TuiJobKey::DashboardOutput(key),
                revision,
                Some(TUI_ACTION_JOB_TIMEOUT),
                "prism-dashboard-output".to_string(),
                move |_| {
                    let lines = crate::observability::with_nonblocking_read_db_named(
                        &repo,
                        "tui.dashboard_output.refresh",
                        |conn| match &job_key {
                            DashboardOutputKey::Plan { run_id, step, .. } => {
                                load_output_lines(conn, run_id, *step)
                                    .map(DashboardOutputLines::Plan)
                            }
                            DashboardOutputKey::Auto { step_run_id, .. } => {
                                load_auto_output_lines(conn, *step_run_id)
                                    .map(DashboardOutputLines::Auto)
                            }
                        },
                    );
                    Ok(Some(TuiJobPayload::DashboardOutput(
                        DashboardOutputResult {
                            key: job_key,
                            revision,
                            lines,
                        },
                    )))
                },
            );
        }
        changed
    }

    fn dashboard_output_requests(&self) -> BTreeMap<DashboardOutputKey, Repository> {
        let mut requests = BTreeMap::new();
        if let Some((repo, run_id)) = self.selected_plan_run_id()
            && let Some(run) = self.plan_runs.get(&run_id)
        {
            requests.insert(
                DashboardOutputKey::Plan {
                    repository: WorktreeRepositoryKey::new(repo.root.clone()),
                    run_id,
                    step: self.resolved_plan_step_selection(run),
                },
                repo,
            );
        }
        if let Some((repo, worktree_path)) = self.selected_auto_scope()
            && let Some(run_id) = self.active_auto_runs.get(&worktree_path)
            && let Some(run) = self.auto_runs.get(run_id)
        {
            let selected_step_run_id = self
                .selected_auto_step_by_run
                .get(run_id)
                .copied()
                .or(run.run.selected_step_run_id)
                .or_else(|| run.steps.first().and_then(|step| step.id));
            if let Some(step_run_id) = selected_step_run_id {
                let repository = WorktreeRepositoryKey::new(repo.root.clone());
                requests.insert(
                    DashboardOutputKey::Auto {
                        repository: repository.clone(),
                        step_run_id,
                    },
                    repo.clone(),
                );
                if let Some(plan_run_id) = run
                    .steps
                    .iter()
                    .find(|step| step.id == Some(step_run_id))
                    .and_then(|step| step.plan_run_id.as_ref())
                    && let Some(plan_run) = self
                        .plan_runs
                        .get(plan_run_id)
                        .or_else(|| self.linked_plan_runs.get(plan_run_id))
                {
                    requests.insert(
                        DashboardOutputKey::Plan {
                            repository,
                            run_id: plan_run_id.clone(),
                            step: self.resolved_plan_step_selection(plan_run),
                        },
                        repo,
                    );
                }
            }
        }
        requests
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
            .map(|step_run_id| self.auto_output_snapshot(&repo, step_run_id))
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
        let mut run = self
            .plan_runs
            .get(plan_run_id)
            .or_else(|| self.linked_plan_runs.get(plan_run_id))?
            .clone();
        let run_scope_path = run.run.scope_path.clone();
        run.run.selected_step = self.resolved_plan_step_selection(&run);
        let output_lines = self.plan_output_snapshot(repo, &run.run.id, run.run.selected_step);
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

    fn plan_output_snapshot(
        &self,
        _repo: &Repository,
        run_id: &str,
        step: usize,
    ) -> Vec<PlanOutputLine> {
        let key = (run_id.to_string(), step);
        self.plan_output_cache
            .borrow()
            .get(&key)
            .cloned()
            .unwrap_or_default()
    }

    fn auto_output_snapshot(&self, repo: &Repository, step_run_id: i64) -> Vec<AutoOutputLine> {
        let key = (WorktreeRepositoryKey::new(repo.root.clone()), step_run_id);
        self.auto_output_cache
            .borrow()
            .get(&key)
            .cloned()
            .unwrap_or_default()
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
        let selected_pr_number = self.selected_repo_pr_number();
        let repo_prs = self
            .repos
            .get(self.current_repo)
            .map(|managed| {
                managed
                    .pr_summaries
                    .iter()
                    .filter(|summary| !summary.merged && summary.state.eq_ignore_ascii_case("OPEN"))
                    .map(|summary| {
                        let has_worktree = self.sessions.iter().any(|session| {
                            session.repo_index == self.current_repo
                                && session
                                    .pr
                                    .summary()
                                    .is_some_and(|pr| pr.number == summary.number)
                        });
                        view::RepoPrRow::from_summary(
                            managed.label.clone(),
                            summary,
                            has_worktree,
                            selected_pr_number == Some(summary.number),
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

    fn tmux_portal_model(&self) -> Option<view::TmuxPortalModel<'_>> {
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

    fn repo_health_label(&self, repo_index: usize) -> String {
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
            if matches!(
                session.agent_state,
                AgentState::NeedsInput | AgentState::NeedsRestart | AgentState::ExitedError
            ) || session.unseen_comments
            {
                attention += 1;
            }
            if session.pr.has_summary() {
                prs += 1;
            }
            match session
                .pr
                .summary()
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
            if matches!(
                session.agent_state,
                AgentState::Attached | AgentState::Running
            ) {
                running += 1;
            }
            if matches!(
                session.agent_state,
                AgentState::NeedsInput | AgentState::NeedsRestart | AgentState::ExitedError
            ) || session.unseen_comments
            {
                attention += 1;
            }
            if session.pr.has_summary() {
                prs += 1;
            }
            match session
                .pr
                .summary()
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
                    view::KeyChoice::new("a", "auto flow"),
                    self.git_choice(GitAction::LazyGit, "g", "lazygit"),
                    self.git_choice(GitAction::Push, "P", "push/create PR"),
                    self.git_choice(GitAction::OpenPr, "o", "open PR"),
                    self.git_choice(GitAction::Merge, "M", "merge"),
                    self.git_choice(GitAction::CiFix, "c", "CI repair"),
                    self.git_choice(GitAction::ReviewFix, "f", "review repair"),
                    self.git_choice(GitAction::ResolveAllComments, "R", "resolve all comments"),
                ],
            }),
            (None, _) => None,
        }
    }
}

fn selectable_choice_key(choices: &view::ChoiceList, key: &str) -> Option<String> {
    choices
        .choices
        .iter()
        .find(|option| !option.disabled && option.key.eq_ignore_ascii_case(key))
        .map(|option| option.key.to_ascii_lowercase())
}

fn choice_list(title: &str, choices: &[(&str, &str)]) -> view::ChoiceList {
    view::ChoiceList {
        title: title.to_string(),
        choices: choices
            .iter()
            .map(|(key, label)| view::KeyChoice::new(*key, *label))
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
    if let Some(label) = session.pr.last_refreshed() {
        return label.to_string();
    }
    if let Some(summary) = session.pr.summary() {
        return summary.updated_at.chars().take(10).collect();
    }
    "-".to_string()
}

#[cfg(test)]
fn test_default_worktree_harness_config(config: &Config) -> Option<Config> {
    config.for_harness("opencode").ok()
}

#[cfg(not(test))]
fn test_default_worktree_harness_config(_config: &Config) -> Option<Config> {
    None
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
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use ratatui::text::Line;

    use crate::agent::AgentState;
    use crate::agent_session::{AgentSessionSlot, AgentSessionWarmupKey, AgentSessionWarmupResult};
    use crate::auto_flow::{
        AutoImplementationSource, AutoRun, AutoRunMode, AutoRunStatus, PersistedAutoRun,
    };
    use crate::config::Config;
    use crate::github::{PrCache, PrDetails, PrReviewComment, PrSummary};
    use crate::opencode::{OpencodeState, OpencodeStatus, parse_event_payload};
    use crate::plan_run::{
        PersistedPlanRun, PlanOutputKind, PlanOutputLine, PlanRun, PlanRunMode, PlanRunStatus,
        PlanStepRun, PlanStepStatus,
    };
    use crate::repo::Repository;
    use crate::session::{Session, WorktreeRepositoryKey};
    use crate::tui_jobs::{CoalescedFacet, JobRegistry};
    use crate::view::{ChoiceList, KeyChoice, OrderedToggleItem, RepoMainView, WorktreeMainView};

    use super::{
        GitAction, ManagedRepo, OpenTmuxSessionTarget, OpencodePollKey, OpencodePollResult,
        PanelFocus, PrPollKey, PrPollResult, PrSummarySessionResult, TmuxPortalCapture,
        TmuxPortalResult, TmuxPortalSnapshot, Tui, TuiJobKey, TuiJobKind, WorktreeListMode,
        confirmation_result, move_enabled_ordered_item, selectable_choice_key,
        toggle_item_in_place, toggle_ordered_item,
    };

    #[test]
    fn confirmation_empty_answer_uses_the_passed_default() {
        assert_eq!(confirmation_result("", true), Some(true));
        assert_eq!(confirmation_result("", false), Some(false));
    }

    #[test]
    fn confirmation_yes_and_no_override_the_default() {
        assert_eq!(confirmation_result("y", false), Some(true));
        assert_eq!(confirmation_result("n", true), Some(false));
    }

    #[test]
    fn confirmation_rejects_unknown_answers() {
        assert_eq!(confirmation_result("maybe", true), None);
        assert_eq!(confirmation_result("ny", false), None);
    }

    #[test]
    fn running_agent_does_not_block_quit() {
        let repo = Repository {
            root: PathBuf::from("/tmp/repo"),
        };
        let mut session = test_session(0, "/tmp/repo", "feature");
        session.agent_state = AgentState::Running;
        let mut tui = Tui::new_single(repo, test_config(), vec![session]);

        assert!(tui.confirm_quit().unwrap());
        assert!(tui.dialog.is_none());
    }

    #[test]
    fn shutdown_notification_requests_the_matching_run_loop_exit_path() {
        let notification = crate::tui_signal::ShutdownNotification::for_test();
        assert_eq!(super::requested_shutdown(&notification), None);

        notification.request_for_test(crate::tui_signal::ShutdownSignal::Sigterm);

        assert_eq!(
            super::requested_shutdown(&notification),
            Some(super::ShutdownReason::Sigterm)
        );
    }

    #[test]
    fn opencode_in_flight_clears_after_panic_and_spawn_failure_then_restarts() {
        let _ = crate::observability::take_captured_events();
        let temp = unique_temp_dir("prism-tui-job-recovery-test");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let session = test_session(0, &temp.join("worktree").display().to_string(), "feature");
        let mut tui = Tui::new_single(repo, test_config(), vec![session]);
        let key = super::OpencodePollKey::for_repository_session(
            &tui.repos[0].identity,
            &tui.sessions[0],
        );

        tui.opencode_polls_in_flight.insert(key.clone());
        tui.spawn_tui_job(
            TuiJobKind::OpencodePoll,
            TuiJobKey::Opencode(key.clone()),
            key.generation,
            Some(Duration::from_secs(1)),
            "panic-before-result".to_string(),
            |_| panic!("before result"),
        );
        wait_for_opencode_job(&mut tui, &key);
        assert!(!tui.opencode_polls_in_flight.contains(&key));

        tui.opencode_polls_in_flight.insert(key.clone());
        tui.jobs.fail_next_spawn();
        tui.spawn_tui_job(
            TuiJobKind::OpencodePoll,
            TuiJobKey::Opencode(key.clone()),
            key.generation,
            Some(Duration::from_secs(1)),
            "spawn-failure".to_string(),
            |_| Ok(None),
        );
        wait_for_opencode_job(&mut tui, &key);
        assert!(!tui.opencode_polls_in_flight.contains(&key));

        tui.opencode_polls_in_flight.insert(key.clone());
        tui.spawn_tui_job(
            TuiJobKind::OpencodePoll,
            TuiJobKey::Opencode(key.clone()),
            key.generation,
            Some(Duration::from_secs(1)),
            "restart-after-failure".to_string(),
            |_| Ok(None),
        );
        wait_for_opencode_job(&mut tui, &key);
        assert!(!tui.opencode_polls_in_flight.contains(&key));

        let terminal_events = crate::observability::take_captured_events()
            .into_iter()
            .filter(|event| event.target == "tui_job" && event.action == "terminal")
            .filter_map(|event| event.data_json)
            .map(|data| serde_json::from_str::<serde_json::Value>(&data).unwrap())
            .filter(|data| data["kind"] == "opencode_poll")
            .filter(|data| {
                data["key"]
                    .as_str()
                    .is_some_and(|key| key.contains(&temp.display().to_string()))
            })
            .collect::<Vec<_>>();
        for (job_id, outcome) in [(1, "panicked"), (2, "spawn_failed"), (3, "completed")] {
            let matching = terminal_events
                .iter()
                .filter(|data| data["job_id"] == job_id && data["outcome"] == outcome)
                .count();
            assert_eq!(matching, 1, "job {job_id} outcome {outcome}");
        }

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn tui_tick_terminal_budget_retains_every_remaining_outcome() {
        let repo = Repository {
            root: PathBuf::from("/tmp/repo"),
        };
        let mut tui = Tui::new_single(repo, test_config(), Vec::new());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(101));
        let completed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for index in 0..100 {
            let barrier = barrier.clone();
            let completed = completed.clone();
            tui.spawn_tui_job(
                TuiJobKind::WorkflowMaintenance,
                TuiJobKey::None,
                0,
                None,
                format!("budget-{index}"),
                move |_| {
                    barrier.wait();
                    completed.fetch_add(1, std::sync::atomic::Ordering::Release);
                    Ok(None)
                },
            );
        }
        barrier.wait();
        while completed.load(std::sync::atomic::Ordering::Acquire) != 100 {
            std::thread::yield_now();
        }
        while !tui.jobs.active_metadata().is_empty() {
            tui.jobs.collect_finished();
            std::thread::yield_now();
        }

        tui.route_tui_job_messages();

        assert_eq!(
            tui.jobs.queue_stats().terminal_depth,
            100 - super::TUI_TICK_ITEM_BUDGET
        );
    }

    #[test]
    fn opencode_snapshot_burst_converges_through_bounded_coalesced_slots() {
        let temp = unique_temp_dir("prism-opencode-coalesced-burst-test");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let mut session = test_session(0, &temp.join("worktree").display().to_string(), "feature");
        session.agent_state = AgentState::Running;
        session.opencode_status = Some(OpencodeStatus {
            server_url: Some("http://127.0.0.1:1".to_string()),
            session_id: Some("ses_1".to_string()),
            title: None,
            state: OpencodeState::Busy,
            detail: None,
            latest_message: None,
            latest_user_message: None,
            recent_messages: Vec::new(),
            active_tool: None,
            todos: Vec::new(),
            last_updated_unix_ms: None,
        });
        let mut tui = Tui::new_single(repo, test_config(), vec![session]);
        tui.jobs = JobRegistry::with_event_capacity(2);
        let worktree = tui.sessions[0].identity_key(&tui.repos[0].identity);
        let stream = super::OpencodeListenerKey {
            worktree: worktree.clone(),
            generation: 0,
            session_id: "ses_1".to_string(),
            server_url: "http://127.0.0.1:1".to_string(),
        };
        tui.opencode_listeners.insert(stream.clone());
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(0);
        let job_stream = stream.clone();
        let job_id = tui.jobs.spawn(
            TuiJobKind::OpencodeListener,
            TuiJobKey::OpencodeListener(stream),
            0,
            None,
            "coalesced-listener".to_string(),
            move |context| {
                let send = |context: &crate::tui_jobs::JobContext<_, _, _>, facet, event| {
                    context.send_coalesced(
                        facet,
                        super::TuiJobPayload::OpencodeEvent(super::OpencodeEventResult {
                            stream: job_stream.clone(),
                            received_at: Instant::now(),
                            event: Ok(event),
                        }),
                    )
                };
                send(
                    &context,
                    CoalescedFacet::Status,
                    parse_event_payload(
                        r#"{"type":"session.status","properties":{"sessionID":"ses_1","status":"busy"}}"#,
                    )
                    .unwrap(),
                )?;
                send(
                    &context,
                    CoalescedFacet::Message,
                    parse_event_payload(
                        r#"{"type":"message.part.updated","properties":{"sessionID":"ses_1","role":"assistant","text":"initial"}}"#,
                    )
                    .unwrap(),
                )?;
                for index in 0..100 {
                    let state = if index == 99 { "retry" } else { "busy" };
                    send(
                        &context,
                        CoalescedFacet::Status,
                        parse_event_payload(&format!(
                            r#"{{"type":"session.status","properties":{{"sessionID":"ses_1","status":"{state}"}}}}"#
                        ))
                        .unwrap(),
                    )?;
                    send(
                        &context,
                        CoalescedFacet::Message,
                        parse_event_payload(&format!(
                            r#"{{"type":"message.part.updated","properties":{{"sessionID":"ses_1","role":"assistant","text":"message-{index}"}}}}"#
                        ))
                        .unwrap(),
                    )?;
                }
                ready_tx.send(()).unwrap();
                while !context.wait(Duration::from_secs(60)) {}
                Ok(None)
            },
        );
        ready_rx.recv().unwrap();

        tui.route_tui_job_messages();

        let status = tui.sessions[0].opencode_status.as_ref().unwrap();
        assert_eq!(status.state, OpencodeState::Retry);
        assert_eq!(status.latest_message.as_deref(), Some("message-99"));
        assert!(tui.opencode_reconcile_requested.contains_key(&worktree));
        let stats = tui.jobs.queue_stats();
        assert_eq!(stats.event_capacity, 2);
        assert_eq!(stats.event_depth, 0);
        assert_eq!(stats.coalesced_depth, 0);
        assert_eq!(stats.coalesced_capacity, 2);
        assert_eq!(stats.overflow_total, 200);
        assert_eq!(stats.coalesced_total, 198);

        tui.jobs.cancel(job_id);
        while tui.jobs.has_jobs() {
            tui.route_tui_job_messages();
            std::thread::yield_now();
        }
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn opencode_overflow_requests_full_reconciliation_and_stale_events_cannot_regress_it() {
        let temp = unique_temp_dir("prism-opencode-overflow-reconcile-test");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let mut session = test_session(0, &temp.join("worktree").display().to_string(), "feature");
        session.agent_state = AgentState::Running;
        session.opencode_status = Some(OpencodeStatus {
            server_url: Some("http://127.0.0.1:1".to_string()),
            session_id: Some("ses_1".to_string()),
            title: None,
            state: OpencodeState::Busy,
            detail: None,
            latest_message: None,
            latest_user_message: None,
            recent_messages: Vec::new(),
            active_tool: None,
            todos: Vec::new(),
            last_updated_unix_ms: None,
        });
        let mut tui = Tui::new_single(repo, test_config(), vec![session]);
        let worktree = tui.sessions[0].identity_key(&tui.repos[0].identity);
        let stream = super::OpencodeListenerKey {
            worktree: worktree.clone(),
            generation: 0,
            session_id: "ses_1".to_string(),
            server_url: "http://127.0.0.1:1".to_string(),
        };
        tui.opencode_listeners.insert(stream.clone());
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(0);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
        let job_stream = stream.clone();
        tui.spawn_tui_job(
            TuiJobKind::OpencodeListener,
            TuiJobKey::OpencodeListener(stream),
            0,
            None,
            "overflow-listener".to_string(),
            move |context| {
                for _ in 0..1_000 {
                    context.send(super::TuiJobPayload::OpencodeEvent(
                        super::OpencodeEventResult {
                            stream: job_stream.clone(),
                            received_at: Instant::now(),
                            event: Ok(parse_event_payload(
                                r#"{"type":"todo.updated","properties":{"sessionID":"ses_1","todos":[{"content":"ordered","status":"pending"}]}}"#,
                            )
                            .unwrap()),
                        },
                    ))?;
                }
                context.send_coalesced(
                    CoalescedFacet::Status,
                    super::TuiJobPayload::OpencodeEvent(super::OpencodeEventResult {
                        stream: job_stream.clone(),
                        received_at: Instant::now(),
                        event: Ok(parse_event_payload(
                            r#"{"type":"session.status","properties":{"sessionID":"ses_1","status":"error"}}"#,
                        )
                        .unwrap()),
                    }),
                )?;
                context.send_coalesced(
                    CoalescedFacet::Message,
                    super::TuiJobPayload::OpencodeEvent(super::OpencodeEventResult {
                        stream: job_stream.clone(),
                        received_at: Instant::now(),
                        event: Ok(parse_event_payload(
                            r#"{"type":"message.part.updated","properties":{"sessionID":"ses_1","role":"assistant","text":"stale message"}}"#,
                        )
                        .unwrap()),
                    }),
                )?;
                ready_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(None)
            },
        );
        ready_rx.recv().unwrap();

        tui.route_tui_job_messages();
        let requested_at = *tui.opencode_reconcile_requested.get(&worktree).unwrap();
        let stats = tui.jobs.queue_stats();
        assert_eq!(stats.overflow_total, 1_002 - stats.event_capacity as u64);
        assert!(stats.event_depth <= stats.event_capacity);
        assert_eq!(stats.coalesced_depth, 2);

        let poll_key = super::OpencodePollKey::for_repository_session(
            &tui.repos[0].identity,
            &tui.sessions[0],
        );
        tui.opencode_poll_tx
            .send(super::OpencodePollResult {
                key: poll_key,
                started_at: requested_at + Duration::from_nanos(1),
                status: Ok(OpencodeStatus {
                    state: OpencodeState::Done,
                    latest_message: Some("fresh poll message".to_string()),
                    ..tui.sessions[0].opencode_status.clone().unwrap()
                }),
            })
            .unwrap();
        tui.poll_opencode_status();
        assert!(!tui.opencode_reconcile_requested.contains_key(&worktree));

        for _ in 0..16 {
            tui.route_tui_job_messages();
            tui.poll_opencode_events();
        }
        assert_eq!(
            tui.sessions[0].opencode_status.as_ref().unwrap().state,
            OpencodeState::Done
        );
        assert_eq!(
            tui.sessions[0]
                .opencode_status
                .as_ref()
                .unwrap()
                .latest_message
                .as_deref(),
            Some("fresh poll message")
        );
        assert_eq!(tui.jobs.queue_stats().coalesced_depth, 0);

        release_tx.send(()).unwrap();
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn stale_opencode_job_payload_is_rejected_after_generation_changes() {
        let temp = unique_temp_dir("prism-tui-job-generation-test");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let session = test_session(0, &temp.join("worktree").display().to_string(), "feature");
        let mut tui = Tui::new_single(repo, test_config(), vec![session]);
        let session_key = tui.sessions[0].identity_key(&tui.repos[0].identity);
        let key = super::OpencodePollKey::for_repository_session(
            &tui.repos[0].identity,
            &tui.sessions[0],
        );
        tui.opencode_polls_in_flight.insert(key.clone());
        let payload_key = key.clone();
        tui.spawn_tui_job(
            TuiJobKind::OpencodePoll,
            TuiJobKey::Opencode(key.clone()),
            key.generation,
            Some(Duration::from_secs(1)),
            "stale-opencode-poll".to_string(),
            move |_| {
                Ok(Some(super::TuiJobPayload::OpencodePoll(
                    super::OpencodePollResult {
                        key: payload_key,
                        started_at: Instant::now(),
                        status: Ok(crate::opencode::OpencodeStatus {
                            server_url: None,
                            session_id: None,
                            title: None,
                            state: crate::opencode::OpencodeState::Busy,
                            detail: None,
                            latest_message: None,
                            latest_user_message: None,
                            recent_messages: Vec::new(),
                            active_tool: None,
                            todos: Vec::new(),
                            last_updated_unix_ms: None,
                        }),
                    },
                )))
            },
        );
        *tui.worktree_generations.get_mut(&session_key).unwrap() = 1;

        wait_for_opencode_job(&mut tui, &key);
        tui.poll_opencode_status();

        assert!(tui.sessions[0].opencode_status.is_none());
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn cleanup_cancels_and_joins_listener_job() {
        let _ = crate::observability::take_captured_events();
        let (mut tui, stopped_rx) = tui_with_active_listener("user-quit");

        tui.cleanup_tui_jobs(super::ShutdownReason::UserQuit)
            .unwrap();

        stopped_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(!tui.jobs.has_jobs());
        assert!(tui.opencode_listeners.is_empty());
        let cleanup = crate::observability::take_captured_events()
            .into_iter()
            .filter(|event| event.target == "tui" && event.action == "shutdown_cleanup")
            .filter_map(|event| event.data_json)
            .map(|data| serde_json::from_str::<serde_json::Value>(&data).unwrap())
            .find(|data| data["reason"] == "user_quit" && data["active_jobs"] == 1)
            .unwrap();
        assert_eq!(cleanup["reason"], "user_quit");
        assert_eq!(cleanup["active_jobs"], 1);
        assert_eq!(cleanup["unfinished_jobs"], 0);
    }

    #[test]
    fn run_error_path_cleans_up_active_listener() {
        let (mut tui, stopped_rx) = tui_with_active_listener("run-error");

        let error = tui
            .finish_run(Ok(Err("injected draw error".to_string())), None)
            .unwrap_err();

        assert_eq!(error, "injected draw error");
        stopped_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(!tui.jobs.has_jobs());
        assert!(tui.opencode_listeners.is_empty());
    }

    #[test]
    fn sigterm_exit_path_cleans_up_active_listener() {
        let (mut tui, stopped_rx) = tui_with_active_listener("sigterm");
        let notification = crate::tui_signal::ShutdownNotification::for_test();
        notification.request_for_test(crate::tui_signal::ShutdownSignal::Sigterm);
        assert_eq!(
            super::requested_shutdown(&notification),
            Some(super::ShutdownReason::Sigterm)
        );

        tui.finish_run(
            Ok(Err("interactive subprocess canceled".to_string())),
            notification.signal(),
        )
        .unwrap();

        stopped_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(!tui.jobs.has_jobs());
        assert!(tui.opencode_listeners.is_empty());
    }

    fn tui_with_active_listener(label: &str) -> (Tui, std::sync::mpsc::Receiver<()>) {
        let repo = Repository {
            root: PathBuf::from(format!("/tmp/prism-cleanup-{label}")),
        };
        let session = test_session(
            0,
            &format!("/tmp/prism-cleanup-{label}/worktree"),
            "feature",
        );
        let mut tui = Tui::new_single(repo, test_config(), vec![session]);
        let key = tui.sessions[0].identity_key(&tui.repos[0].identity);
        let stream = super::OpencodeListenerKey {
            worktree: key,
            generation: 0,
            session_id: "ses_1".to_string(),
            server_url: "http://127.0.0.1:41000".to_string(),
        };
        let (stopped_tx, stopped_rx) = std::sync::mpsc::channel();
        tui.opencode_listeners.insert(stream.clone());
        tui.spawn_tui_job(
            TuiJobKind::OpencodeListener,
            TuiJobKey::OpencodeListener(stream),
            0,
            None,
            "cleanup-listener".to_string(),
            move |context| {
                while !context.wait(Duration::from_secs(60)) {}
                stopped_tx.send(()).unwrap();
                Ok(None)
            },
        );

        (tui, stopped_rx)
    }

    fn wait_for_opencode_job(tui: &mut Tui, key: &super::OpencodePollKey) {
        let started = Instant::now();
        while tui.opencode_polls_in_flight.contains(key) {
            tui.route_tui_job_messages();
            assert!(started.elapsed() < Duration::from_secs(1));
            std::thread::yield_now();
        }
    }

    #[test]
    fn workflow_polling_does_not_access_database_on_tui_thread() {
        let temp = unique_temp_dir("prism-tui-database-poll-test");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        crate::observability::with_writable_db(&repo, |_| Ok(())).unwrap();
        let mut tui = Tui::new_single(repo, test_config(), Vec::new());

        crate::observability::deny_database_access_on_current_thread(|| {
            tui.tick_tui_action_jobs();
        });

        assert_eq!(tui.workflow_polls_in_flight.len(), 1);

        drop(tui);
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn applying_pr_poll_result_does_no_io_on_tui_thread() {
        let temp = unique_temp_dir("prism-tui-pr-result-test");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        crate::observability::with_writable_db(&repo, |_| Ok(())).unwrap();
        let session = test_session(0, &temp.display().to_string(), "feature");
        let mut tui = Tui::new_single(repo, test_config(), vec![session]);
        let repository = tui.repos[0].identity.clone();
        let session_key = tui.sessions[0].identity_key(&repository);
        let poll_started_at = Instant::now();
        tui.sessions[0].pr.begin_summary_poll(poll_started_at);
        tui.repos[0].pr_summary_last_polled = Some(poll_started_at);
        tui.repos[0].pr_summary_poll_in_flight = true;
        tui.pr_poll_tx
            .send(PrPollResult::Summary {
                repository: repository.clone(),
                sessions: vec![session_key.clone()],
                github_remote_configured: true,
                summaries: Ok(vec![test_pr_summary(false)]),
                observations: Ok(vec![PrSummarySessionResult {
                    key: session_key,
                    summary: Some(test_pr_summary(false)),
                }]),
                refreshed: "now".to_string(),
                poll_started_at,
            })
            .unwrap();
        let tmux_slot = AgentSessionSlot::for_repository_session(&repository, &tui.sessions[0]);
        tui.tmux_generations.insert(tmux_slot.clone(), 0);
        tui.tmux_warmup_tx
            .send(AgentSessionWarmupResult {
                key: AgentSessionWarmupKey::new(tmux_slot, 0),
                running: Some(true),
                error: None,
            })
            .unwrap();
        let opencode_key =
            OpencodePollKey::for_repository_session_generation(&repository, &tui.sessions[0], 0);
        tui.opencode_poll_tx
            .send(OpencodePollResult {
                key: opencode_key,
                started_at: Instant::now(),
                status: Ok(OpencodeStatus {
                    server_url: Some("http://127.0.0.1:41000".to_string()),
                    session_id: Some("ses_1".to_string()),
                    title: None,
                    state: OpencodeState::Busy,
                    detail: None,
                    latest_message: None,
                    latest_user_message: None,
                    recent_messages: Vec::new(),
                    active_tool: None,
                    todos: Vec::new(),
                    last_updated_unix_ms: Some(1),
                }),
            })
            .unwrap();

        let changes = crate::flight_recorder::deny_external_calls_on_current_thread(|| {
            crate::observability::deny_database_access_on_current_thread(|| {
                tui.tick_tui_action_jobs()
            })
        });

        assert!(changes.pull_requests);
        assert_eq!(tui.sessions[0].pr.summary().unwrap().number, 1);
        assert_eq!(tui.sessions[0].agent_state, AgentState::Running);

        drop(tui);
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn failed_pr_details_respect_retry_backoff_on_tui_tick() {
        let temp = unique_temp_dir("prism-tui-pr-details-backoff-test");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let mut tui = Tui::new_single(
            repo,
            test_config(),
            vec![test_session(0, &temp.display().to_string(), "feature")],
        );
        tui.focused_panel = PanelFocus::Worktrees;
        tui.sessions[0].pr = PrCache::observed(test_pr_summary(false), None);
        let mut failed_poll = tui.sessions[0].pr.begin_details_poll();
        crate::remote::dispatcher::refresh_change_request_details_state(
            "feature",
            &mut failed_poll,
            &tui.sessions[0].path,
            &tui.repos[0].config,
        );
        let repository = tui.repos[0].identity.clone();
        let generation = tui.worktree_generations[&tui.sessions[0].identity_key(&repository)];
        let key =
            PrPollKey::for_repository_session_generation(&repository, &tui.sessions[0], generation);
        tui.repos[0].pr_summary_last_polled = Some(Instant::now());
        tui.repos[0].pr_summary_poll_in_flight = true;
        tui.pr_poll_tx
            .send(PrPollResult::Details {
                key,
                cache: Box::new(failed_poll),
            })
            .unwrap();

        crate::flight_recorder::deny_external_calls_on_current_thread(|| {
            crate::observability::deny_database_access_on_current_thread(|| {
                tui.tick_tui_action_jobs();
            });
        });

        assert!(tui.pr_polls_in_flight.is_empty());
        assert!(tui.sessions[0].pr.details().is_none());

        drop(tui);
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn repeated_dashboard_rendering_uses_only_cached_output() {
        let temp = unique_temp_dir("prism-tui-dashboard-database-test");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        crate::observability::with_writable_db(&repo, |_| Ok(())).unwrap();
        let session = test_session(0, &temp.display().to_string(), "feature");
        let scope_path = session.path.clone();
        let mut run = test_plan_run_with_steps("plan", &scope_path.display().to_string(), 1);
        run.run.repo_root = temp.display().to_string();
        crate::observability::with_writable_db(&repo, |conn| {
            crate::plan_run::save_plan_run(conn, &run)?;
            crate::plan_run::append_output_line(
                conn,
                &PlanOutputLine {
                    run_id: "plan".to_string(),
                    step: 1,
                    line_number: 1,
                    time_unix_ms: 1,
                    kind: PlanOutputKind::Assistant,
                    text: "cached output".to_string(),
                    block_id: None,
                },
                100,
            )
        })
        .unwrap();
        let mut tui = Tui::new_single(repo.clone(), test_config(), vec![session]);
        tui.focused_panel = PanelFocus::Worktrees;
        tui.remember_plan_run(run);
        tui.plan_output_cache.borrow_mut().insert(
            ("plan".to_string(), 1),
            vec![PlanOutputLine {
                run_id: "plan".to_string(),
                step: 1,
                line_number: 1,
                time_unix_ms: 1,
                kind: PlanOutputKind::Assistant,
                text: "cached output".to_string(),
                block_id: None,
            }],
        );

        let dashboards = crate::observability::deny_database_access_on_current_thread(|| {
            (0..3)
                .map(|_| tui.current_plan_dashboard())
                .collect::<Vec<_>>()
        });

        assert!(dashboards.iter().all(|dashboard| {
            dashboard.as_ref().unwrap().output_lines[0].text == "cached output"
        }));

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn returning_from_tmux_does_not_wait_for_worktree_refresh() {
        let temp = unique_temp_dir("prism-tmux-return-refresh-test");
        let worktree = temp.join("feature");
        fs::create_dir_all(&worktree).unwrap();
        fs::write(worktree.join(".git"), "gitdir: /tmp/gitdir\n").unwrap();
        let git = temp.join("git");
        let refresh_gate = temp.join("allow-refresh");
        fs::write(
            &git,
            format!(
                r#"#!/bin/sh
case "$*" in
  *"worktree list --porcelain"*)
    while [ ! -f {:?} ]; do sleep 0.1; done
    printf 'worktree {}\nHEAD abc\nbranch refs/heads/feature\n\n'
    ;;
  *"status --short --branch"*) printf '## feature\n' ;;
  *"remote get-url origin"*) printf 'git@github.com:owner/repo.git\n' ;;
esac
"#,
                refresh_gate.display().to_string(),
                worktree.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&git).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&git, permissions).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        fs::create_dir_all(repo.prism_dir()).unwrap();
        fs::write(
            repo.prism_dir().join("config.toml"),
            format!("[tools]\ngit = {:?}\n", git.display().to_string()),
        )
        .unwrap();
        let config = Config::load(&repo);
        let session = test_session(0, &temp.display().to_string(), "feature");
        let mut tui = Tui::new_single(repo, config, vec![session]);
        tui.focused_panel = PanelFocus::Worktrees;
        tui.tmux_portal_size = Some((72, 18));

        let started = Instant::now();
        crate::flight_recorder::deny_external_calls_on_current_thread(|| {
            crate::observability::deny_database_access_on_current_thread(|| {
                tui.refresh_sessions_after_tmux().unwrap();
                tui.refresh_sessions_after_tmux().unwrap();
                tui.poll_tmux_portal();
            });
        });
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_millis(250),
            "returning from tmux waited for refresh for {elapsed:?}"
        );

        fs::write(refresh_gate, "").unwrap();
        let wait_started = Instant::now();
        while tui.session_refresh_in_flight && wait_started.elapsed() < Duration::from_secs(3) {
            crate::flight_recorder::deny_external_calls_on_current_thread(|| {
                crate::observability::deny_database_access_on_current_thread(|| {
                    tui.poll_session_refresh();
                });
            });
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!tui.session_refresh_in_flight);
        assert!(!tui.session_refresh_pending);

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn disabled_choice_keys_are_not_selectable() {
        let choices = ChoiceList {
            title: "Harness".to_string(),
            choices: vec![
                KeyChoice::disabled("1", "OpenCode"),
                KeyChoice::new("2", "Codex"),
            ],
        };

        assert_eq!(selectable_choice_key(&choices, "1"), None);
        assert_eq!(selectable_choice_key(&choices, "2").as_deref(), Some("2"));
    }

    #[test]
    fn git_actions_disable_pr_operations_until_a_pr_is_known() {
        let repo = Repository {
            root: PathBuf::from("/tmp/repo"),
        };
        let mut tui = Tui::new_single(
            repo,
            test_config(),
            vec![test_session(0, "/tmp/repo", "feature")],
        );
        tui.focused_panel = PanelFocus::Worktrees;

        assert!(tui.git_action_enabled(GitAction::Push));
        assert!(!tui.git_action_enabled(GitAction::OpenPr));
        assert!(!tui.git_action_enabled(GitAction::Merge));

        tui.sessions[0].pr = PrCache::observed(test_pr_summary(false), None);
        assert!(tui.git_action_enabled(GitAction::OpenPr));
        assert!(tui.git_action_enabled(GitAction::Merge));
        assert!(!tui.git_action_enabled(GitAction::CiFix));
        tui.sessions[0].pr = PrCache::observed(
            test_pr_summary(false),
            Some(PrDetails {
                ci_failures: vec![crate::github::CiFailure {
                    name: "failed".to_string(),
                    ..crate::github::CiFailure::default()
                }],
                ..PrDetails::default()
            }),
        );
        assert!(tui.git_action_enabled(GitAction::CiFix));

        tui.sessions[0].pr = PrCache::observed(test_pr_summary(true), None);
        assert!(tui.git_action_enabled(GitAction::OpenPr));
        assert!(!tui.git_action_enabled(GitAction::Merge));
        assert!(!tui.git_action_enabled(GitAction::ReviewFix));

        let mut closed = test_pr_summary(false);
        closed.state = "CLOSED".to_string();
        tui.sessions[0].pr = PrCache::observed(closed, None);
        assert!(tui.git_action_enabled(GitAction::OpenPr));
        assert!(!tui.git_action_enabled(GitAction::Merge));
        assert!(!tui.git_action_enabled(GitAction::CiFix));
    }

    #[test]
    fn submit_review_requires_the_configured_gh_executable() {
        let temp = unique_temp_dir("prism-tui-submit-review-test");
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let mut config = test_config();
        let mut tui = Tui::new_single(repo, config.clone(), Vec::new());
        tui.focused_panel = PanelFocus::Repos;
        tui.main_focused = true;
        tui.repos[0].pr_summaries = vec![test_pr_summary(false)];

        assert!(!tui.git_action_enabled(GitAction::SubmitReview));

        crate::test_support::install_tool(&mut config, &temp, "gh", "#!/bin/sh\nexit 0\n");
        tui.repos[0].config = config;

        assert!(tui.git_action_enabled(GitAction::SubmitReview));

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn review_resolution_action_requires_main_panel_and_unresolved_threads() {
        let repo = Repository {
            root: PathBuf::from("/tmp/repo"),
        };
        let mut tui = Tui::new_single(
            repo,
            test_config(),
            vec![test_session(0, "/tmp/repo", "feature")],
        );
        tui.focused_panel = PanelFocus::Worktrees;
        tui.sessions[0].pr = PrCache::observed(
            test_pr_summary(false),
            Some(PrDetails {
                review_comments: vec![PrReviewComment {
                    thread_id: "thread-1".to_string(),
                    body: "inline".to_string(),
                    resolved: false,
                    ..PrReviewComment::default()
                }],
                ..PrDetails::default()
            }),
        );

        assert!(!tui.git_action_enabled(GitAction::ResolveAllComments));

        tui.focus_main();
        assert!(tui.git_action_enabled(GitAction::ResolveAllComments));

        tui.sessions[0].pr.mark_preserved_stale();
        assert!(!tui.git_action_enabled(GitAction::ResolveAllComments));

        tui.sessions[0].pr = PrCache::observed(
            test_pr_summary(false),
            Some(PrDetails {
                review_comments: vec![PrReviewComment {
                    thread_id: "  ".to_string(),
                    resolved: false,
                    ..PrReviewComment::default()
                }],
                ..PrDetails::default()
            }),
        );
        assert!(!tui.git_action_enabled(GitAction::ResolveAllComments));

        tui.sessions[0].pr = PrCache::observed(
            test_pr_summary(false),
            Some(PrDetails {
                review_comments: vec![PrReviewComment {
                    thread_id: "thread-1".to_string(),
                    resolved: true,
                    ..PrReviewComment::default()
                }],
                ..PrDetails::default()
            }),
        );
        assert!(!tui.git_action_enabled(GitAction::ResolveAllComments));
    }

    #[test]
    fn tmux_portal_rejects_capture_from_previous_generation() {
        let repo = Repository {
            root: PathBuf::from("/tmp/repo"),
        };
        let mut tui = Tui::new_single(
            repo,
            test_config(),
            vec![test_session(0, "/tmp/repo", "feature")],
        );
        tui.focused_panel = PanelFocus::Worktrees;
        tui.tmux_portal_size = Some((72, 18));
        tui.refresh_worktree_harness_configs();
        let slot =
            AgentSessionSlot::for_repository_session(&tui.repos[0].identity, &tui.sessions[0]);
        let stale_key = AgentSessionWarmupKey::new(slot.clone(), 0);
        let current_key = AgentSessionWarmupKey::new(slot.clone(), 1);
        tui.tmux_generations.insert(slot, 1);
        tui.tmux_portal_last_polled
            .insert(current_key.clone(), Instant::now());
        tui.tmux_portal_tx
            .send(TmuxPortalResult {
                key: stale_key,
                started_at: Instant::now(),
                capture: Ok(vec![Line::from("stale output")]),
                resized_size: None,
            })
            .unwrap();

        assert!(tui.poll_tmux_portal());
        assert_eq!(
            tui.tmux_portal.as_ref().map(|portal| &portal.key),
            Some(&current_key),
        );
        assert_eq!(
            tui.tmux_portal
                .as_ref()
                .and_then(|portal| portal.capture.as_ref()),
            None,
        );
    }

    #[test]
    fn tmux_portal_starts_capture_immediately_after_selection() {
        let repo = Repository {
            root: PathBuf::from("/tmp/repo"),
        };
        let mut tui = Tui::new_single(
            repo,
            test_config(),
            vec![test_session(0, "/tmp/repo", "feature")],
        );
        tui.focused_panel = PanelFocus::Worktrees;
        tui.tmux_portal_size = Some((72, 18));
        tui.refresh_worktree_harness_configs();
        let slot =
            AgentSessionSlot::for_repository_session(&tui.repos[0].identity, &tui.sessions[0]);
        tui.tmux_generations.insert(slot, 0);

        assert!(tui.poll_tmux_portal());
        assert!(
            !tui.tmux_portal_polls_in_flight.is_empty(),
            "selecting a worktree should immediately start an asynchronous tmux capture"
        );
    }

    #[test]
    fn workflow_database_writer_does_not_block_tmux_portal_polling() {
        let temp = unique_temp_dir("prism-tmux-portal-database-test");
        fs::create_dir_all(&temp).unwrap();
        let tmux = temp.join("tmux");
        fs::write(&tmux, "#!/bin/sh\nexit 1\n").unwrap();
        let mut permissions = fs::metadata(&tmux).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&tmux, permissions).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let mut config = test_config();
        config
            .tools
            .insert("tmux".to_string(), tmux.display().to_string());
        let session = test_session(0, &temp.display().to_string(), "feature");
        crate::session::worktree_harness(&repo, &session).unwrap();
        let mut tui = Tui::new_single(repo.clone(), config, vec![session]);
        tui.focused_panel = PanelFocus::Worktrees;
        tui.tmux_portal_size = Some((72, 18));
        tui.refresh_worktree_harness_configs();
        let slot =
            AgentSessionSlot::for_repository_session(&tui.repos[0].identity, &tui.sessions[0]);
        tui.tmux_generations.insert(slot, 0);
        let blocker = rusqlite::Connection::open(crate::observability::db_path(&repo)).unwrap();
        blocker
            .execute_batch("begin exclusive transaction")
            .unwrap();

        let started = Instant::now();
        tui.tick_tui_action_jobs();
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_millis(250),
            "worktree polling blocked input for {elapsed:?}"
        );

        drop(blocker);
        let _ = tui.tmux_portal_rx.recv_timeout(Duration::from_secs(1));
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn tmux_portal_resizes_once_for_unchanged_target_and_size() {
        let temp = unique_temp_dir("prism-tmux-portal-resize-test");
        fs::create_dir_all(&temp).unwrap();
        let log = temp.join("tmux.log");
        let tmux = temp.join("tmux");
        fs::write(
            &tmux,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {:?}\nif [ \"$1\" = capture-pane ]; then printf 'output\\n'; fi\nexit 0\n",
                log.display().to_string()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&tmux).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&tmux, permissions).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let mut config = test_config();
        config
            .tools
            .insert("tmux".to_string(), tmux.display().to_string());
        let session = test_session(0, &temp.display().to_string(), "feature");
        let mut tui = Tui::new_single(repo, config, vec![session]);
        tui.focused_panel = PanelFocus::Worktrees;
        tui.tmux_portal_size = Some((72, 18));
        tui.refresh_worktree_harness_configs();
        let slot =
            AgentSessionSlot::for_repository_session(&tui.repos[0].identity, &tui.sessions[0]);
        let key = AgentSessionWarmupKey::new(slot.clone(), 0);
        tui.tmux_generations.insert(slot, 0);

        tui.poll_tmux_portal();
        wait_for_tmux_portal_job(&mut tui);
        tui.tmux_portal_last_polled
            .insert(key, Instant::now() - Duration::from_secs(1));
        tui.poll_tmux_portal();
        wait_for_tmux_portal_job(&mut tui);

        let commands = fs::read_to_string(log).unwrap();
        assert_eq!(commands.matches("resize-window").count(), 1);
        assert_eq!(commands.matches("capture-pane").count(), 2);

        let _ = fs::remove_dir_all(temp);
    }

    fn wait_for_tmux_portal_job(tui: &mut Tui) {
        let started = Instant::now();
        while !tui.tmux_portal_polls_in_flight.is_empty() {
            tui.poll_tmux_portal();
            assert!(started.elapsed() < Duration::from_secs(1));
            std::thread::yield_now();
        }
    }

    #[test]
    fn tmux_portal_keeps_previous_capture_while_new_selection_loads() {
        let repo = Repository {
            root: PathBuf::from("/tmp/repo"),
        };
        let mut tui = Tui::new_single(
            repo,
            test_config(),
            vec![
                test_session(0, "/tmp/repo-a", "feature-a"),
                test_session(0, "/tmp/repo-b", "feature-b"),
            ],
        );
        tui.focused_panel = PanelFocus::Worktrees;
        tui.tmux_portal_size = Some((72, 18));
        tui.refresh_worktree_harness_configs();
        let previous_slot =
            AgentSessionSlot::for_repository_session(&tui.repos[0].identity, &tui.sessions[0]);
        let selected_slot =
            AgentSessionSlot::for_repository_session(&tui.repos[0].identity, &tui.sessions[1]);
        tui.tmux_generations.insert(previous_slot.clone(), 0);
        tui.tmux_generations.insert(selected_slot.clone(), 0);
        tui.tmux_portal = Some(TmuxPortalSnapshot {
            key: AgentSessionWarmupKey::new(previous_slot.clone(), 0),
            capture: Some(TmuxPortalCapture {
                key: AgentSessionWarmupKey::new(previous_slot, 0),
                result: Ok(vec![Line::from("previous capture")]),
            }),
        });
        tui.select_worktree(1);

        let model = tui.tmux_portal_model().expect("tmux portal model");
        let crate::view::TmuxPortalState::Ready(lines) = model.state else {
            panic!("previous capture should survive the selection redraw");
        };
        assert_eq!(model.branch, "feature-a");
        assert_eq!(lines, &[Line::from("previous capture")]);

        assert!(tui.poll_tmux_portal());
        assert_eq!(
            tui.tmux_portal.as_ref().map(|portal| &portal.key.slot),
            Some(&selected_slot)
        );
        assert_eq!(
            tui.tmux_portal
                .as_ref()
                .and_then(|portal| portal.capture.as_ref())
                .and_then(|capture| capture.result.as_ref().ok()),
            Some(&vec![Line::from("previous capture")])
        );
        assert!(
            tui.tmux_portal_polls_in_flight
                .contains_key(&AgentSessionWarmupKey::new(selected_slot, 0))
        );
    }

    #[test]
    fn tmux_portal_waits_for_running_capture_after_selection_changes() {
        let repo = Repository {
            root: PathBuf::from("/tmp/repo"),
        };
        let mut tui = Tui::new_single(
            repo,
            test_config(),
            vec![
                test_session(0, "/tmp/repo-a", "feature-a"),
                test_session(0, "/tmp/repo-b", "feature-b"),
            ],
        );
        tui.focused_panel = PanelFocus::Worktrees;
        tui.tmux_portal_size = Some((72, 18));
        tui.refresh_worktree_harness_configs();
        let first_slot =
            AgentSessionSlot::for_repository_session(&tui.repos[0].identity, &tui.sessions[0]);
        let second_slot =
            AgentSessionSlot::for_repository_session(&tui.repos[0].identity, &tui.sessions[1]);
        let first_key = AgentSessionWarmupKey::new(first_slot.clone(), 0);
        let second_key = AgentSessionWarmupKey::new(second_slot.clone(), 0);
        tui.tmux_generations.insert(first_slot, 0);
        tui.tmux_generations.insert(second_slot, 0);
        tui.tmux_portal_polls_in_flight
            .insert(first_key.clone(), Instant::now());
        tui.select_worktree(1);

        tui.poll_tmux_portal();

        assert!(tui.tmux_portal_polls_in_flight.contains_key(&first_key));
        assert!(!tui.tmux_portal_polls_in_flight.contains_key(&second_key));
    }

    #[test]
    fn tmux_portal_tracks_in_flight_capture_when_inactive() {
        let repo = Repository {
            root: PathBuf::from("/tmp/repo"),
        };
        let mut tui = Tui::new_single(
            repo,
            test_config(),
            vec![test_session(0, "/tmp/repo", "feature")],
        );
        let slot =
            AgentSessionSlot::for_repository_session(&tui.repos[0].identity, &tui.sessions[0]);
        let key = AgentSessionWarmupKey::new(slot, 0);
        tui.tmux_portal_polls_in_flight
            .insert(key.clone(), Instant::now());

        assert!(!tui.poll_tmux_portal());
        assert!(tui.tmux_portal_polls_in_flight.contains_key(&key));
    }

    #[test]
    fn tmux_portal_ignores_superseded_capture_for_same_key() {
        let repo = Repository {
            root: PathBuf::from("/tmp/repo"),
        };
        let mut tui = Tui::new_single(
            repo,
            test_config(),
            vec![test_session(0, "/tmp/repo", "feature")],
        );
        tui.focused_panel = PanelFocus::Worktrees;
        tui.tmux_portal_size = Some((72, 18));
        tui.refresh_worktree_harness_configs();
        let slot =
            AgentSessionSlot::for_repository_session(&tui.repos[0].identity, &tui.sessions[0]);
        let key = AgentSessionWarmupKey::new(slot.clone(), 0);
        tui.tmux_generations.insert(slot, 0);
        let previous_started_at = Instant::now();
        let current_started_at = previous_started_at + Duration::from_millis(1);
        tui.tmux_portal_polls_in_flight
            .insert(key.clone(), current_started_at);
        tui.tmux_portal_last_polled
            .insert(key.clone(), current_started_at);
        tui.tmux_portal_tx
            .send(TmuxPortalResult {
                key: key.clone(),
                started_at: previous_started_at,
                capture: Ok(vec![Line::from("superseded output")]),
                resized_size: None,
            })
            .unwrap();

        assert!(tui.poll_tmux_portal());
        assert_eq!(
            tui.tmux_portal_polls_in_flight.get(&key),
            Some(&current_started_at)
        );
        assert_eq!(
            tui.tmux_portal
                .as_ref()
                .and_then(|portal| portal.capture.as_ref()),
            None
        );
    }

    #[test]
    fn ordered_toggle_groups_enabled_items_before_disabled_items() {
        let mut items = ordered_toggle_items();
        let mut selected = 1;

        toggle_ordered_item(&mut items, &mut selected);

        assert_eq!(selected, 2);
        assert_eq!(
            items
                .iter()
                .map(|item| (item.id.as_str(), item.enabled))
                .collect::<Vec<_>>(),
            vec![("one", true), ("three", false), ("two", false)]
        );

        toggle_ordered_item(&mut items, &mut selected);

        assert_eq!(selected, 1);
        assert_eq!(
            items
                .iter()
                .map(|item| (item.id.as_str(), item.enabled))
                .collect::<Vec<_>>(),
            vec![("one", true), ("two", true), ("three", false)]
        );
    }

    #[test]
    fn ordered_toggle_moves_only_enabled_items() {
        let mut items = ordered_toggle_items();
        let mut selected = 1;

        move_enabled_ordered_item(&mut items, &mut selected, -1);

        assert_eq!(selected, 0);
        assert_eq!(
            items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec!["two", "one", "three"]
        );

        selected = 2;
        move_enabled_ordered_item(&mut items, &mut selected, -1);
        assert_eq!(selected, 2);
    }

    #[test]
    fn recovery_toggle_keeps_stable_order() {
        let mut items = vec![
            OrderedToggleItem {
                id: "first".to_string(),
                label: "First".to_string(),
                enabled: false,
            },
            OrderedToggleItem {
                id: "second".to_string(),
                label: "Second".to_string(),
                enabled: false,
            },
        ];

        toggle_item_in_place(&mut items, 1);

        assert_eq!(items[0].id, "first");
        assert_eq!(items[1].id, "second");
        assert!(!items[0].enabled);
        assert!(items[1].enabled);
    }

    fn ordered_toggle_items() -> Vec<OrderedToggleItem> {
        vec![
            OrderedToggleItem {
                id: "one".to_string(),
                label: "First".to_string(),
                enabled: true,
            },
            OrderedToggleItem {
                id: "two".to_string(),
                label: "Second".to_string(),
                enabled: true,
            },
            OrderedToggleItem {
                id: "three".to_string(),
                label: "Third".to_string(),
                enabled: false,
            },
        ]
    }

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
    fn switching_from_global_to_repo_mode_preserves_selected_worktree() {
        let mut tui = test_tui();
        tui.worktree_list_mode = WorktreeListMode::Global;
        tui.focus_worktrees();
        tui.select_worktree(1);
        tui.select_repo(1);
        tui.sessions[3].hidden = true;

        tui.switch_worktree_list_mode(WorktreeListMode::Repo);

        assert_eq!(tui.current_repo, 0);
        assert_eq!(tui.selected_worktree_index(), Some(1));
    }

    #[test]
    fn persisted_worktree_list_mode_loads_and_updates_on_switch() {
        let temp = unique_temp_dir("prism-tui-ui-state-test");
        let path = temp.join("ui-state.toml");
        crate::ui_state::save_to_path(&path, WorktreeListMode::Global).unwrap();
        let mut tui = test_tui();

        tui.use_persisted_ui_state(path.clone()).unwrap();

        assert_eq!(tui.worktree_list_mode, WorktreeListMode::Global);

        tui.focus_worktrees();
        tui.switch_worktree_list_mode(WorktreeListMode::Repo);

        assert_eq!(tui.worktree_list_mode, WorktreeListMode::Repo);
        assert_eq!(
            crate::ui_state::load_from_path(&path).unwrap(),
            Some(WorktreeListMode::Repo)
        );

        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn invalid_persisted_ui_state_keeps_current_mode_and_reports_error() {
        let temp = unique_temp_dir("prism-tui-invalid-ui-state-test");
        fs::create_dir_all(&temp).unwrap();
        let path = temp.join("ui-state.toml");
        fs::write(&path, "worktree_list_mode = 42\n").unwrap();
        let mut tui = Tui::new(Vec::new(), 0, Vec::new());

        tui.use_persisted_ui_state(path.clone()).unwrap();

        assert_eq!(tui.worktree_list_mode, WorktreeListMode::Repo);
        assert!(
            tui.status_message
                .as_deref()
                .is_some_and(|message| message.contains(&path.display().to_string()))
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "worktree_list_mode = 42\n"
        );
        fs::remove_dir_all(temp).unwrap();
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
        assert_eq!(tui.repo_main_view, RepoMainView::ChangeRequests);

        tui.focus_main();
        tui.move_right();

        assert_eq!(tui.focused_panel, PanelFocus::Repos);
        assert_eq!(tui.repo_main_view, RepoMainView::Kanban);

        tui.move_left();

        assert_eq!(tui.focused_panel, PanelFocus::Repos);
        assert_eq!(tui.repo_main_view, RepoMainView::ChangeRequests);

        tui.focused_panel = PanelFocus::Worktrees;
        tui.main_focused = false;
        tui.move_left();

        assert_eq!(tui.focused_panel, PanelFocus::Worktrees);
        assert_eq!(tui.repo_main_view, RepoMainView::ChangeRequests);
    }

    #[test]
    fn main_panel_scrolls_when_pr_comments_are_selectable() {
        let mut tui = test_tui();
        tui.focus_worktrees();
        tui.select_worktree(1);
        tui.sessions[1].pr = PrCache::observed(
            test_pr_summary(false),
            Some(crate::github::PrDetails {
                comments: vec![
                    crate::github::PrComment {
                        body: "first comment".to_string(),
                        ..crate::github::PrComment::default()
                    },
                    crate::github::PrComment {
                        body: "second comment".to_string(),
                        ..crate::github::PrComment::default()
                    },
                ],
                ..crate::github::PrDetails::default()
            }),
        );
        tui.focus_main();

        tui.move_down();

        assert_eq!(tui.main_scroll, 1);
        assert_eq!(tui.selected_comment, 1);

        tui.move_up();

        assert_eq!(tui.main_scroll, 0);
        assert_eq!(tui.selected_comment, 0);
    }

    #[test]
    fn sidebar_navigation_leaves_main_focus() {
        let mut tui = test_tui();
        tui.focus_main();

        tui.focus_repos();
        assert!(!tui.main_focused);
        assert_eq!(tui.focused_panel, PanelFocus::Repos);

        tui.focus_main();
        tui.focus_next_panel();
        assert!(!tui.main_focused);
        assert_eq!(tui.focused_panel, PanelFocus::Worktrees);
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
                harness_id: "opencode".to_string(),
                adapter_id: "opencode".to_string(),
                id: id.to_string(),
                repo_root: "/repo-one".to_string(),
                worktree_path: PathBuf::from(worktree_path),
                worktree_incarnation: None,
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
                harness_id: "opencode".to_string(),
                adapter_id: "opencode".to_string(),
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
                execution: crate::harness::ExecutionRef::default(),
                session: crate::harness::SessionRef::default(),
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

    #[test]
    fn pr_poll_identity_uses_repository_and_worktree_generation_not_repo_order() {
        let repository = WorktreeRepositoryKey::new(PathBuf::from("/tmp/repo"));
        let mut session = test_session(0, "/tmp/repo", "feature");
        let first = PrPollKey::for_repository_session_generation(&repository, &session, 3);

        session.repo_index = 9;
        let reordered = PrPollKey::for_repository_session_generation(&repository, &session, 3);
        let recreated = PrPollKey::for_repository_session_generation(&repository, &session, 4);

        assert_eq!(first, reordered);
        assert_ne!(first, recreated);
    }

    fn test_session(repo_index: usize, root: &str, branch: &str) -> Session {
        let path = PathBuf::from(format!("{root}/{branch}"));
        let _ = fs::create_dir_all(&path);
        Session {
            repo_index,
            repo_label: format!("repo-{repo_index}"),
            repo_key: None,
            path: path.clone(),
            incarnation: String::new(),
            path_display: path.display().to_string(),
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
        let mut config = crate::test_support::test_config();
        config.default_agent = "opencode".to_string();
        config.default_base = Some("main".to_string());
        config
    }

    fn test_pr_summary(merged: bool) -> PrSummary {
        PrSummary {
            number: 1,
            change_request_identity: None,
            title: "PR".to_string(),
            author: "author".to_string(),
            body: String::new(),
            url: "https://example.test/pr/1".to_string(),
            state: if merged { "MERGED" } else { "OPEN" }.to_string(),
            review_decision: String::new(),
            requested_reviewers: Vec::new(),
            head_ref: "feature".to_string(),
            base_ref: "main".to_string(),
            head_sha: "abc123".to_string(),
            updated_at: String::new(),
            check_status: String::new(),
            merge_state_status: String::new(),
            comment_count: 0,
            merged,
            draft: false,
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
