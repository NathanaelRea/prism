use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::agent_session::{AgentSessionSlot, AgentSessionWarmupKey, AgentSessionWarmupResult};
use crate::config::Config;
#[cfg(test)]
use crate::desktop_notification::DesktopNotifier;
use crate::input::{Key, KeyInput};
use crate::repo::Repository;
use crate::session::{Session, WorktreeRepositoryKey, WorktreeSessionKey};
use crate::terminal::stdin_is_tty;
use crate::tmux::TmuxWindow;
use crate::tui_jobs::{JobId, JobRegistry, LatestReceiver, LatestSender, latest_channel};
use crate::tui_runtime::{RuntimeEvent, TerminalRuntime};
use crate::tui_signal::{ShutdownNotification, ShutdownSignal};
use crate::view;
use crate::workspace_state::RepositorySnapshot;

mod agent_state;
mod attach;
mod dialog;
mod git_actions;
pub(crate) mod input;
mod job_orchestration;
mod job_protocol;
pub(crate) mod jobs;
mod navigation;
mod operator;
mod presentation;
mod remote_action;
mod repository;
pub(crate) mod runtime;
pub(crate) mod signal;
pub(crate) mod state;
mod workflow;

#[cfg(test)]
mod tests;

use agent_state::AgentStatePersistenceRequest;
use dialog::{choice_list, ctrl_key};
#[cfg(test)]
use dialog::{
    confirmation_result, create_session_fields, create_session_submit_key,
    move_enabled_ordered_item, selectable_choice_key, toggle_item_in_place, toggle_ordered_item,
    update_create_session_variant_field,
};
pub(crate) use git_actions::GitAction;
use git_actions::{
    GitActionExecution, git_action_error_title, git_action_execution, git_action_for_key,
};
use job_orchestration::ShutdownReason;
pub(crate) use job_orchestration::{TuiJobKey, TuiJobKind, TuiJobPayload};
use job_protocol::pr_delivery_key;
pub(crate) use job_protocol::{
    DefaultBranchPollResult, DeleteSessionKey, DeleteSessionResult, OpencodeEventResult,
    OpencodeListenerKey, OpencodePollKey, OpencodePollResult, PrDeliveryKey, PrPersistenceRequest,
    PrPollKey, PrPollResult, PrSummarySessionResult, SessionRefreshResult, SessionRefreshSnapshot,
    TmuxPortalCapture, TmuxPortalResult, TmuxPortalSnapshot, TmuxPortalTarget, WorkflowPollResult,
    WorkflowPollSnapshot, WtHookLogObservation, WtHookLogPollResult, WtObservation, WtPollResult,
};
#[allow(unused_imports)]
pub(crate) use navigation::{NavigationSnapshot, PanelFocus, WorktreeListMode};
use navigation::{OpenTmuxSessionTarget, worktree_updated_label};
pub(crate) use remote_action::{
    RemoteActionDelivery, RemoteActionRequest, RemoteActionValue, RemoteMutationTarget,
};
use remote_action::{
    RemoteActionReconciliationContext, RemoteMutationReconciliationMarker,
    uncertain_remote_mutation_error,
};
#[cfg(test)]
use remote_action::{
    remote_action_abandon_requested, remote_action_timeout, remote_mutation_targets_overlap,
};
#[allow(unused_imports)]
pub(crate) use repository::{
    ManagedRepo, SelectedRepoContext, SelectedWorktreeContext, WtHookLogInventory,
    load_worktree_harness_configs, maintain_workflow_storage,
};

