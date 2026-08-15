use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant};

use crate::session::WorktreeRepositoryKey;
use crate::tui_jobs::{
    JobContext, JobId, JobMessage, JobMetadata, JobRegistry, LatestReceiver, LatestSender,
    QueueStats, latest_channel,
};

use super::remote_action::{
    RemoteActionDelivery, RemoteActionReconciliationContext, RemoteMutationReconciliationMarker,
    RemoteMutationTarget, update_persisted_remote_mutation_markers,
};
use super::{TuiJobKey, TuiJobKind, TuiJobPayload};

type MarkerKey = (PathBuf, u64, JobId);

#[derive(Clone)]
struct MarkerPersistenceRequest {
    marker: RemoteMutationReconciliationMarker,
}

pub(super) struct MarkerPersistenceResult {
    pub key: MarkerKey,
    pub result: Result<bool, String>,
}

enum MarkerWriterCommand {
    Persist {
        key: MarkerKey,
        request: MarkerPersistenceRequest,
    },
}

const MARKER_RETRY_MAX_BACKOFF: Duration = Duration::from_millis(250);
const MARKER_WRITER_START_MAX_BACKOFF: Duration = Duration::from_millis(250);

fn retry_backoff(failures: u32, maximum: Duration) -> Duration {
    let shift = failures.saturating_sub(1).min(5);
    Duration::from_millis((10_u64 << shift).min(maximum.as_millis() as u64))
}

fn persist_marker_once(
    request: &MarkerPersistenceRequest,
    #[cfg(test)] write_failures: &std::sync::atomic::AtomicUsize,
) -> Result<bool, String> {
    #[cfg(test)]
    if write_failures
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
            remaining.checked_sub(1)
        })
        .is_ok()
    {
        return Err("injected durable marker write failure".to_string());
    }

    let marker = &request.marker;
    update_persisted_remote_mutation_markers(&marker.database_path, |persisted| {
        if let Some(existing) = persisted
            .iter_mut()
            .find(|existing| existing.target == marker.target)
        {
            if existing.recorded_unix_ms <= marker.recorded_unix_ms {
                *existing = marker.clone();
            }
        } else {
            persisted.push(marker.clone());
        }
        persisted.iter().any(|existing| {
            existing.target == marker.target
                && existing.recorded_unix_ms == marker.recorded_unix_ms
                && existing.job_id == marker.job_id
        })
    })
}

fn run_marker_writer(
    receiver: mpsc::Receiver<MarkerWriterCommand>,
    result_sender: mpsc::Sender<MarkerPersistenceResult>,
    stop: Arc<AtomicBool>,
    #[cfg(test)] write_failures: Arc<std::sync::atomic::AtomicUsize>,
) {
    while let Ok(MarkerWriterCommand::Persist { key, request }) = receiver.recv() {
        let mut failures = 0_u32;
        loop {
            if stop.load(Ordering::Acquire) {
                return;
            }
            match persist_marker_once(
                &request,
                #[cfg(test)]
                &write_failures,
            ) {
                Ok(persisted) => {
                    let _ = result_sender.send(MarkerPersistenceResult {
                        key,
                        result: Ok(persisted),
                    });
                    break;
                }
                Err(_) => {
                    failures = failures.saturating_add(1);
                    thread::sleep(retry_backoff(failures, MARKER_RETRY_MAX_BACKOFF));
                }
            }
        }
    }
}

