use std::collections::BTreeMap;
use std::time::Instant;

use ratatui::text::Line;

use crate::agent_session::{AgentSessionSlot, AgentSessionWarmupKey};
use crate::auto_flow::{AutoOutputLine, PersistedAutoRun};
use crate::config::Config;
use crate::opencode::{OpencodeEvent, OpencodeStatus};
use crate::plan_run::{PersistedPlanRun, PlanOutputLine};
use crate::remote::{PrCache, PrSummary};
use crate::repo::Repository;
use crate::session::{Session, WorktreeRepositoryKey, WorktreeSessionKey};
use crate::workspace_state::RepositorySnapshot;

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
        capabilities: Option<crate::remote::Capabilities>,
        summaries: Result<Vec<PrSummary>, String>,
        observations: Result<Vec<PrSummarySessionResult>, String>,
        remote_branch_heads: BTreeMap<(String, String), String>,
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
        remote_update: bool,
        status_label: Option<String>,
        auto_run: Result<Option<Box<PersistedAutoRun>>, String>,
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
    pub(crate) remote_update: bool,
    pub(crate) session: Session,
    pub(crate) config: Config,
    pub(crate) auto_run_id: Option<String>,
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
    pub(super) repository: RepositorySnapshot,
    pub(super) generalized_runs: Result<Vec<crate::run::RunSummary>, String>,
    pub(super) generalized_detail: Result<Option<Box<crate::run::RunProjection>>, String>,
    pub(super) plan_runs: Result<Vec<PersistedPlanRun>, String>,
    pub(super) auto_runs: Result<Vec<PersistedAutoRun>, String>,
    pub(super) linked_plan_runs: Result<Vec<PersistedPlanRun>, String>,
    pub(super) worker_health: Result<(), String>,
}

pub(crate) struct WorkflowPollResult {
    pub(super) repository: WorktreeRepositoryKey,
    pub(super) revision: u64,
    pub(super) snapshot: Result<WorkflowPollSnapshot, String>,
}

pub(crate) enum DashboardOutputLines {
    Plan(Vec<PlanOutputLine>),
    Auto(Vec<AutoOutputLine>),
}

pub(crate) struct DashboardOutputResult {
    pub(super) key: DashboardOutputKey,
    pub(super) revision: u64,
    pub(super) lines: Result<DashboardOutputLines, String>,
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

pub(crate) struct WtPollResult {
    pub repository: WorktreeRepositoryKey,
    pub observation: Result<WtObservation, crate::worktrunk::WorktrunkFailure>,
}

pub(crate) struct WtObservation {
    pub snapshot: crate::worktrunk::WorktrunkSnapshot,
    pub facts: BTreeMap<WorktreeSessionKey, crate::worktrunk::WorktrunkWorktreeFacts>,
    pub observed_at: Instant,
}

pub(crate) struct WtHookLogPollResult {
    pub repository: WorktreeRepositoryKey,
    pub observation: Result<WtHookLogObservation, crate::worktrunk::WorktrunkFailure>,
}

pub(crate) struct WtHookLogObservation {
    pub entries: Vec<crate::worktrunk::HookLogEntry>,
    pub observed_at: Instant,
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

pub(super) fn pr_delivery_key(result: &PrPollResult) -> PrDeliveryKey {
    match result {
        PrPollResult::Summary { repository, .. } => PrDeliveryKey::Summary(repository.clone()),
        PrPollResult::Details { key, .. } => PrDeliveryKey::Details(key.clone()),
        PrPollResult::Persistence { key, .. } => PrDeliveryKey::Persistence(key.clone()),
    }
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
