use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::agent_session::{AgentSessionWarmupKey, AgentSessionWarmupResult};
use crate::remote::PrCache;
use crate::session::{WorktreeRepositoryKey, WorktreeSessionKey};
use crate::tui_jobs::{JobContext, JobId, JobMessage, JobMetadata, JobOutcome};

use super::{
    DefaultBranchPollResult, DeferredMergeCleanupResult, DeleteSessionKey, DeleteSessionResult,
    OpencodeEventResult, OpencodeListenerKey, OpencodePollKey, OpencodePollResult, PrPollKey,
    PrPollResult, RemoteActionDelivery, RemoteActionValue, SessionRefreshResult,
    TUI_JOB_SHUTDOWN_GRACE, TUI_MUTATION_SHUTDOWN_BOUND, TUI_TICK_ITEM_BUDGET,
    TUI_TICK_TIME_BUDGET, TmuxPortalResult, Tui, TuiBackgroundChanges, WorkflowPollResult,
    WtHookLogPollResult, WtPollResult, uncertain_remote_mutation_error,
};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum TuiJobKind {
    SessionRefresh,
    AgentStatePersistence,
    PrSummary,
    PrDetails,
    PrPersistence,
    DeferredMergeCleanup,
    WorkflowPoll,
    DeleteSession,
    TmuxWarmup,
    TmuxPortal,
    WorktreeColumns,
    WorktrunkHookLogs,
    DefaultBranch,
    OpencodePoll,
    OpencodeListener,
    RemoteAction,
    RemoteReconciliation,
}