/// Owns the nonvisual lifecycle of TUI background work.
///
/// Admission, cancellation, remote-action delivery, durable reconciliation tracking, and
/// shutdown/draining state are kept behind this API so UI code can only make valid transitions.
pub(crate) struct BackgroundRuntime {
    jobs: JobRegistry<TuiJobKind, TuiJobKey, TuiJobPayload>,
    remote_action_tx: LatestSender<JobId, RemoteActionDelivery>,
    remote_action_rx: LatestReceiver<JobId, RemoteActionDelivery>,
    remote_action_failures: BTreeMap<JobId, String>,
    remote_actions_requiring_reconciliation: BTreeSet<JobId>,
    remote_action_reconciliation_contexts: BTreeMap<JobId, RemoteActionReconciliationContext>,
    remote_mutations_requiring_reconciliation:
        BTreeMap<PathBuf, Vec<RemoteMutationReconciliationMarker>>,
    remote_markers_loaded: BTreeSet<WorktreeRepositoryKey>,
    remote_marker_loads_in_flight: BTreeSet<WorktreeRepositoryKey>,
    remote_reconciliations_in_flight: BTreeSet<MarkerKey>,
    remote_reconciliation_jobs: BTreeMap<JobId, MarkerKey>,
    persisted_remote_reconciliation_markers: BTreeSet<MarkerKey>,
    marker_persistence_tx: mpsc::Sender<MarkerPersistenceResult>,
    marker_persistence_rx: mpsc::Receiver<MarkerPersistenceResult>,
    marker_writer_tx: Option<mpsc::Sender<MarkerWriterCommand>>,
    marker_writer_handle: Option<thread::JoinHandle<()>>,
    marker_writer_stop: Arc<AtomicBool>,
    marker_writer_next_start: Instant,
    marker_writer_start_failures: u32,
    marker_persistence_pending: BTreeMap<MarkerKey, MarkerPersistenceRequest>,
    marker_writer_enqueued: BTreeSet<MarkerKey>,
    #[cfg(test)]
    marker_writer_spawn_failures: usize,
    #[cfg(test)]
    marker_write_failures: Arc<std::sync::atomic::AtomicUsize>,
    shutdown_errors: Vec<String>,
    accepting: bool,
    draining: bool,
    routing: bool,
}

impl Default for BackgroundRuntime {
    fn default() -> Self {
        let (remote_action_tx, remote_action_rx) =
            latest_channel(|result: &RemoteActionDelivery| result.id);
        let (marker_persistence_tx, marker_persistence_rx) = mpsc::channel();
        Self {
            jobs: JobRegistry::default(),
            remote_action_tx,
            remote_action_rx,
            remote_action_failures: BTreeMap::new(),
            remote_actions_requiring_reconciliation: BTreeSet::new(),
            remote_action_reconciliation_contexts: BTreeMap::new(),
            remote_mutations_requiring_reconciliation: BTreeMap::new(),
            remote_markers_loaded: BTreeSet::new(),
            remote_marker_loads_in_flight: BTreeSet::new(),
            remote_reconciliations_in_flight: BTreeSet::new(),
            remote_reconciliation_jobs: BTreeMap::new(),
            persisted_remote_reconciliation_markers: BTreeSet::new(),
            marker_persistence_tx,
            marker_persistence_rx,
            marker_writer_tx: None,
            marker_writer_handle: None,
            marker_writer_stop: Arc::new(AtomicBool::new(false)),
            marker_writer_next_start: Instant::now(),
            marker_writer_start_failures: 0,
            marker_persistence_pending: BTreeMap::new(),
            marker_writer_enqueued: BTreeSet::new(),
            #[cfg(test)]
            marker_writer_spawn_failures: 0,
            #[cfg(test)]
            marker_write_failures: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            shutdown_errors: Vec::new(),
            accepting: true,
            draining: false,
            routing: false,
        }
    }
}

impl Drop for BackgroundRuntime {
    fn drop(&mut self) {
        if !self.marker_persistence_pending.is_empty() {
            eprintln!(
                "Prism marker writer stopped with {} unresolved durable marker write(s)",
                self.marker_persistence_pending.len()
            );
        }
        self.marker_writer_stop.store(true, Ordering::Release);
        self.marker_writer_tx.take();
        if let Some(handle) = self.marker_writer_handle.take() {
            let _ = handle.join();
        }
    }
}

impl BackgroundRuntime {
    pub(super) fn spawn<F>(
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
        let label = kind.label();
        let diagnostic = crate::tui_jobs::JobDiagnostic {
            timeout,
            kind: label,
        };
        if matches!(
            kind,
            TuiJobKind::DeleteSession | TuiJobKind::RemoteAction | TuiJobKind::RemoteReconciliation
        ) {
            self.jobs
                .spawn_reliable_diagnostic(kind, key, generation, name, diagnostic, job)
        } else {
            self.jobs
                .spawn_diagnostic(kind, key, generation, name, diagnostic, job)
        }
    }

    pub(super) fn begin_routing(&mut self) -> bool {
        if self.routing {
            false
        } else {
            self.routing = true;
            true
        }
    }

    pub(super) fn finish_routing(&mut self) {
        self.routing = false;
    }

    pub(crate) fn is_routing(&self) -> bool {
        self.routing
    }

