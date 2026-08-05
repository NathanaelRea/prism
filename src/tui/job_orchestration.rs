use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::agent_session::{AgentSessionWarmupKey, AgentSessionWarmupResult};
use crate::auto_flow::PersistedAutoRun;
use crate::remote::PrCache;
use crate::session::{WorktreeRepositoryKey, WorktreeSessionKey};
use crate::tui_jobs::{JobContext, JobId, JobMessage, JobMetadata, JobOutcome};

use super::{
    DashboardOutputKey, DashboardOutputResult, DefaultBranchPollResult, DeleteSessionKey,
    DeleteSessionResult, OpencodeEventResult, OpencodeListenerKey, OpencodePollKey,
    OpencodePollResult, PrPollKey, PrPollResult, RemoteActionDelivery, RemoteActionValue,
    SessionRefreshResult, TUI_ACTION_JOB_TIMEOUT, TUI_JOB_SHUTDOWN_GRACE,
    TUI_MUTATION_SHUTDOWN_BOUND, TUI_TICK_ITEM_BUDGET, TUI_TICK_TIME_BUDGET, TmuxPortalResult, Tui,
    TuiBackgroundChanges, WORKFLOW_MAINTENANCE_INTERVAL, WorkflowPollResult, WtHookLogPollResult,
    WtPollResult, maintain_workflow_storage, uncertain_remote_mutation_error,
};

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
    WorktrunkHookLogs,
    DefaultBranch,
    OpencodePoll,
    OpencodeListener,
    RemoteAction,
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
            Self::WorktrunkHookLogs => "worktrunk_hook_logs",
            Self::DefaultBranch => "default_branch",
            Self::OpencodePoll => "opencode_poll",
            Self::OpencodeListener => "opencode_listener",
            Self::RemoteAction => "remote_action",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum TuiJobKey {
    None,
    Repository(WorktreeRepositoryKey),
    WorktrunkHookLogs(WorktreeRepositoryKey),
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
    WorkflowMaintenance,
    PrPoll(PrPollResult),
    WorkflowPoll(WorkflowPollResult),
    DashboardOutput(DashboardOutputResult),
    DeleteSession(DeleteSessionResult),
    TmuxWarmup(AgentSessionWarmupResult),
    TmuxPortal(TmuxPortalResult),
    WorktreeColumns(WtPollResult),
    WorktrunkHookLogs(WtHookLogPollResult),
    DefaultBranch(DefaultBranchPollResult),
    OpencodePoll(OpencodePollResult),
    OpencodeEvent(OpencodeEventResult),
    RemoteAction(Box<RemoteActionDelivery>),
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
            dashboard_output: self.poll_dashboard_outputs(),
            pull_requests: self.poll_pull_requests(false),
            delete_sessions: self.poll_delete_sessions(),
            status_message: self.expire_status_message(),
        };
        self.start_scheduled_wt_polls();
        self.start_pending_wt_hook_log_refreshes();
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

    pub(super) fn confirm_quit(&mut self) -> Result<bool, String> {
        if !self.delete_sessions_in_flight.is_empty() {
            self.show_message("delete in progress; wait for it to finish before quitting")?;
            return Ok(false);
        }
        Ok(true)
    }

    pub(crate) fn request_workflow_maintenance(&mut self) {
        self.workflow_maintenance_due = true;
    }

    pub(super) fn poll_workflow_maintenance(&mut self) {
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
    ) -> JobId
    where
        F: FnOnce(
                JobContext<TuiJobKind, TuiJobKey, TuiJobPayload>,
            ) -> Result<Option<TuiJobPayload>, String>
            + Send
            + 'static,
    {
        if matches!(kind, TuiJobKind::DeleteSession | TuiJobKind::RemoteAction) {
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
            )
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
            )
        }
    }

    pub(super) fn repository_root_for_job_key(&self, key: &TuiJobKey) -> Option<PathBuf> {
        match key {
            TuiJobKey::Repository(repository) => Some(repository.root.clone()),
            TuiJobKey::WorktrunkHookLogs(repository) => Some(repository.root.clone()),
            TuiJobKey::Worktree(worktree) | TuiJobKey::AgentStatePersistence(worktree) => {
                Some(worktree.repository.root.clone())
            }
            TuiJobKey::Pr(key) | TuiJobKey::PrPersistence(key) => {
                Some(key.worktree.repository.root.clone())
            }
            TuiJobKey::Delete(key) => Some(key.worktree.repository.root.clone()),
            TuiJobKey::Tmux(key) => Some(key.slot.worktree.repository.root.clone()),
            TuiJobKey::Opencode(key) => Some(key.worktree.repository.root.clone()),
            TuiJobKey::OpencodeListener(key) => Some(key.worktree.repository.root.clone()),
            TuiJobKey::WorkflowRepository(repository) => Some(repository.root.clone()),
            TuiJobKey::DashboardOutput(key) => Some(match key {
                DashboardOutputKey::Plan { repository, .. }
                | DashboardOutputKey::Auto { repository, .. } => repository.root.clone(),
            }),
            TuiJobKey::None => None,
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

    pub(super) fn route_tui_job_messages_with_budget(
        &mut self,
        limit: usize,
        deadline: Instant,
    ) -> usize {
        for metadata in self.jobs.active_metadata() {
            if !self.job_generation_is_current(&metadata)
                && !self
                    .remote_actions_requiring_reconciliation
                    .contains(&metadata.id)
            {
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
                self.remote_action_failures
                    .insert(metadata.id, error.clone());
                if self
                    .remote_actions_requiring_reconciliation
                    .contains(&metadata.id)
                    && !matches!(outcome, JobOutcome::SpawnFailed(_))
                    && let Some(reconciliation) = self
                        .remote_action_reconciliation_contexts
                        .get(&metadata.id)
                        .cloned()
                    && let Err(marker_error) = self.record_remote_mutation_reconciliation(
                        &reconciliation.key,
                        metadata.id,
                        &error,
                        &reconciliation.target,
                    )
                {
                    self.remote_action_failures
                        .insert(metadata.id, format!("{error}; {marker_error}"));
                    self.shutdown_remote_action_errors.push(marker_error);
                }
                self.remote_actions_requiring_reconciliation
                    .remove(&metadata.id);
                self.remote_action_reconciliation_contexts
                    .remove(&metadata.id);
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
                    | TuiJobKey::Repository(_)
                    | TuiJobKey::WorktrunkHookLogs(_)
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
            if self.job_generation_is_current(&metadata)
                || self
                    .remote_actions_requiring_reconciliation
                    .contains(&metadata.id)
            {
                self.route_tui_job_payload_for_metadata(&metadata, payload);
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
            if self.job_generation_is_current(&metadata)
                || self
                    .remote_actions_requiring_reconciliation
                    .contains(&metadata.id)
            {
                self.route_tui_job_payload_for_metadata(&metadata, payload);
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

    pub(super) fn persist_shutdown_auto_run(
        &mut self,
        persisted: &PersistedAutoRun,
    ) -> Result<(), String> {
        let managed = self
            .repos
            .iter()
            .find(|managed| managed.repo.root == Path::new(&persisted.run.repo_root))
            .ok_or_else(|| {
                format!(
                    "remote mutation Auto Flow repository no longer exists: {}",
                    persisted.run.repo_root
                )
            })?;
        let mut persisted = persisted.clone();
        crate::observability::with_writable_db(&managed.repo, |conn| {
            crate::auto_flow::save_auto_run(conn, &mut persisted)
        })?;
        self.remember_auto_run(persisted);
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
            RemoteActionValue::Cache(cache) => self.persist_shutdown_remote_cache(key, cache),
            RemoteActionValue::Resolved { cache, .. } => {
                self.persist_shutdown_remote_cache(key, cache)
            }
            RemoteActionValue::PushPrepared(prepared) => {
                self.persist_shutdown_remote_cache(key, &prepared.cache)
            }
            RemoteActionValue::GuardedPush {
                persisted, cache, ..
            }
            | RemoteActionValue::ReviewResolutionFinished {
                persisted, cache, ..
            } => {
                self.persist_shutdown_remote_cache(key, cache)?;
                self.persist_shutdown_auto_run(persisted)
            }
            RemoteActionValue::ReviewResolutionPrepared {
                persisted, cache, ..
            } => {
                self.persist_shutdown_remote_cache(key, cache)?;
                self.persist_shutdown_auto_run(persisted)
            }
            RemoteActionValue::MergeExecution { session, result: _ } => {
                let managed = self
                    .repos
                    .get(session.repo_index)
                    .ok_or_else(|| "merged worktree repository no longer exists".to_string())?;
                crate::remote::persist_pr_cache_snapshot(
                    &managed.repo,
                    &session.branch,
                    &session.pr,
                )?;
                if let Some(index) = self.sessions.iter().position(|current| {
                    current.repo_index == session.repo_index && current.path == session.path
                }) {
                    self.sessions[index].pr = session.pr.clone();
                }
                Ok(())
            }
            RemoteActionValue::WorktrunkUserConfig(_)
            | RemoteActionValue::ChangeRequests(_)
            | RemoteActionValue::CreatePrepared(_)
            | RemoteActionValue::MergeAuthorization { .. }
            | RemoteActionValue::NotApplicable
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
        while let Ok(delivery) = self.remote_action_rx.try_recv() {
            if !self
                .remote_actions_requiring_reconciliation
                .contains(&delivery.id)
            {
                continue;
            }
            let Some(reconciliation) = self
                .remote_action_reconciliation_contexts
                .get(&delivery.id)
                .cloned()
            else {
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
                )
            {
                errors.push(format!("{error}; {marker_error}"));
            }
            self.remote_actions_requiring_reconciliation
                .remove(&delivery.id);
            self.remote_action_reconciliation_contexts
                .remove(&delivery.id);
        }
        errors
    }

    pub(super) fn route_tui_job_payload_for_metadata(
        &mut self,
        metadata: &JobMetadata<TuiJobKind, TuiJobKey>,
        payload: TuiJobPayload,
    ) {
        if self.scheduling_stopped
            && self
                .remote_actions_requiring_reconciliation
                .contains(&metadata.id)
            && let TuiJobPayload::RemoteAction(delivery) = &payload
        {
            if let Err(error) =
                self.apply_shutdown_remote_action_result(&metadata.key, &delivery.result)
            {
                let marker = self
                    .remote_action_reconciliation_contexts
                    .get(&metadata.id)
                    .cloned()
                    .ok_or_else(|| {
                        "remote mutation is missing its reconciliation target".to_string()
                    })
                    .and_then(|reconciliation| {
                        self.record_remote_mutation_reconciliation(
                            &reconciliation.key,
                            metadata.id,
                            &error,
                            &reconciliation.target,
                        )
                    });
                let error = match marker {
                    Ok(()) => error,
                    Err(marker_error) => {
                        self.shutdown_remote_action_errors
                            .push(marker_error.clone());
                        format!("{error}; {marker_error}")
                    }
                };
                self.remote_action_failures.insert(metadata.id, error);
            }
            self.remote_actions_requiring_reconciliation
                .remove(&metadata.id);
            self.remote_action_reconciliation_contexts
                .remove(&metadata.id);
        }
        self.route_tui_job_payload(payload);
    }

    pub(super) fn route_tui_job_payload(&self, payload: TuiJobPayload) {
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
                let _ = self.remote_action_tx.send(*result);
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
            TuiJobKey::None => {
                metadata.kind == TuiJobKind::WorkflowMaintenance
                    || metadata.generation == self.session_inventory_generation
            }
            TuiJobKey::Repository(_) => metadata.generation == self.session_inventory_generation,
            TuiJobKey::WorktrunkHookLogs(repository) => {
                self.repos.iter().any(|repo| &repo.identity == repository)
            }
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

    pub(super) fn cleanup_tui_jobs(&mut self, reason: ShutdownReason) -> Result<(), String> {
        let mut errors = Vec::new();
        let started = Instant::now();
        let active_jobs = self.jobs.active_metadata().len();
        self.scheduling_stopped = true;
        errors.extend(self.apply_routed_remote_actions_for_shutdown());
        self.jobs.stop_accepting();
        let protected = self.remote_actions_requiring_reconciliation.clone();
        self.jobs.cancel_all_except(&protected);
        if let Err(error) = self.shutdown_owned_opencode_servers() {
            errors.push(error);
        }

        let mutation_wait_started = Instant::now();
        while !self.remote_actions_requiring_reconciliation.is_empty()
            && mutation_wait_started.elapsed() < TUI_MUTATION_SHUTDOWN_BOUND
        {
            self.route_tui_job_messages();
            if !self.remote_actions_requiring_reconciliation.is_empty() {
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        if !self.remote_actions_requiring_reconciliation.is_empty() {
            let unfinished_mutations = self.remote_actions_requiring_reconciliation.clone();
            for metadata in self.jobs.active_metadata() {
                if !unfinished_mutations.contains(&metadata.id) {
                    continue;
                }
                let reason = format!(
                    "remote mutation exceeded the {:?} TUI shutdown bound",
                    TUI_MUTATION_SHUTDOWN_BOUND
                );
                let marker = self
                    .remote_action_reconciliation_contexts
                    .get(&metadata.id)
                    .cloned()
                    .ok_or_else(|| {
                        "remote mutation is missing its reconciliation target".to_string()
                    })
                    .and_then(|reconciliation| {
                        self.record_remote_mutation_reconciliation(
                            &reconciliation.key,
                            metadata.id,
                            &reason,
                            &reconciliation.target,
                        )
                    });
                if let Err(error) = marker {
                    errors.push(error);
                }
                self.jobs.cancel(metadata.id);
            }
        }

        let cancellation_started = Instant::now();
        while self.jobs.has_jobs() && cancellation_started.elapsed() < TUI_JOB_SHUTDOWN_GRACE {
            self.route_tui_job_messages();
            if self.jobs.has_jobs() {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        while self.route_tui_job_messages() > 0 {}
        errors.append(&mut self.shutdown_remote_action_errors);
        let unfinished = self.jobs.abandon_unfinished();
        self.remote_actions_requiring_reconciliation.clear();
        self.remote_action_reconciliation_contexts.clear();
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