impl TuiJobKind {
    pub(super) const fn label(&self) -> &'static str {
        match self {
            Self::SessionRefresh => "session_refresh",
            Self::AgentStatePersistence => "agent_state_persistence",
            Self::PrSummary => "pr_summary",
            Self::PrDetails => "pr_details",
            Self::PrPersistence => "pr_persistence",
            Self::DeferredMergeCleanup => "deferred_merge_cleanup",
            Self::WorkflowPoll => "workflow_poll",
            Self::DeleteSession => "delete_session",
            Self::TmuxWarmup => "tmux_warmup",
            Self::TmuxPortal => "tmux_portal",
            Self::WorktreeColumns => "worktree_columns",
            Self::WorktrunkHookLogs => "worktrunk_hook_logs",
            Self::DefaultBranch => "default_branch",
            Self::OpencodePoll => "opencode_poll",
            Self::OpencodeListener => "opencode_listener",
            Self::RemoteAction => "remote_action",
            Self::RemoteReconciliation => "remote_reconciliation",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum TuiJobKey {
    None,
    /// Process-lifetime work whose result must not be invalidated by session generations.
    System,
    Repository(WorktreeRepositoryKey),
    WorktrunkHookLogs(WorktreeRepositoryKey),
    WorkflowRepository(WorktreeRepositoryKey),
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
pub(super) enum ShutdownReason {
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
    PrPoll(PrPollResult),
    DeferredMergeCleanup(DeferredMergeCleanupResult),
    WorkflowPoll(WorkflowPollResult),
    DeleteSession(DeleteSessionResult),
    TmuxWarmup(AgentSessionWarmupResult),
    TmuxPortal(TmuxPortalResult),
    WorktreeColumns(WtPollResult),
    WorktrunkHookLogs(WtHookLogPollResult),
    DefaultBranch(DefaultBranchPollResult),
    OpencodePoll(OpencodePollResult),
    OpencodeEvent(OpencodeEventResult),
    RemoteAction(Box<RemoteActionDelivery>),
    RemoteReconciliation(super::remote_reconciliation::RemoteReconciliationResult),
    RemoteMarkersLoaded(super::remote_action::LoadedRemoteMutationMarkers),
    RemoteActionProgress { id: JobId, message: String },
}

impl Tui {
    pub(crate) fn tick_tui_action_jobs(&mut self) -> TuiBackgroundChanges {
        let started = Instant::now();
        self.tui_tick_active = true;
        let routed = self.route_tui_job_messages();
        let changes = TuiBackgroundChanges {
            sessions: self.poll_session_refresh(),
            tmux: self.poll_tmux_agent_warmup(),
            tmux_portal: self.poll_tmux_portal(),
            worktree_columns: self.poll_wt_columns(),
            worktrunk_hook_logs: self.poll_wt_hook_logs(),
            default_branch: self.poll_default_branch_status(),
            opencode_status: self.poll_opencode_status(),
            opencode_events: self.poll_opencode_events(),
            workflows: self.poll_workflow_runs(),
            pull_requests: self.poll_pull_requests(false),
            delete_sessions: self.poll_delete_sessions(),
            status_message: self.expire_status_message(),
        };
        self.start_scheduled_wt_polls();
        self.start_pending_wt_hook_log_refreshes();
        self.start_default_branch_status_poll(false);
        self.start_opencode_status_poll(false);
        self.start_opencode_event_listeners();
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

    pub(super) fn confirm_quit(&mut self) -> Result<bool, String> {
        if !self.delete_sessions_in_flight.is_empty() {
            self.show_message("delete in progress; wait for it to finish before quitting")?;
            return Ok(false);
        }
        Ok(true)
    }

    pub(crate) fn spawn_tui_job<F, Fut>(
        &mut self,
        kind: TuiJobKind,
        key: TuiJobKey,
        generation: u64,
        timeout: Option<Duration>,
        name: String,
        job: F,
    ) -> JobId
    where
        F: FnOnce(JobContext<TuiJobKind, TuiJobKey, TuiJobPayload>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<Option<TuiJobPayload>, String>> + Send + 'static,
    {
        self.background
            .spawn(kind, key, generation, timeout, name, job)
    }

    pub(super) fn repository_key_for_job_key<'a>(
        &self,
        key: &'a TuiJobKey,
    ) -> Option<&'a WorktreeRepositoryKey> {
        match key {
            TuiJobKey::Repository(repository)
            | TuiJobKey::WorktrunkHookLogs(repository)
            | TuiJobKey::WorkflowRepository(repository) => Some(repository),
            TuiJobKey::Worktree(worktree) | TuiJobKey::AgentStatePersistence(worktree) => {
                Some(&worktree.repository)
            }
            TuiJobKey::Pr(key) | TuiJobKey::PrPersistence(key) => Some(&key.worktree.repository),
            TuiJobKey::Delete(key) => Some(&key.worktree.repository),
            TuiJobKey::Tmux(key) => Some(&key.slot.worktree.repository),
            TuiJobKey::Opencode(key) => Some(&key.worktree.repository),
            TuiJobKey::OpencodeListener(key) => Some(&key.worktree.repository),
            TuiJobKey::None | TuiJobKey::System => None,
        }
    }

    pub(super) fn repository_root_for_job_key(&self, key: &TuiJobKey) -> Option<PathBuf> {
        self.repository_key_for_job_key(key)
            .map(|repository| repository.root.clone())
    }
    pub(crate) fn route_tui_job_messages(&mut self) -> usize {
        if !self.background.begin_routing() {
            return 0;
        }
        let deadline = Instant::now() + TUI_TICK_TIME_BUDGET;
        for result in self.background.drain_marker_persistence_results() {
            if let Err(error) = result.result {
                self.background.record_remote_failure(result.key.2, error);
            }
        }
        let processed = self.route_tui_job_messages_with_budget(TUI_TICK_ITEM_BUDGET, deadline);
        self.background.finish_routing();
        processed
    }

    pub(super) fn route_tui_job_messages_with_budget(
        &mut self,
        limit: usize,
        deadline: Instant,
    ) -> usize {
        for metadata in self.background.active_metadata() {
            if !self.job_generation_is_current(&metadata)
                && !self.background.remote_action_is_tracked(metadata.id)
            {
                self.background.cancel(metadata.id);
            }
        }
        let mut processed = 0;
        let mut restart_session_refresh = false;
        while processed < limit && (processed == 0 || Instant::now() < deadline) {
            let Some(message) = self.background.drain_terminals(1).into_iter().next() else {
                break;
            };
            let JobMessage::Terminal { metadata, outcome } = message else {
                unreachable!();
            };
            processed += 1;
            // Some completed jobs apply their state change from the payload. Keep their poll slot
            // owned until that payload is applied: under a spent routing budget the terminal can
            // be observed one tick before the coalesced payload.
            let state_payload_pending = matches!(
                metadata.kind,
                TuiJobKind::DeferredMergeCleanup
                    | TuiJobKind::DeleteSession
                    | TuiJobKind::TmuxPortal
            ) && matches!(&outcome, JobOutcome::Completed)
                && self.job_generation_is_current(&metadata);
            if !state_payload_pending {
                self.clear_tui_job_in_flight(&metadata);
            }
            self.record_tui_job_terminal(&metadata, &outcome);
            let delete_needs_recovery_refresh = metadata.kind == TuiJobKind::DeleteSession
                && !matches!(outcome, JobOutcome::Completed);
            if metadata.kind == TuiJobKind::RemoteAction
                && !matches!(outcome, JobOutcome::Completed)
            {
                let error = outcome.error_message().unwrap_or_else(|| {
                    if matches!(outcome, JobOutcome::Canceled) {
                        "remote action canceled".to_string()
                    } else if matches!(outcome, JobOutcome::DeadlineExceeded) {
                        "remote action timed out".to_string()
                    } else {
                        "remote action failed".to_string()
                    }
                });
                self.background
                    .record_remote_failure(metadata.id, error.clone());
                if self.background.remote_action_is_tracked(metadata.id)
                    && !matches!(outcome, JobOutcome::SpawnFailed(_))
                    && let Some(reconciliation) = self.background.remote_context(metadata.id)
                    && let Err(marker_error) = self.record_remote_mutation_reconciliation(
                        &reconciliation.key,
                        metadata.id,
                        &error,
                        &reconciliation.target,
                        Some(&reconciliation.ledger),
                    )
                {
                    self.background
                        .record_remote_failure(metadata.id, format!("{error}; {marker_error}"));
                    self.background.push_shutdown_error(marker_error);
                }
                self.background.finish_remote_action(metadata.id);
            }
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
            if delete_needs_recovery_refresh && let TuiJobKey::Delete(key) = &metadata.key {
                if let Some(repo_index) = self
                    .repos
                    .iter()
                    .position(|repo| repo.identity == key.worktree.repository)
                {
                    self.request_worktrunk_refreshes(repo_index);
                }
                let _ = self.refresh_sessions_after_tmux();
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
                    | TuiJobKey::System
                    | TuiJobKey::Repository(_)
                    | TuiJobKey::WorktrunkHookLogs(_)
                    | TuiJobKey::WorkflowRepository(_)
                    | TuiJobKey::Delete(_) => false,
                });
            if selected_job { 1 } else { 3 }
        };

        while processed < limit
            && Instant::now() < deadline
            && self
                .background
                .latest_min_priority(priority)
                .is_some_and(|value| value <= 1)
        {
            let Some(JobMessage::Payload { metadata, payload }) =
                self.background.take_latest_by(priority)
            else {
                break;
            };
            processed += 1;
            if self.job_generation_is_current(&metadata)
                || self.background.remote_action_is_tracked(metadata.id)
            {
                self.route_tui_job_payload_for_metadata(&metadata, payload);
            } else {
                self.clear_tui_job_in_flight(&metadata);
            }
        }

        while processed < limit && Instant::now() < deadline {
            let Some(message) = self.background.take_stream_event() else {
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
                self.background.take_latest_by(priority)
            else {
                break;
            };
            processed += 1;
            if self.job_generation_is_current(&metadata)
                || self.background.remote_action_is_tracked(metadata.id)
            {
                self.route_tui_job_payload_for_metadata(&metadata, payload);
            } else {
                self.clear_tui_job_in_flight(&metadata);
            }
        }

        for metadata in self.background.take_dirty_jobs() {
            if self.job_generation_is_current(&metadata)
                && let TuiJobKey::OpencodeListener(stream) = metadata.key
            {
                self.request_opencode_reconciliation_for(stream.worktree);
            }
        }
        let stats = self.background.queue_stats();
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
        if restart_session_refresh && !self.background.is_draining() {
            let _ = self.refresh_sessions_after_tmux();
        }
        processed
    }

    pub(super) fn session_index_for_job_key(&self, key: &TuiJobKey) -> Option<usize> {
        let TuiJobKey::Worktree(worktree) = key else {
            return None;
        };
        self.sessions.iter().position(|session| {
            self.repos
                .get(session.repo_index)
                .is_some_and(|managed| session.identity_key(&managed.identity) == *worktree)
        })
    }

    pub(super) fn persist_shutdown_remote_cache(
        &mut self,
        key: &TuiJobKey,
        cache: &PrCache,
    ) -> Result<(), String> {
        let index = self
            .session_index_for_job_key(key)
            .ok_or_else(|| "remote mutation worktree no longer exists".to_string())?;
        let managed = self
            .repos
            .get(self.sessions[index].repo_index)
            .ok_or_else(|| "remote mutation repository no longer exists".to_string())?;
        crate::remote::persist_pr_cache_snapshot(
            &managed.repo,
            &self.sessions[index].branch,
            cache,
        )?;
        self.sessions[index].pr = cache.clone();
        Ok(())
    }

    pub(super) fn apply_shutdown_remote_action_result(
        &mut self,
        key: &TuiJobKey,
        result: &Result<RemoteActionValue, String>,
    ) -> Result<(), String> {
        let value = result
            .as_ref()
            .map_err(|error| format!("remote mutation result requires reconciliation: {error}"))?;
        match value {
            RemoteActionValue::Cache(cache)
            | RemoteActionValue::Push { cache, .. }
            | RemoteActionValue::Resolved { cache, .. }
            | RemoteActionValue::Merge { cache, .. } => {
                self.persist_shutdown_remote_cache(key, cache)
            }
            RemoteActionValue::WorktrunkUserConfig(_)
            | RemoteActionValue::ChangeRequests(_)
            | RemoteActionValue::MergeRejected(_)
            | RemoteActionValue::Complete => Ok(()),
        }?;
        uncertain_remote_mutation_error(result).map_or(Ok(()), |error| {
            Err(format!(
                "remote mutation completion requires reconciliation: {error}"
            ))
        })
    }

    pub(super) fn apply_routed_remote_actions_for_shutdown(&mut self) -> Vec<String> {
        let mut errors = Vec::new();
        while let Some(delivery) = self.background.receive_remote_action() {
            if !self.background.remote_action_is_tracked(delivery.id) {
                continue;
            }
            let Some(reconciliation) = self.background.remote_context(delivery.id) else {
                errors.push(format!(
                    "remote mutation {} was routed without its reconciliation key",
                    delivery.id
                ));
                continue;
            };
            if let Err(error) =
                self.apply_shutdown_remote_action_result(&reconciliation.key, &delivery.result)
                && let Err(marker_error) = self.record_remote_mutation_reconciliation(
                    &reconciliation.key,
                    delivery.id,
                    &error,
                    &reconciliation.target,
                    Some(&reconciliation.ledger),
                )
            {
                errors.push(format!("{error}; {marker_error}"));
            }
            self.background.finish_remote_action(delivery.id);
        }
        errors
    }

    pub(super) fn route_tui_job_payload_for_metadata(
        &mut self,
        metadata: &JobMetadata<TuiJobKind, TuiJobKey>,
        payload: TuiJobPayload,
    ) {
        if self.background.is_draining()
            && self.background.remote_action_is_tracked(metadata.id)
            && let TuiJobPayload::RemoteAction(delivery) = &payload
        {
            if let Err(error) =
                self.apply_shutdown_remote_action_result(&metadata.key, &delivery.result)
            {
                let marker = self
                    .background
                    .remote_context(metadata.id)
                    .ok_or_else(|| {
                        "remote mutation is missing its reconciliation target".to_string()
                    })
                    .and_then(|reconciliation| {
                        self.record_remote_mutation_reconciliation(
                            &reconciliation.key,
                            metadata.id,
                            &error,
                            &reconciliation.target,
                            Some(&reconciliation.ledger),
                        )
                    });
                let error = match marker {
                    Ok(()) => error,
                    Err(marker_error) => {
                        self.background.push_shutdown_error(marker_error.clone());
                        format!("{error}; {marker_error}")
                    }
                };
                self.background.record_remote_failure(metadata.id, error);
            }
            self.background.finish_remote_action(metadata.id);
        }
        self.route_tui_job_payload(payload);
    }

    pub(super) fn route_tui_job_payload(&mut self, payload: TuiJobPayload) {
        match payload {
            TuiJobPayload::SessionRefresh(result) => {
                let _ = self.session_refresh_tx.send(result);
            }
            TuiJobPayload::PrPoll(result) => {
                let _ = self.pr_poll_tx.send(result);
            }
            TuiJobPayload::DeferredMergeCleanup(result) => {
                self.apply_deferred_merge_cleanup_result(result);
            }
            TuiJobPayload::WorkflowPoll(result) => {
                let _ = self.workflow_poll_tx.send(result);
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
            TuiJobPayload::WorktrunkHookLogs(result) => {
                let _ = self.wt_hook_log_poll_tx.send(result);
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
            TuiJobPayload::RemoteAction(result) => {
                self.background.deliver_remote_action(*result);
            }
            TuiJobPayload::RemoteReconciliation(result) => {
                self.apply_remote_reconciliation_result(result);
            }
            TuiJobPayload::RemoteMarkersLoaded(result) => {
                self.apply_loaded_remote_mutation_markers(result);
            }
            TuiJobPayload::RemoteActionProgress { id, message } => {
                let _active_job = id;
                if let Some(crate::view::DialogModel::Progress {
                    message: displayed, ..
                }) = &mut self.dialog
                {
                    *displayed = message;
                }
            }
        }
    }

    pub(super) fn clear_tui_job_in_flight(
        &mut self,
        metadata: &JobMetadata<TuiJobKind, TuiJobKey>,
    ) {
        match (&metadata.kind, &metadata.key) {
            (TuiJobKind::SessionRefresh, _) => self.session_refresh_in_flight = false,
            (TuiJobKind::AgentStatePersistence, TuiJobKey::AgentStatePersistence(key)) => {
                self.agent_state_persistence_in_flight.remove(key);
            }
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
            (TuiJobKind::DeferredMergeCleanup, TuiJobKey::Delete(key)) => {
                self.deferred_merge_cleanups_in_flight.remove(key);
            }
            (TuiJobKind::WorkflowPoll, TuiJobKey::WorkflowRepository(repository)) => {
                self.workflow_polls_in_flight.remove(repository);
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
            (TuiJobKind::WorktrunkHookLogs, TuiJobKey::WorktrunkHookLogs(repository)) => {
                if let Some(repo) = self
                    .repos
                    .iter_mut()
                    .find(|repo| &repo.identity == repository)
                {
                    repo.wt_hook_logs.refresh_in_flight = false;
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

    pub(super) fn job_generation_is_current(
        &self,
        metadata: &JobMetadata<TuiJobKind, TuiJobKey>,
    ) -> bool {
        match &metadata.key {
            TuiJobKey::None => metadata.generation == self.session_inventory_generation,
            TuiJobKey::System => true,
            TuiJobKey::Repository(repository) => {
                metadata.kind == TuiJobKind::RemoteReconciliation
                    || (metadata.generation == self.session_inventory_generation
                        && self.repos.iter().any(|repo| &repo.identity == repository))
            }
            TuiJobKey::WorktrunkHookLogs(repository) => {
                self.repos.iter().any(|repo| &repo.identity == repository)
            }
            TuiJobKey::WorkflowRepository(_) => metadata.generation == self.workflow_revision,
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

    pub(super) fn worktree_generation_is_current(
        &self,
        worktree: &WorktreeSessionKey,
        generation: u64,
    ) -> bool {
        self.worktree_generations.get(worktree).copied() == Some(generation)
    }
    pub(super) fn record_tui_job_terminal(
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

    pub(super) fn record_tui_queue_stats(&self, stats: crate::tui_jobs::QueueStats) {
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

    pub(super) fn recover_failed_tui_job(&mut self, metadata: &JobMetadata<TuiJobKind, TuiJobKey>) {
        if metadata.kind == TuiJobKind::SessionRefresh {
            self.session_refresh_pending = true;
        }
        if metadata.kind == TuiJobKind::RemoteReconciliation {
            self.background.fail_reconciliation_job(metadata.id);
            if metadata.key == TuiJobKey::System {
                self.background.fail_marker_loads();
                let error =
                    "remote mutation marker loading failed; coordinated mutations remain blocked"
                        .to_string();
                for session in &mut self.sessions {
                    session.pr.require_reconciliation(&error);
                }
                self.background.push_shutdown_error(error);
            }
        }
        if let (TuiJobKind::WorktreeColumns, TuiJobKey::Repository(repository)) =
            (&metadata.kind, &metadata.key)
            && let Some(repo_index) = self
                .repos
                .iter()
                .position(|repo| &repo.identity == repository)
        {
            let error = "Worktrunk observation job failed or timed out".to_string();
            self.mark_wt_observation_stale(repo_index, error, None);
        }
        if let (TuiJobKind::WorktrunkHookLogs, TuiJobKey::WorktrunkHookLogs(repository)) =
            (&metadata.kind, &metadata.key)
            && let Some(repo_index) = self
                .repos
                .iter()
                .position(|repo| &repo.identity == repository)
        {
            self.mark_wt_hook_logs_stale(
                repo_index,
                "Worktrunk hook-log inventory job failed or timed out".to_string(),
            );
        }
        if let (TuiJobKind::DeleteSession, TuiJobKey::Delete(key)) = (&metadata.kind, &metadata.key)
        {
            if let Some(session) = self.sessions.iter_mut().find(|session| {
                self.repos
                    .get(session.repo_index)
                    .is_some_and(|repo| session.identity_key(&repo.identity) == key.worktree)
            }) {
                session.hidden = false;
            }
            self.ensure_navigation_valid();
        }
    }

    pub(super) async fn cleanup_tui_jobs(&mut self, reason: ShutdownReason) -> Result<(), String> {
        let mut errors = Vec::new();
        let started = Instant::now();
        let active_jobs = self.background.begin_shutdown();
        errors.extend(self.apply_routed_remote_actions_for_shutdown());
        self.background.stop_admission_for_shutdown();
        if let Err(error) = self.shutdown_owned_opencode_servers().await {
            errors.push(error);
        }

        let mutation_wait_started = Instant::now();
        while !self.background.tracked_remote_action_ids().is_empty()
            && mutation_wait_started.elapsed() < TUI_MUTATION_SHUTDOWN_BOUND
        {
            self.route_tui_job_messages();
            if !self.background.tracked_remote_action_ids().is_empty() {
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        if !self.background.tracked_remote_action_ids().is_empty() {
            let unfinished_mutations = self.background.tracked_remote_action_ids();
            for metadata in self.background.active_metadata() {
                if !unfinished_mutations.contains(&metadata.id) {
                    continue;
                }
                let reason = format!(
                    "remote mutation exceeded the {:?} TUI shutdown bound",
                    TUI_MUTATION_SHUTDOWN_BOUND
                );
                let marker = self
                    .background
                    .remote_context(metadata.id)
                    .ok_or_else(|| {
                        "remote mutation is missing its reconciliation target".to_string()
                    })
                    .and_then(|reconciliation| {
                        self.record_remote_mutation_reconciliation(
                            &reconciliation.key,
                            metadata.id,
                            &reason,
                            &reconciliation.target,
                            Some(&reconciliation.ledger),
                        )
                    });
                if let Err(error) = marker {
                    errors.push(error);
                }
                self.background.cancel(metadata.id);
            }
        }

        let cancellation_started = Instant::now();
        while self.background.has_jobs() && cancellation_started.elapsed() < TUI_JOB_SHUTDOWN_GRACE
        {
            self.route_tui_job_messages();
            if self.background.has_jobs() {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        while self.route_tui_job_messages() > 0 {}
        errors.extend(self.background.take_shutdown_errors());
        let unresolved_markers = self.background.unresolved_marker_persistence();
        if unresolved_markers > 0 {
            errors.push(format!(
                "shutdown durability failure: {unresolved_markers} remote mutation marker write(s) remain unacknowledged after {:?}",
                TUI_JOB_SHUTDOWN_GRACE
            ));
        }
        let unfinished = self.background.abandon_unfinished();
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

    pub(super) fn record_tui_cleanup_failure(&self, error: &str) {
        let message = format!("TUI cleanup failed: {error}");
        if crate::observability::append_runtime_message(&self.repo, &message).is_err() {
            eprintln!("prism: {message}");
        }
    }
}