    pub(crate) fn active_metadata(&mut self) -> Vec<JobMetadata<TuiJobKind, TuiJobKey>> {
        self.jobs.active_metadata()
    }

    pub(super) fn drain_terminals(
        &mut self,
        limit: usize,
    ) -> Vec<JobMessage<TuiJobKind, TuiJobKey, TuiJobPayload>> {
        self.jobs.drain_terminals(limit)
    }

    pub(super) fn latest_min_priority(
        &mut self,
        priority: impl Fn(&JobMetadata<TuiJobKind, TuiJobKey>) -> u8,
    ) -> Option<u8> {
        self.jobs.latest_min_priority(priority)
    }

    pub(super) fn take_latest_by(
        &mut self,
        priority: impl Fn(&JobMetadata<TuiJobKind, TuiJobKey>) -> u8,
    ) -> Option<JobMessage<TuiJobKind, TuiJobKey, TuiJobPayload>> {
        self.jobs.take_latest_by(priority)
    }

    pub(super) fn take_stream_event(
        &mut self,
    ) -> Option<JobMessage<TuiJobKind, TuiJobKey, TuiJobPayload>> {
        self.jobs.take_stream_event()
    }

    pub(super) fn take_dirty_jobs(&mut self) -> Vec<JobMetadata<TuiJobKind, TuiJobKey>> {
        self.jobs.take_dirty_jobs()
    }

    pub(super) fn queue_stats(&mut self) -> QueueStats {
        self.jobs.queue_stats()
    }

    pub(crate) fn cancel(&mut self, id: JobId) {
        self.jobs.cancel(id);
    }

    pub(super) fn has_jobs(&mut self) -> bool {
        self.maintain_marker_writer();
        self.jobs.has_jobs() || !self.marker_persistence_pending.is_empty()
    }

    pub(super) fn begin_shutdown(&mut self) -> usize {
        if self.draining {
            return self.jobs.active_metadata().len();
        }
        self.draining = true;
        self.jobs.active_metadata().len()
    }

    /// Closes admission only after the owner has drained already-routed remote results. Those
    /// results may need to admit one final durable marker-persistence job.
    pub(super) fn stop_admission_for_shutdown(&mut self) {
        assert!(
            self.draining,
            "shutdown admission can only stop while draining"
        );
        if !self.accepting {
            return;
        }
        self.accepting = false;
        self.jobs.stop_accepting();
        let protected = self.remote_actions_requiring_reconciliation.clone();
        self.jobs.cancel_all_except(&protected);
    }

    #[cfg(test)]
    pub(super) fn cancel_stale_except(&mut self, current: &BTreeSet<JobId>) {
        for metadata in self.jobs.active_metadata() {
            if !current.contains(&metadata.id)
                && !self
                    .remote_actions_requiring_reconciliation
                    .contains(&metadata.id)
            {
                self.jobs.cancel(metadata.id);
            }
        }
    }

    pub(super) fn is_draining(&self) -> bool {
        self.draining
    }

    pub(super) fn abandon_unfinished(&mut self) -> usize {
        let unfinished = self.jobs.abandon_unfinished();
        self.remote_actions_requiring_reconciliation.clear();
        self.remote_action_reconciliation_contexts.clear();
        unfinished
    }

    pub(super) fn unresolved_marker_persistence(&self) -> usize {
        self.marker_persistence_pending.len()
    }

    pub(super) fn track_remote_action(
        &mut self,
        id: JobId,
        context: RemoteActionReconciliationContext,
    ) {
        self.remote_actions_requiring_reconciliation.insert(id);
        self.remote_action_reconciliation_contexts
            .insert(id, context);
    }

    pub(super) fn finish_remote_action(&mut self, id: JobId) {
        self.remote_actions_requiring_reconciliation.remove(&id);
        self.remote_action_reconciliation_contexts.remove(&id);
    }

    pub(super) fn remote_action_is_tracked(&self, id: JobId) -> bool {
        self.remote_actions_requiring_reconciliation.contains(&id)
    }

    pub(super) fn remote_context(&self, id: JobId) -> Option<RemoteActionReconciliationContext> {
        self.remote_action_reconciliation_contexts.get(&id).cloned()
    }

    pub(super) fn tracked_remote_action_ids(&self) -> BTreeSet<JobId> {
        self.remote_actions_requiring_reconciliation.clone()
    }