pub struct Tui {
    pub(crate) repo: Repository,
    pub(crate) config: Config,
    pub(crate) repos: Vec<ManagedRepo>,
    pub(crate) current_repo: usize,
    pub(crate) sessions: Vec<Session>,
    #[cfg(test)]
    pub(crate) desktop_notifier: DesktopNotifier,
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
    pub(crate) selected_workflow_step: Option<String>,
    pub(crate) workflow_step_selection_manual: bool,
    pub(crate) worktree_list_mode: WorktreeListMode,
    ui_state_path: Option<PathBuf>,
    pub(crate) selected_comment: usize,
    pub(crate) selected_worktree_by_repo: BTreeMap<PathBuf, PathBuf>,
    pub(crate) selected_pr_by_repo:
        BTreeMap<PathBuf, crate::remote::CanonicalChangeRequestIdentity>,
    pub(crate) pr_poll_tx: LatestSender<PrDeliveryKey, PrPollResult>,
    pub(crate) pr_poll_rx: LatestReceiver<PrDeliveryKey, PrPollResult>,
    pub(crate) pr_polls_in_flight: BTreeSet<PrPollKey>,
    pub(crate) pr_persistence_in_flight: BTreeSet<PrPollKey>,
    pub(crate) pr_persistence_pending: BTreeMap<PrPollKey, PrPersistenceRequest>,
    pub(crate) pr_persistence_versions: BTreeMap<PrPollKey, u64>,
    remote_action_tx: LatestSender<JobId, RemoteActionDelivery>,
    remote_action_rx: LatestReceiver<JobId, RemoteActionDelivery>,
    remote_action_failures: BTreeMap<JobId, String>,
    remote_actions_requiring_reconciliation: BTreeSet<JobId>,
    remote_action_reconciliation_contexts: BTreeMap<JobId, RemoteActionReconciliationContext>,
    remote_mutations_requiring_reconciliation:
        BTreeMap<PathBuf, Vec<RemoteMutationReconciliationMarker>>,
    shutdown_remote_action_errors: Vec<String>,
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
    pub(crate) wt_hook_log_poll_tx: LatestSender<WorktreeRepositoryKey, WtHookLogPollResult>,
    pub(crate) wt_hook_log_poll_rx: LatestReceiver<WorktreeRepositoryKey, WtHookLogPollResult>,
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
    workflow_poll_tx: LatestSender<WorktreeRepositoryKey, WorkflowPollResult>,
    workflow_poll_rx: LatestReceiver<WorktreeRepositoryKey, WorkflowPollResult>,
    workflow_polls_in_flight: BTreeSet<WorktreeRepositoryKey>,
    workflow_last_polled: BTreeMap<WorktreeRepositoryKey, Instant>,
    workflow_revision: u64,
    worker_health: Option<Result<(), String>>,
    workspace_repositories: BTreeMap<WorktreeRepositoryKey, RepositorySnapshot>,
    selected_workflow_run: Option<String>,
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
const TUI_MUTATION_SHUTDOWN_BOUND: Duration = Duration::from_secs(30 * 60);
const REMOTE_MUTATION_RECONCILIATION_KEY: &str = "tui.remote_mutation_reconciliation_required";
const TUI_TICK_ITEM_BUDGET: usize = 32;
const TUI_TICK_TIME_BUDGET: Duration = Duration::from_millis(8);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LeaderHint {
    Root,
    Git,
    Workflow,
}

#[derive(Default)]
pub(crate) struct TuiBackgroundChanges {
    sessions: bool,
    tmux: bool,
    tmux_portal: bool,
    worktree_columns: bool,
    worktrunk_hook_logs: bool,
    default_branch: bool,
    opencode_status: bool,
    opencode_events: bool,
    workflows: bool,
    pull_requests: bool,
    delete_sessions: bool,
    status_message: bool,
}

impl TuiBackgroundChanges {
    pub(crate) fn any(&self) -> bool {
        self.tmux
            || self.sessions
            || self.tmux_portal
            || self.worktree_columns
            || self.worktrunk_hook_logs
            || self.default_branch
            || self.opencode_status
            || self.opencode_events
            || self.workflows
            || self.pull_requests
            || self.delete_sessions
            || self.status_message
    }
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

impl Tui {
    pub fn new(repos: Vec<ManagedRepo>, current_repo: usize, sessions: Vec<Session>) -> Self {
        let (pr_poll_tx, pr_poll_rx) = latest_channel(pr_delivery_key);
        let (remote_action_tx, remote_action_rx) =
            latest_channel(|result: &RemoteActionDelivery| result.id);
        let (workflow_poll_tx, workflow_poll_rx) =
            latest_channel(|result: &WorkflowPollResult| result.repository.clone());
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
        let (wt_hook_log_poll_tx, wt_hook_log_poll_rx) =
            latest_channel(|result: &WtHookLogPollResult| result.repository.clone());
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
            #[cfg(test)]
            desktop_notifier: DesktopNotifier::new(),
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
            selected_workflow_step: None,
            workflow_step_selection_manual: false,
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
            remote_action_tx,
            remote_action_rx,
            remote_action_failures: BTreeMap::new(),
            remote_actions_requiring_reconciliation: BTreeSet::new(),
            remote_action_reconciliation_contexts: BTreeMap::new(),
            remote_mutations_requiring_reconciliation: BTreeMap::new(),
            shutdown_remote_action_errors: Vec::new(),
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
            wt_hook_log_poll_tx,
            wt_hook_log_poll_rx,
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
            workflow_poll_tx,
            workflow_poll_rx,
            workflow_polls_in_flight: BTreeSet::new(),
            workflow_last_polled: BTreeMap::new(),
            workflow_revision: 0,
            worker_health: None,
            workspace_repositories: BTreeMap::new(),
            selected_workflow_run: None,
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
        tui.load_remote_mutation_reconciliation_markers();
        tui.ensure_navigation_valid();
        #[cfg(test)]
        tui.reseed_desktop_notifications();
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
        #[cfg(target_os = "macos")]
        let _notification_subscription = crate::worker::subscribe_notifications()?;
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
                    let area = runtime.area()?;
                    if self.handle_mouse_event(event, area) {
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
                    self.start_wt_column_poll();
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
                    self.select_adjacent_workflow(-1);
                    pending_g = false;
                }
                Key::NextBlock => {
                    self.clear_leader_hint();
                    self.select_adjacent_workflow(1);
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
                Key::LeaderWorkflow => {
                    self.leader_hint = Some(LeaderHint::Workflow);
                }
                Key::OpenTmuxSession => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if self.handle_workflow_enter() {
                        self.draw(runtime)?;
                        continue;
                    }
                    if self.open_selected_comment_dialog(runtime)? {
                        self.draw(runtime)?;
                        continue;
                    }
                    match self.open_tmux_session_target() {
                        OpenTmuxSessionTarget::RepoDefaultAgent(index) => {
                            self.enter_agent_mode_for_index(runtime, index)?
                        }
                        OpenTmuxSessionTarget::WorktreeAgent => self.enter_agent_mode(runtime)?,
                        OpenTmuxSessionTarget::RepoPr => {
                            self.open_selected_repo_pr_agent(runtime)?
                        }
                        OpenTmuxSessionTarget::Blocked(message) => self.show_message(message)?,
                    }
                }
                Key::WorkflowLauncher => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if let Err(error) = self.launch_workflow(runtime) {
                        self.show_error("workflow launcher failed", &error)?;
                    }
                }
                Key::WorkflowAi => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if let Err(error) = self.create_ai_workflow(runtime) {
                        self.show_error("AI Workflow creation failed", &error)?;
                    }
                }
                Key::WorkflowPauseResume => {
                    if let Err(error) = self.control_selected_workflow(runtime, "toggle") {
                        self.show_error("Workflow control failed", &error)?;
                    }
                }
                Key::WorkflowRetry => {
                    if let Err(error) = self.control_selected_workflow(runtime, "retry") {
                        self.show_error("Workflow retry failed", &error)?;
                    }
                }
                Key::Configuration => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if let Err(error) = self.show_configuration_tree(runtime) {
                        self.show_error("configuration failed", &error)?;
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
                Key::OpenPr => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if self.git_action_enabled(GitAction::OpenPr)
                        && let Err(error) = self.open_selected_pr(runtime)
                    {
                        self.show_error("open PR failed", &error)?;
                    }
                }
                Key::OpenDevelopmentUrl => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if let Err(error) = self.open_selected_development_url() {
                        self.show_error("open development URL failed", &error)?;
                    }
                }
                Key::WorktrunkLogs => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if let Err(error) = self.show_selected_worktrunk_logs(runtime) {
                        self.show_error("Worktrunk hook logs failed", &error)?;
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
                        self.start_wt_column_poll();
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
                Key::Push => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if self.git_action_enabled(GitAction::Push)
                        && let Err(error) = self.push_selected_branch(runtime)
                    {
                        self.show_error("push failed", &error)?;
                    }
                }
                Key::Merge | Key::CiFix | Key::ReviewFix => {
                    self.clear_leader_hint();
                    pending_g = false;
                    let action = git_action_for_key(key).expect("matched Git action key");
                    if self.git_action_enabled(action) {
                        let result = match git_action_execution(action) {
                            GitActionExecution::ProviderMerge => {
                                self.merge_selected_change_request(runtime)
                            }
                            GitActionExecution::Stabilize => {
                                self.launch_stabilization_workflow(runtime)
                            }
                        };
                        if let Err(error) = result {
                            self.show_error(git_action_error_title(action), &error)?;
                        }
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
                Key::PullDefault => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if self.focused_panel != PanelFocus::Repos {
                        self.show_message("focus repos to pull the default branch")?;
                    } else if let Err(error) = self.pull_default_branch(runtime) {
                        self.show_error("pull failed", &error)?;
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
                    match self.control_selected_workflow(runtime, "cancel") {
                        Ok(true) => {}
                        Ok(false) if self.focused_panel != PanelFocus::Worktrees => {
                            self.show_message("focus worktrees to abort an agent session")?;
                        }
                        Ok(false) => {
                            if let Err(error) = self.abort_selected_opencode_session(runtime) {
                                self.show_error("abort failed", &error)?;
                            }
                        }
                        Err(error) => self.show_error("Workflow control failed", &error)?,
                    }
                }
                Key::OpenRemotePrs => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if self.focused_panel != PanelFocus::Repos {
                        self.show_message("focus repos to open a remote PR worktree")?;
                    } else if self.selected_repo_list_support()
                        != Some(crate::remote::SupportLevel::Supported)
                    {
                        self.show_message("remote PR listing is unavailable for this provider")?;
                    } else if let Err(error) = self.open_remote_pr_worktree(runtime) {
                        self.show_error("open remote PR worktree failed", &error)?;
                    }
                }
                Key::Delete => {
                    self.clear_leader_hint();
                    pending_g = false;
                    if self.focused_panel == PanelFocus::Status {
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
                    if self.focused_panel != PanelFocus::Worktrees {
                        self.show_message(
                            "focus worktrees to permanently delete a worktree/session",
                        )?;
                    } else if let Err(error) = self.delete_session(runtime) {
                        self.show_error("delete failed", &error)?;
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
}