    pub(super) fn deliver_remote_action(&self, delivery: RemoteActionDelivery) {
        let _ = self.remote_action_tx.send(delivery);
    }

    pub(super) fn receive_remote_action(&self) -> Option<RemoteActionDelivery> {
        self.remote_action_rx.try_recv().ok()
    }

    pub(super) fn record_remote_failure(&mut self, id: JobId, error: String) {
        self.remote_action_failures.insert(id, error);
    }

    pub(super) fn take_remote_failure(&mut self, id: JobId) -> Option<String> {
        self.remote_action_failures.remove(&id)
    }

    pub(super) fn markers(&self, root: &Path) -> Option<&[RemoteMutationReconciliationMarker]> {
        self.remote_mutations_requiring_reconciliation
            .get(root)
            .map(Vec::as_slice)
    }

    pub(super) fn begin_marker_load(&mut self, repository: WorktreeRepositoryKey) -> bool {
        !self.remote_markers_loaded.contains(&repository)
            && self.remote_marker_loads_in_flight.insert(repository)
    }

    pub(super) fn finish_marker_loads(&mut self, repositories: &BTreeSet<WorktreeRepositoryKey>) {
        self.remote_marker_loads_in_flight
            .retain(|repository| !repositories.contains(repository));
    }

    pub(super) fn fail_marker_loads(&mut self) {
        self.remote_marker_loads_in_flight.clear();
    }

    pub(super) fn markers_are_loaded(&self, repository: &WorktreeRepositoryKey) -> bool {
        self.remote_markers_loaded.contains(repository)
    }

    pub(super) fn apply_loaded_markers(
        &mut self,
        repositories: BTreeSet<WorktreeRepositoryKey>,
        mut markers: BTreeMap<PathBuf, Vec<RemoteMutationReconciliationMarker>>,
    ) {
        for repository in repositories {
            self.remote_markers_loaded
                .retain(|loaded| loaded.root != repository.root);
            self.remote_markers_loaded.insert(repository.clone());
            self.persisted_remote_reconciliation_markers
                .retain(|(root, _, _)| root != &repository.root);
            self.remote_mutations_requiring_reconciliation
                .remove(&repository.root);
            if let Some(entries) = markers.remove(&repository.root) {
                for marker in &entries {
                    self.persisted_remote_reconciliation_markers.insert((
                        repository.root.clone(),
                        marker.recorded_unix_ms,
                        marker.job_id,
                    ));
                }
                self.remote_mutations_requiring_reconciliation
                    .insert(repository.root, entries);
            }
        }
    }

    pub(crate) fn retain_repositories(&mut self, repositories: &BTreeSet<WorktreeRepositoryKey>) {
        self.remote_markers_loaded
            .retain(|repository| repositories.contains(repository));
        self.remote_marker_loads_in_flight
            .retain(|repository| repositories.contains(repository));
        let roots = repositories
            .iter()
            .map(|repository| &repository.root)
            .collect::<BTreeSet<_>>();
        self.remote_mutations_requiring_reconciliation
            .retain(|root, _| roots.contains(root));
        self.persisted_remote_reconciliation_markers
            .retain(|(root, _, _)| roots.contains(root));
    }

    pub(super) fn upsert_marker(
        &mut self,
        root: PathBuf,
        marker: RemoteMutationReconciliationMarker,
    ) {
        let markers = self
            .remote_mutations_requiring_reconciliation
            .entry(root)
            .or_default();
        if let Some(existing) = markers
            .iter_mut()
            .find(|existing| existing.target == marker.target)
        {
            *existing = marker;
        } else {
            markers.push(marker);
        }
    }

    pub(super) fn marker_blocks(
        &self,
        repository: &WorktreeRepositoryKey,
        target: &RemoteMutationTarget,
    ) -> bool {
        !self.remote_markers_loaded.contains(repository)
            || self.markers(&repository.root).is_some_and(|markers| {
                markers.iter().any(|marker| {
                    matches!(marker.target, RemoteMutationTarget::Unknown { .. })
                        || super::remote_action::remote_mutation_targets_overlap(
                            &marker.target,
                            target,
                        )
                })
            })
    }

    pub(super) fn persist_marker(
        &mut self,
        repository_root: PathBuf,
        marker: RemoteMutationReconciliationMarker,
    ) -> Result<(), String> {
        let key = (
            repository_root.clone(),
            marker.recorded_unix_ms,
            marker.job_id,
        );
        self.marker_persistence_pending
            .insert(key, MarkerPersistenceRequest { marker });
        self.maintain_marker_writer();
        Ok(())
    }

    pub(super) fn drain_marker_persistence_results(&mut self) -> Vec<MarkerPersistenceResult> {
        let mut results = Vec::new();
        while let Ok(result) = self.marker_persistence_rx.try_recv() {
            self.marker_persistence_pending.remove(&result.key);
            self.marker_writer_enqueued.remove(&result.key);
            if result.result.as_ref().is_ok_and(|persisted| *persisted) {
                self.persisted_remote_reconciliation_markers
                    .insert(result.key.clone());
            }
            results.push(result);
        }
        self.maintain_marker_writer();
        results
    }

    fn maintain_marker_writer(&mut self) {
        if self
            .marker_writer_handle
            .as_ref()
            .is_some_and(thread::JoinHandle::is_finished)
        {
            if let Some(handle) = self.marker_writer_handle.take() {
                let _ = handle.join();
            }
            self.marker_writer_tx = None;
            self.marker_writer_enqueued.clear();
        }
        if self.marker_persistence_pending.is_empty() {
            return;
        }
        if self.marker_writer_tx.is_none() && Instant::now() >= self.marker_writer_next_start {
            self.start_marker_writer();
        }
        let Some(sender) = self.marker_writer_tx.clone() else {
            return;
        };
        let unsent = self
            .marker_persistence_pending
            .iter()
            .filter(|(key, _)| !self.marker_writer_enqueued.contains(*key))
            .map(|(key, request)| (key.clone(), request.clone()))
            .collect::<Vec<_>>();
        for (key, request) in unsent {
            if sender
                .send(MarkerWriterCommand::Persist {
                    key: key.clone(),
                    request,
                })
                .is_ok()
            {
                self.marker_writer_enqueued.insert(key);
            } else {
                self.marker_writer_tx = None;
                self.marker_writer_enqueued.clear();
                self.marker_writer_next_start = Instant::now();
                break;
            }
        }
    }

    fn start_marker_writer(&mut self) {
        #[cfg(test)]
        if self.marker_writer_spawn_failures > 0 {
            self.marker_writer_spawn_failures -= 1;
            self.record_marker_writer_start_failure();
            return;
        }
        let (command_sender, command_receiver) = mpsc::channel();
        let result_sender = self.marker_persistence_tx.clone();
        let stop = self.marker_writer_stop.clone();
        #[cfg(test)]
        let write_failures = self.marker_write_failures.clone();
        match thread::Builder::new()
            .name("prism-marker-persistence".to_string())
            .spawn(move || {
                run_marker_writer(
                    command_receiver,
                    result_sender,
                    stop,
                    #[cfg(test)]
                    write_failures,
                );
            }) {
            Ok(handle) => {
                self.marker_writer_tx = Some(command_sender);
                self.marker_writer_handle = Some(handle);
                self.marker_writer_start_failures = 0;
                self.marker_writer_next_start = Instant::now();
            }
            Err(_) => self.record_marker_writer_start_failure(),
        }
    }

    fn record_marker_writer_start_failure(&mut self) {
        self.marker_writer_start_failures = self.marker_writer_start_failures.saturating_add(1);
        self.marker_writer_next_start = Instant::now()
            + retry_backoff(
                self.marker_writer_start_failures,
                MARKER_WRITER_START_MAX_BACKOFF,
            );
    }

    pub(super) fn marker_is_persisted(&self, key: &MarkerKey) -> bool {
        self.persisted_remote_reconciliation_markers.contains(key)
    }

    pub(super) fn begin_reconciliation(&mut self, key: MarkerKey) -> bool {
        self.remote_reconciliations_in_flight.insert(key)
    }

    pub(super) fn track_reconciliation_job(&mut self, job_id: JobId, key: MarkerKey) {
        self.remote_reconciliation_jobs.insert(job_id, key);
    }

    pub(super) fn fail_reconciliation_job(&mut self, job_id: JobId) {
        if let Some(key) = self.remote_reconciliation_jobs.remove(&job_id) {
            self.remote_reconciliations_in_flight.remove(&key);
        }
    }

    pub(super) fn finish_reconciliation(
        &mut self,
        root: &Path,
        version: u64,
        job_id: JobId,
        target: &RemoteMutationTarget,
        succeeded: bool,
    ) {
        let key = (root.to_path_buf(), version, job_id);
        self.remote_reconciliations_in_flight.remove(&key);
        self.remote_reconciliation_jobs
            .retain(|_, tracked| tracked != &key);
        if !succeeded {
            return;
        }
        let mut remove_repository = false;
        if let Some(markers) = self.remote_mutations_requiring_reconciliation.get_mut(root) {
            markers.retain(|marker| {
                !(marker.target == *target
                    && marker.recorded_unix_ms == version
                    && marker.job_id == job_id)
            });
            remove_repository = markers.is_empty();
        }
        self.persisted_remote_reconciliation_markers.remove(&key);
        if remove_repository {
            self.remote_mutations_requiring_reconciliation.remove(root);
        }
    }

    pub(super) fn push_shutdown_error(&mut self, error: String) {
        self.shutdown_errors.push(error);
    }

    pub(super) fn take_shutdown_errors(&mut self) -> Vec<String> {
        std::mem::take(&mut self.shutdown_errors)
    }

    #[cfg(test)]
    pub(super) fn fail_next_spawn(&mut self) {
        self.jobs.fail_next_spawn();
    }

    #[cfg(test)]
    pub(super) fn fail_marker_writer_spawns(&mut self, count: usize) {
        self.marker_writer_spawn_failures = count;
        self.marker_writer_tx = None;
        self.marker_writer_enqueued.clear();
        self.marker_writer_next_start = Instant::now();
    }

    #[cfg(test)]
    pub(super) fn fail_marker_writes(&mut self, count: usize) {
        self.marker_write_failures.store(count, Ordering::Release);
    }

    #[cfg(test)]
    pub(super) fn replace_registry(
        &mut self,
        registry: JobRegistry<TuiJobKind, TuiJobKey, TuiJobPayload>,
    ) {
        assert!(!self.draining, "cannot replace registry while draining");
        self.jobs = registry;
    }

    #[cfg(test)]
    pub(super) fn collect_finished(&mut self) {
        self.jobs.collect_finished();
    }

    #[cfg(test)]
    pub(super) fn marker_count(&self, root: &Path) -> usize {
        self.markers(root).map_or(0, <[_]>::len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_preserves_reconciliation_jobs_and_orders_admission_cutoff() {
        let mut runtime = BackgroundRuntime::default();
        let id = runtime.spawn(
            TuiJobKind::RemoteAction,
            TuiJobKey::None,
            0,
            None,
            "mutation".into(),
            |_| {
                std::thread::sleep(Duration::from_millis(20));
                Ok(None)
            },
        );
        runtime.track_remote_action(
            id,
            RemoteActionReconciliationContext {
                key: TuiJobKey::None,
                target: RemoteMutationTarget::Unknown { marker_id: "x".into() },
                ledger: super::super::RemoteMutationLedgerContext {
                    repository: PathBuf::from("repo"),
                    worktree: PathBuf::from("worktree"),
                    request_id: "request".into(),
                    operation: crate::workflow::remote_operation::RemoteMutationOperation::TuiFetchChangeRequest(
                        crate::workflow::remote_operation::TuiRemoteFetchPayload {
                            repository: PathBuf::from("repo"),
                            worktree: PathBuf::from("worktree"),
                            summary: crate::remote::PrSummary {
                                number: 1,
                                change_request_identity: None,
                                native_state_evidence: crate::remote::NativeStateEvidence::default(),
                                title: "test".into(),
                                author: "test".into(),
                                body: String::new(),
                                url: String::new(),
                                state: "OPEN".into(),
                                review_decision: String::new(),
                                requested_reviewers: Vec::new(),
                                head_ref: "branch".into(),
                                base_ref: "main".into(),
                                head_sha: "head".into(),
                                updated_at: String::new(),
                                check_status: String::new(),
                                merge_state_status: String::new(),
                                queue_state: String::new(),
                                comment_count: 0,
                                merged: false,
                                draft: false,
                            },
                            branch: "branch".into(),
                        },
                    ),
                    subject: "subject".into(),
                },
            },
        );
        assert_eq!(runtime.begin_shutdown(), 1);
        assert!(runtime.remote_action_is_tracked(id));
        assert!(runtime.is_draining());
        runtime.stop_admission_for_shutdown();
    }

    #[test]
    fn generation_admission_cancels_stale_jobs_and_routes_terminal() {
        let mut runtime = BackgroundRuntime::default();
        let current = runtime.spawn(
            TuiJobKind::PrSummary,
            TuiJobKey::None,
            1,
            None,
            "current".into(),
            |_| Ok(None),
        );
        let stale = runtime.spawn(
            TuiJobKind::PrSummary,
            TuiJobKey::None,
            0,
            None,
            "stale".into(),
            |_| {
                std::thread::sleep(Duration::from_millis(50));
                Ok(None)
            },
        );
        runtime.cancel_stale_except(&BTreeSet::from([current]));
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let mut stale_canceled = false;
        while std::time::Instant::now() < deadline && runtime.has_jobs() {
            for message in runtime.drain_terminals(8) {
                if let JobMessage::Terminal { metadata, outcome } = message
                    && metadata.id == stale
                {
                    stale_canceled = matches!(outcome, crate::tui_jobs::JobOutcome::Canceled);
                }
            }
            std::thread::yield_now();
        }
        assert!(
            stale_canceled,
            "stale generation must route a canceled terminal"
        );
    }

    #[test]
    fn marker_admission_stays_closed_until_startup_load_finishes() {
        let mut runtime = BackgroundRuntime::default();
        let repository = WorktreeRepositoryKey::new(PathBuf::from("repo"));
        let target = RemoteMutationTarget::Unknown {
            marker_id: "startup".into(),
        };

        assert!(runtime.marker_blocks(&repository, &target));
        runtime.apply_loaded_markers(BTreeSet::from([repository.clone()]), BTreeMap::new());
        assert!(!runtime.marker_blocks(&repository, &target));
    }

    #[test]
    fn superseded_marker_write_is_not_reported_as_persisted() {
        let temp = std::env::temp_dir().join(format!(
            "prism-superseded-marker-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&temp).unwrap();
        let database_path = temp.join("prism.db");
        let target = RemoteMutationTarget::Unknown {
            marker_id: "same".into(),
        };
        let marker = |version, job_id| RemoteMutationReconciliationMarker {
            target: target.clone(),
            ledger: None,
            database_path: database_path.clone(),
            job_id,
            reason: "uncertain".into(),
            recorded_unix_ms: version,
        };
        update_persisted_remote_mutation_markers(&database_path, |markers| {
            markers.push(marker(2, 2));
        })
        .unwrap();

        let persisted = persist_marker_once(
            &MarkerPersistenceRequest {
                marker: marker(1, 1),
            },
            &std::sync::atomic::AtomicUsize::new(0),
        )
        .unwrap();

        assert!(!persisted);
        let encoded = crate::persistence::database::load_metadata(
            &database_path,
            super::super::REMOTE_MUTATION_RECONCILIATION_KEY,
        )
        .unwrap()
        .unwrap();
        let markers =
            serde_json::from_str::<Vec<RemoteMutationReconciliationMarker>>(&encoded).unwrap();
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].recorded_unix_ms, 2);
        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn failed_reconciliation_job_releases_the_retry_key() {
        let mut runtime = BackgroundRuntime::default();
        let key = (PathBuf::from("repo"), 1, 2);

        assert!(runtime.begin_reconciliation(key.clone()));
        runtime.track_reconciliation_job(3, key.clone());
        runtime.fail_reconciliation_job(3);

        assert!(runtime.begin_reconciliation(key));
    }

    #[test]
    fn stale_reconciliation_cannot_clear_newer_marker() {
        let mut runtime = BackgroundRuntime::default();
        let root = PathBuf::from("repo");
        let target = RemoteMutationTarget::Unknown {
            marker_id: "same".into(),
        };
        let marker = |version, job_id| RemoteMutationReconciliationMarker {
            target: target.clone(),
            ledger: None,
            database_path: PathBuf::from("db"),
            job_id,
            reason: "uncertain".into(),
            recorded_unix_ms: version,
        };
        runtime.upsert_marker(root.clone(), marker(2, 2));
        runtime.finish_reconciliation(&root, 1, 1, &target, true);
        assert_eq!(runtime.markers(&root).unwrap()[0].recorded_unix_ms, 2);
    }
}
