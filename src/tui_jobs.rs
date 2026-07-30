use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub(crate) type JobId = u64;

#[derive(Clone, Debug)]
pub(crate) struct JobMetadata<K, Q> {
    pub id: JobId,
    pub kind: K,
    pub key: Q,
    pub generation: u64,
    pub started_at: Instant,
    pub deadline: Option<Instant>,
    diagnostic_kind: Option<&'static str>,
}

#[derive(Debug)]
pub(crate) enum JobOutcome {
    Completed,
    Failed(String),
    SpawnFailed(std::io::Error),
    Panicked(String),
    Canceled,
    DeadlineExceeded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JobOutcomeKind {
    Completed,
    Failed,
    SpawnFailed,
    Panicked,
    Canceled,
    DeadlineExceeded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum CoalescedFacet {
    Status,
    Message,
}

const COALESCED_FACET_COUNT: usize = 2;

impl JobOutcomeKind {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::SpawnFailed => "spawn_failed",
            Self::Panicked => "panicked",
            Self::Canceled => "canceled",
            Self::DeadlineExceeded => "deadline_exceeded",
        }
    }
}

impl JobOutcome {
    pub(crate) const fn kind(&self) -> JobOutcomeKind {
        match self {
            Self::Completed => JobOutcomeKind::Completed,
            Self::Failed(_) => JobOutcomeKind::Failed,
            Self::SpawnFailed(_) => JobOutcomeKind::SpawnFailed,
            Self::Panicked(_) => JobOutcomeKind::Panicked,
            Self::Canceled => JobOutcomeKind::Canceled,
            Self::DeadlineExceeded => JobOutcomeKind::DeadlineExceeded,
        }
    }

    pub(crate) fn error_message(&self) -> Option<String> {
        match self {
            Self::Failed(error) | Self::Panicked(error) => Some(error.clone()),
            Self::SpawnFailed(error) => Some(error.to_string()),
            Self::Completed | Self::Canceled | Self::DeadlineExceeded => None,
        }
    }
}

pub(crate) enum JobMessage<K, Q, P> {
    Payload {
        metadata: JobMetadata<K, Q>,
        payload: P,
    },
    Terminal {
        metadata: JobMetadata<K, Q>,
        outcome: JobOutcome,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct QueueStats {
    pub event_depth: usize,
    pub event_capacity: usize,
    pub coalesced_depth: usize,
    pub coalesced_capacity: usize,
    pub latest_depth: usize,
    pub terminal_depth: usize,
    pub overflow_total: u64,
    pub overflow_delta: u64,
    pub coalesced_total: u64,
    pub coalesced_delta: u64,
    pub dirty: bool,
}

pub(crate) struct LatestSender<K, T> {
    values: Arc<Mutex<BTreeMap<K, T>>>,
    key: fn(&T) -> K,
}

pub(crate) struct LatestReceiver<K, T> {
    values: Arc<Mutex<BTreeMap<K, T>>>,
}

impl<K, T> Clone for LatestSender<K, T> {
    fn clone(&self) -> Self {
        Self {
            values: self.values.clone(),
            key: self.key,
        }
    }
}

impl<K: Ord, T> LatestSender<K, T> {
    pub(crate) fn send(&self, value: T) -> Result<(), String> {
        let key = (self.key)(&value);
        self.values
            .lock()
            .map_err(|_| "latest-state delivery lock poisoned".to_string())?
            .insert(key, value);
        Ok(())
    }
}

impl<K: Ord + Clone, T> LatestReceiver<K, T> {
    pub(crate) fn try_recv(&self) -> Result<T, ()> {
        let mut values = self.values.lock().map_err(|_| ())?;
        let key = values.keys().next().cloned().ok_or(())?;
        values.remove(&key).ok_or(())
    }

    #[cfg(test)]
    pub(crate) fn recv_timeout(&self, timeout: Duration) -> Result<T, ()> {
        let started = Instant::now();
        loop {
            if let Ok(value) = self.try_recv() {
                return Ok(value);
            }
            if started.elapsed() >= timeout {
                return Err(());
            }
            thread::yield_now();
        }
    }
}

pub(crate) fn latest_channel<K: Ord, T>(
    key: fn(&T) -> K,
) -> (LatestSender<K, T>, LatestReceiver<K, T>) {
    let values = Arc::new(Mutex::new(BTreeMap::new()));
    (
        LatestSender {
            values: values.clone(),
            key,
        },
        LatestReceiver { values },
    )
}

struct CancellationState {
    canceled: Arc<AtomicBool>,
    mutex: Mutex<()>,
    wake: Condvar,
}

type CoalescedSlots<K, Q, P> = BTreeMap<(JobId, CoalescedFacet), StreamPayload<K, Q, P>>;

struct EventDelivery<K, Q, P> {
    tx: mpsc::SyncSender<StreamPayload<K, Q, P>>,
    depth: AtomicUsize,
    capacity: usize,
    coalesced: Mutex<CoalescedSlots<K, Q, P>>,
    next_sequence: AtomicU64,
    coalesced_total: AtomicU64,
    overflow_total: AtomicU64,
    dirty: AtomicBool,
}

struct StreamPayload<K, Q, P> {
    sequence: u64,
    metadata: JobMetadata<K, Q>,
    payload: P,
}

pub(crate) struct JobContext<K, Q, P> {
    metadata: JobMetadata<K, Q>,
    cancellation: Arc<CancellationState>,
    dirty: Arc<AtomicBool>,
    delivery: Arc<EventDelivery<K, Q, P>>,
}

impl<K: Clone, Q: Clone, P> Clone for JobContext<K, Q, P> {
    fn clone(&self) -> Self {
        Self {
            metadata: self.metadata.clone(),
            cancellation: self.cancellation.clone(),
            dirty: self.dirty.clone(),
            delivery: self.delivery.clone(),
        }
    }
}

impl<K: Clone, Q: Clone, P> JobContext<K, Q, P> {
    pub(crate) fn id(&self) -> JobId {
        self.metadata.id
    }

    pub(crate) fn is_canceled(&self) -> bool {
        self.cancellation.canceled.load(Ordering::Acquire)
            || self
                .metadata
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
    }

    /// Ordered streaming payloads are best effort. A full queue marks the stream
    /// dirty; the consumer reconciles from authoritative state instead of blocking.
    pub(crate) fn send(&self, payload: P) -> Result<(), String> {
        self.send_inner(payload, None)
    }

    /// Supersedable snapshots use one bounded overflow slot per job and facet.
    /// Neither the queue nor the slot path ever blocks the producer.
    pub(crate) fn send_coalesced(&self, facet: CoalescedFacet, payload: P) -> Result<(), String> {
        self.send_inner(payload, Some(facet))
    }

    fn send_inner(&self, payload: P, facet: Option<CoalescedFacet>) -> Result<(), String> {
        if self.is_canceled() {
            return Err("job canceled".to_string());
        }
        let message = StreamPayload {
            sequence: self.delivery.next_sequence.fetch_add(1, Ordering::AcqRel),
            metadata: self.metadata.clone(),
            payload,
        };
        self.delivery.depth.fetch_add(1, Ordering::AcqRel);
        match self.delivery.tx.try_send(message) {
            Ok(()) => Ok(()),
            Err(mpsc::TrySendError::Full(message)) => {
                self.delivery.depth.fetch_sub(1, Ordering::AcqRel);
                self.delivery.overflow_total.fetch_add(1, Ordering::AcqRel);
                self.delivery.dirty.store(true, Ordering::Release);
                self.dirty.store(true, Ordering::Release);
                if let Some(facet) = facet {
                    let mut slots = match self.delivery.coalesced.try_lock() {
                        Ok(slots) => slots,
                        Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
                        Err(std::sync::TryLockError::WouldBlock) => {
                            self.record_delivery_pressure("dropped", Some(facet), false);
                            return Ok(());
                        }
                    };
                    if self.is_canceled() {
                        return Err("job canceled".to_string());
                    }
                    let operation = match slots.entry((message.metadata.id, facet)) {
                        std::collections::btree_map::Entry::Vacant(entry) => {
                            entry.insert(message);
                            "overflow_retained"
                        }
                        std::collections::btree_map::Entry::Occupied(mut entry) => {
                            if message.sequence > entry.get().sequence {
                                entry.insert(message);
                            }
                            self.delivery.coalesced_total.fetch_add(1, Ordering::AcqRel);
                            "coalesced"
                        }
                    };
                    drop(slots);
                    self.record_delivery_pressure(operation, Some(facet), true);
                    return Ok(());
                }
                self.record_delivery_pressure("dropped", None, false);
                Ok(())
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                self.delivery.depth.fetch_sub(1, Ordering::AcqRel);
                Err("job delivery receiver disconnected".to_string())
            }
        }
    }

    fn record_delivery_pressure(
        &self,
        operation: &'static str,
        facet: Option<CoalescedFacet>,
        retained: bool,
    ) {
        let mut fields = vec![
            crate::flight_recorder::unsigned("job_id", self.metadata.id),
            crate::flight_recorder::text(
                "job_type",
                self.metadata.diagnostic_kind.unwrap_or("unknown"),
            ),
            crate::flight_recorder::unsigned(
                "queue_depth",
                self.delivery.depth.load(Ordering::Acquire),
            ),
            crate::flight_recorder::unsigned("queue_capacity", self.delivery.capacity),
            crate::flight_recorder::boolean("retained", retained),
        ];
        if let Some(facet) = facet {
            fields.push(crate::flight_recorder::text(
                "facet",
                match facet {
                    CoalescedFacet::Status => "status",
                    CoalescedFacet::Message => "message",
                },
            ));
        }
        crate::flight_recorder::record("queue", operation, None, fields);
    }

    pub(crate) fn wait(&self, duration: Duration) -> bool {
        if self.is_canceled() {
            return true;
        }
        let wait = self
            .metadata
            .deadline
            .map(|deadline| {
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(duration)
            })
            .unwrap_or(duration);
        let guard = self
            .cancellation
            .mutex
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _ = self
            .cancellation
            .wake
            .wait_timeout_while(guard, wait, |_| !self.is_canceled());
        self.is_canceled()
    }
}

struct JobCompletion<P> {
    outcome: JobOutcome,
    payload: Option<P>,
}

struct JobDelivery {
    timeout: Option<Duration>,
    coalesce_payload: bool,
    diagnostic_kind: Option<&'static str>,
}

pub(crate) struct JobDiagnostic {
    pub timeout: Option<Duration>,
    pub kind: &'static str,
}

enum JobState<P> {
    Running(JoinHandle<JobCompletion<P>>),
    Finished(JobOutcome),
}

struct JobEntry<K, Q, P> {
    metadata: JobMetadata<K, Q>,
    cancellation: Arc<CancellationState>,
    dirty: Arc<AtomicBool>,
    coalesce_payload: bool,
    state: JobState<P>,
}

type LatestKey<K, Q> = (K, Q, u64, JobId);
type LatestValue<K, Q, P> = (JobMetadata<K, Q>, P);

pub(crate) struct JobRegistry<K, Q, P> {
    next_id: JobId,
    accepting: bool,
    jobs: BTreeMap<JobId, JobEntry<K, Q, P>>,
    latest: BTreeMap<LatestKey<K, Q>, LatestValue<K, Q, P>>,
    delivery: Arc<EventDelivery<K, Q, P>>,
    event_rx: mpsc::Receiver<StreamPayload<K, Q, P>>,
    pending_event: Option<StreamPayload<K, Q, P>>,
    pending_dirty: BTreeMap<JobId, JobMetadata<K, Q>>,
    overflow_reported: u64,
    coalesced_reported: u64,
    #[cfg(test)]
    fail_next_spawn: bool,
}

const DEFAULT_EVENT_CAPACITY: usize = 256;

impl<K, Q, P> Default for JobRegistry<K, Q, P> {
    fn default() -> Self {
        Self::with_event_capacity(DEFAULT_EVENT_CAPACITY)
    }
}

impl<K, Q, P> JobRegistry<K, Q, P> {
    pub(crate) fn with_event_capacity(capacity: usize) -> Self {
        let (tx, event_rx) = mpsc::sync_channel(capacity);
        Self {
            next_id: 1,
            accepting: true,
            jobs: BTreeMap::new(),
            latest: BTreeMap::new(),
            delivery: Arc::new(EventDelivery {
                tx,
                depth: AtomicUsize::new(0),
                capacity,
                coalesced: Mutex::new(BTreeMap::new()),
                next_sequence: AtomicU64::new(0),
                coalesced_total: AtomicU64::new(0),
                overflow_total: AtomicU64::new(0),
                dirty: AtomicBool::new(false),
            }),
            event_rx,
            pending_event: None,
            pending_dirty: BTreeMap::new(),
            overflow_reported: 0,
            coalesced_reported: 0,
            #[cfg(test)]
            fail_next_spawn: false,
        }
    }
}

impl<K, Q, P> JobRegistry<K, Q, P>
where
    K: Clone + Ord + Send + 'static,
    Q: Clone + Ord + Send + 'static,
    P: Send + 'static,
{
    #[cfg(test)]
    pub(crate) fn spawn<F>(
        &mut self,
        kind: K,
        key: Q,
        generation: u64,
        timeout: Option<Duration>,
        name: String,
        job: F,
    ) -> JobId
    where
        F: FnOnce(JobContext<K, Q, P>) -> Result<Option<P>, String> + Send + 'static,
    {
        self.spawn_with_delivery(
            kind,
            key,
            generation,
            name,
            JobDelivery {
                timeout,
                coalesce_payload: true,
                diagnostic_kind: None,
            },
            job,
        )
    }

    #[cfg(test)]
    pub(crate) fn spawn_reliable<F>(
        &mut self,
        kind: K,
        key: Q,
        generation: u64,
        timeout: Option<Duration>,
        name: String,
        job: F,
    ) -> JobId
    where
        F: FnOnce(JobContext<K, Q, P>) -> Result<Option<P>, String> + Send + 'static,
    {
        self.spawn_with_delivery(
            kind,
            key,
            generation,
            name,
            JobDelivery {
                timeout,
                coalesce_payload: false,
                diagnostic_kind: None,
            },
            job,
        )
    }

    pub(crate) fn spawn_diagnostic<F>(
        &mut self,
        kind: K,
        key: Q,
        generation: u64,
        name: String,
        diagnostic: JobDiagnostic,
        job: F,
    ) -> JobId
    where
        F: FnOnce(JobContext<K, Q, P>) -> Result<Option<P>, String> + Send + 'static,
    {
        self.spawn_with_delivery(
            kind,
            key,
            generation,
            name,
            JobDelivery {
                timeout: diagnostic.timeout,
                coalesce_payload: true,
                diagnostic_kind: Some(diagnostic.kind),
            },
            job,
        )
    }

    pub(crate) fn spawn_reliable_diagnostic<F>(
        &mut self,
        kind: K,
        key: Q,
        generation: u64,
        name: String,
        diagnostic: JobDiagnostic,
        job: F,
    ) -> JobId
    where
        F: FnOnce(JobContext<K, Q, P>) -> Result<Option<P>, String> + Send + 'static,
    {
        self.spawn_with_delivery(
            kind,
            key,
            generation,
            name,
            JobDelivery {
                timeout: diagnostic.timeout,
                coalesce_payload: false,
                diagnostic_kind: Some(diagnostic.kind),
            },
            job,
        )
    }

    fn spawn_with_delivery<F>(
        &mut self,
        kind: K,
        key: Q,
        generation: u64,
        name: String,
        delivery: JobDelivery,
        job: F,
    ) -> JobId
    where
        F: FnOnce(JobContext<K, Q, P>) -> Result<Option<P>, String> + Send + 'static,
    {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let started_at = Instant::now();
        let metadata = JobMetadata {
            id,
            kind,
            key,
            generation,
            started_at,
            deadline: delivery.timeout.map(|timeout| started_at + timeout),
            diagnostic_kind: delivery.diagnostic_kind,
        };
        let cancellation = Arc::new(CancellationState {
            canceled: Arc::new(AtomicBool::new(false)),
            mutex: Mutex::new(()),
            wake: Condvar::new(),
        });
        let dirty = Arc::new(AtomicBool::new(false));
        if !self.accepting {
            self.insert_finished(
                metadata,
                cancellation,
                dirty,
                delivery.coalesce_payload,
                JobOutcome::Failed("TUI is shutting down".to_string()),
            );
            return id;
        }

        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_spawn) {
            self.insert_finished(
                metadata,
                cancellation,
                dirty,
                delivery.coalesce_payload,
                JobOutcome::SpawnFailed(std::io::Error::other("injected thread spawn failure")),
            );
            return id;
        }

        let context = JobContext {
            metadata: metadata.clone(),
            cancellation: cancellation.clone(),
            dirty: dirty.clone(),
            delivery: self.delivery.clone(),
        };
        let terminal_metadata = metadata.clone();
        let thread = thread::Builder::new().name(name).spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                let run = || {
                    crate::process::with_cancellation(context.cancellation.canceled.clone(), || {
                        job(context.clone())
                    })
                };
                if let Some(job_type) = context.metadata.diagnostic_kind {
                    crate::flight_recorder::with_job_context(context.metadata.id, job_type, run)
                } else {
                    run()
                }
            }));
            match result {
                Err(payload) => JobCompletion {
                    outcome: JobOutcome::Panicked(panic_message(payload)),
                    payload: None,
                },
                Ok(_)
                    if terminal_metadata
                        .deadline
                        .is_some_and(|deadline| Instant::now() >= deadline) =>
                {
                    JobCompletion {
                        outcome: JobOutcome::DeadlineExceeded,
                        payload: None,
                    }
                }
                Ok(_) if context.is_canceled() => JobCompletion {
                    outcome: JobOutcome::Canceled,
                    payload: None,
                },
                Ok(Ok(payload)) => JobCompletion {
                    outcome: JobOutcome::Completed,
                    payload,
                },
                Ok(Err(error)) => JobCompletion {
                    outcome: JobOutcome::Failed(error),
                    payload: None,
                },
            }
        });
        match thread {
            Ok(handle) => {
                self.jobs.insert(
                    id,
                    JobEntry {
                        metadata,
                        cancellation,
                        dirty,
                        coalesce_payload: delivery.coalesce_payload,
                        state: JobState::Running(handle),
                    },
                );
            }
            Err(error) => self.insert_finished(
                metadata,
                cancellation,
                dirty,
                delivery.coalesce_payload,
                JobOutcome::SpawnFailed(error),
            ),
        }
        id
    }

    #[cfg(test)]
    pub(crate) fn collect_finished(&mut self) {
        self.collect_finished_limit(usize::MAX);
    }

    fn collect_finished_limit(&mut self, limit: usize) {
        self.cancel_expired();
        let finished = self
            .jobs
            .iter()
            .filter_map(|(id, entry)| match &entry.state {
                JobState::Running(handle) if handle.is_finished() => Some(*id),
                JobState::Running(_) | JobState::Finished(_) => None,
            })
            .take(limit)
            .collect::<Vec<_>>();
        for id in finished {
            let Some(entry) = self.jobs.remove(&id) else {
                continue;
            };
            let JobState::Running(handle) = entry.state else {
                unreachable!();
            };
            let completion = handle.join().unwrap_or_else(|payload| JobCompletion {
                outcome: JobOutcome::Panicked(panic_message(payload)),
                payload: None,
            });
            if let Some(payload) = completion.payload {
                let key = (
                    entry.metadata.kind.clone(),
                    entry.metadata.key.clone(),
                    entry.metadata.generation,
                    entry.metadata.id,
                );
                let current_key = entry
                    .coalesce_payload
                    .then(|| {
                        self.latest
                            .keys()
                            .find(|(kind, key, generation, _)| {
                                kind == &entry.metadata.kind
                                    && key == &entry.metadata.key
                                    && generation == &entry.metadata.generation
                            })
                            .cloned()
                    })
                    .flatten();
                if let Some(current_key) = current_key {
                    self.delivery.coalesced_total.fetch_add(1, Ordering::AcqRel);
                    crate::flight_recorder::record(
                        "queue",
                        "coalesced",
                        None,
                        vec![
                            crate::flight_recorder::unsigned("job_id", entry.metadata.id),
                            crate::flight_recorder::text(
                                "job_type",
                                entry.metadata.diagnostic_kind.unwrap_or("unknown"),
                            ),
                            crate::flight_recorder::text("facet", "completion"),
                            crate::flight_recorder::boolean("retained", true),
                        ],
                    );
                    if current_key.3 > entry.metadata.id {
                        self.jobs.insert(
                            id,
                            JobEntry {
                                metadata: entry.metadata,
                                cancellation: entry.cancellation,
                                dirty: entry.dirty,
                                coalesce_payload: entry.coalesce_payload,
                                state: JobState::Finished(completion.outcome),
                            },
                        );
                        continue;
                    }
                    self.latest.remove(&current_key);
                }
                self.latest.insert(key, (entry.metadata.clone(), payload));
            }
            entry.cancellation.canceled.store(true, Ordering::Release);
            self.clear_coalesced(id);
            self.jobs.insert(
                id,
                JobEntry {
                    metadata: entry.metadata,
                    cancellation: entry.cancellation,
                    dirty: entry.dirty,
                    coalesce_payload: entry.coalesce_payload,
                    state: JobState::Finished(completion.outcome),
                },
            );
        }
    }

    pub(crate) fn drain_terminals(&mut self, limit: usize) -> Vec<JobMessage<K, Q, P>> {
        self.collect_finished_limit(limit);
        let ids = self
            .jobs
            .iter()
            .filter_map(|(id, entry)| matches!(entry.state, JobState::Finished(_)).then_some(*id))
            .take(limit)
            .collect::<Vec<_>>();
        ids.into_iter()
            .filter_map(|id| {
                let entry = self.jobs.remove(&id)?;
                if entry.dirty.swap(false, Ordering::AcqRel) {
                    self.pending_dirty
                        .insert(entry.metadata.id, entry.metadata.clone());
                }
                let JobState::Finished(outcome) = entry.state else {
                    unreachable!();
                };
                Some(JobMessage::Terminal {
                    metadata: entry.metadata,
                    outcome,
                })
            })
            .collect()
    }

    pub(crate) fn take_latest_by<F>(&mut self, mut priority: F) -> Option<JobMessage<K, Q, P>>
    where
        F: FnMut(&JobMetadata<K, Q>) -> u8,
    {
        let key = self
            .latest
            .iter()
            .min_by_key(|(_, (metadata, _))| (priority(metadata), metadata.id))
            .map(|(key, _)| key.clone())?;
        let (metadata, payload) = self.latest.remove(&key)?;
        Some(JobMessage::Payload { metadata, payload })
    }

    pub(crate) fn latest_min_priority<F>(&self, mut priority: F) -> Option<u8>
    where
        F: FnMut(&JobMetadata<K, Q>) -> u8,
    {
        self.latest
            .values()
            .map(|(metadata, _)| priority(metadata))
            .min()
    }

    pub(crate) fn take_stream_event(&mut self) -> Option<JobMessage<K, Q, P>> {
        if self.pending_event.is_none()
            && let Ok(message) = self.event_rx.try_recv()
        {
            self.delivery.depth.fetch_sub(1, Ordering::AcqRel);
            self.pending_event = Some(message);
        }
        let mut slots = self
            .delivery
            .coalesced
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let coalesced_key = slots
            .iter()
            .min_by_key(|(_, value)| value.sequence)
            .map(|(key, _)| *key);
        let take_coalesced = coalesced_key.is_some_and(|key| {
            self.pending_event
                .as_ref()
                .is_none_or(|pending| slots[&key].sequence < pending.sequence)
        });
        let value = if take_coalesced {
            slots.remove(&coalesced_key?)?
        } else {
            self.pending_event.take()?
        };
        Some(JobMessage::Payload {
            metadata: value.metadata,
            payload: value.payload,
        })
    }

    #[cfg(test)]
    pub(crate) fn drain_events(&mut self, limit: usize) -> Vec<JobMessage<K, Q, P>> {
        let mut messages = Vec::new();
        for _ in 0..limit {
            let message = if let Some(message) = self.pending_event.take() {
                message
            } else {
                let Ok(message) = self.event_rx.try_recv() else {
                    break;
                };
                self.delivery.depth.fetch_sub(1, Ordering::AcqRel);
                message
            };
            messages.push(JobMessage::Payload {
                metadata: message.metadata,
                payload: message.payload,
            });
        }
        messages
    }

    pub(crate) fn queue_stats(&mut self) -> QueueStats {
        let overflow_total = self.delivery.overflow_total.load(Ordering::Acquire);
        let overflow_delta = overflow_total.saturating_sub(self.overflow_reported);
        self.overflow_reported = overflow_total;
        let coalesced_total = self.delivery.coalesced_total.load(Ordering::Acquire);
        let coalesced_delta = coalesced_total.saturating_sub(self.coalesced_reported);
        self.coalesced_reported = coalesced_total;
        let coalesced_depth = self
            .delivery
            .coalesced
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .len();
        QueueStats {
            event_depth: self.delivery.depth.load(Ordering::Acquire),
            event_capacity: self.delivery.capacity,
            coalesced_depth,
            coalesced_capacity: self
                .jobs
                .values()
                .filter(|entry| matches!(entry.state, JobState::Running(_)))
                .count()
                .saturating_mul(COALESCED_FACET_COUNT),
            latest_depth: self.latest.len(),
            terminal_depth: self
                .jobs
                .values()
                .filter(|entry| matches!(entry.state, JobState::Finished(_)))
                .count(),
            overflow_total,
            overflow_delta,
            coalesced_total,
            coalesced_delta,
            dirty: self.delivery.dirty.swap(false, Ordering::AcqRel),
        }
    }

    pub(crate) fn take_dirty_jobs(&mut self) -> Vec<JobMetadata<K, Q>> {
        for entry in self.jobs.values() {
            if entry.dirty.swap(false, Ordering::AcqRel) {
                self.pending_dirty
                    .insert(entry.metadata.id, entry.metadata.clone());
            }
        }
        std::mem::take(&mut self.pending_dirty)
            .into_values()
            .collect()
    }

    pub(crate) fn stop_accepting(&mut self) {
        self.accepting = false;
    }

    pub(crate) fn cancel_all(&self) {
        for entry in self.jobs.values() {
            entry.cancellation.canceled.store(true, Ordering::Release);
            entry.cancellation.wake.notify_all();
        }
        self.delivery
            .coalesced
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
    }

    pub(crate) fn cancel(&self, id: JobId) {
        if let Some(entry) = self.jobs.get(&id) {
            entry.cancellation.canceled.store(true, Ordering::Release);
            entry.cancellation.wake.notify_all();
        }
        self.clear_coalesced(id);
    }

    pub(crate) fn active_metadata(&self) -> Vec<JobMetadata<K, Q>> {
        self.jobs
            .values()
            .filter(|entry| matches!(entry.state, JobState::Running(_)))
            .map(|entry| entry.metadata.clone())
            .collect()
    }

    pub(crate) fn has_jobs(&self) -> bool {
        !self.jobs.is_empty()
    }

    pub(crate) fn abandon_unfinished(&mut self) -> usize {
        let count = self.jobs.len();
        self.jobs.clear();
        self.latest.clear();
        self.delivery
            .coalesced
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        count
    }

    fn insert_finished(
        &mut self,
        metadata: JobMetadata<K, Q>,
        cancellation: Arc<CancellationState>,
        dirty: Arc<AtomicBool>,
        coalesce_payload: bool,
        outcome: JobOutcome,
    ) {
        self.jobs.insert(
            metadata.id,
            JobEntry {
                metadata,
                cancellation,
                dirty,
                coalesce_payload,
                state: JobState::Finished(outcome),
            },
        );
    }

    fn cancel_expired(&self) {
        let now = Instant::now();
        for entry in self.jobs.values() {
            if entry
                .metadata
                .deadline
                .is_some_and(|deadline| now >= deadline)
            {
                entry.cancellation.canceled.store(true, Ordering::Release);
                entry.cancellation.wake.notify_all();
            }
        }
    }

    fn clear_coalesced(&self, id: JobId) {
        self.delivery
            .coalesced
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retain(|(job_id, _), _| *job_id != id);
    }

    #[cfg(test)]
    pub(crate) fn fail_next_spawn(&mut self) {
        self.fail_next_spawn = true;
    }
}

impl<K, Q, P> Drop for JobRegistry<K, Q, P> {
    fn drop(&mut self) {
        self.accepting = false;
        for entry in self.jobs.values() {
            entry.cancellation.canceled.store(true, Ordering::Release);
            entry.cancellation.wake.notify_all();
        }
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::process::Command;
    use std::time::Duration;

    use super::{CoalescedFacet, JobMessage, JobOutcome, JobRegistry};

    #[test]
    fn panic_emits_one_terminal_outcome_and_a_later_job_can_start() {
        let mut jobs = JobRegistry::<&'static str, String, ()>::default();
        jobs.spawn(
            "poll",
            "same-key".to_string(),
            1,
            Some(Duration::from_secs(1)),
            "panic-job".to_string(),
            |_| panic!("before result"),
        );
        let terminal = wait_for_terminal(&mut jobs);
        assert!(matches!(terminal, JobOutcome::Panicked(message) if message == "before result"));
        assert!(!jobs.has_jobs());

        jobs.spawn(
            "poll",
            "same-key".to_string(),
            1,
            Some(Duration::from_secs(1)),
            "replacement-job".to_string(),
            |_| Ok(None),
        );
        assert!(matches!(
            wait_for_terminal(&mut jobs),
            JobOutcome::Completed
        ));
    }

    #[test]
    fn spawn_failure_emits_one_terminal_outcome_and_a_later_job_can_start() {
        let mut jobs = JobRegistry::<&'static str, String, ()>::default();
        jobs.fail_next_spawn();
        jobs.spawn(
            "poll",
            "same-key".to_string(),
            1,
            None,
            "failed-job".to_string(),
            |_| Ok(None),
        );
        let terminal = wait_for_terminal(&mut jobs);
        assert!(
            matches!(terminal, JobOutcome::SpawnFailed(error) if error.to_string().contains("spawn failure"))
        );

        jobs.spawn(
            "poll",
            "same-key".to_string(),
            2,
            None,
            "replacement-job".to_string(),
            |_| Ok(None),
        );
        assert!(matches!(
            wait_for_terminal(&mut jobs),
            JobOutcome::Completed
        ));
    }

    #[test]
    fn bounded_stream_counts_exact_overflow_without_blocking() {
        let capacity = 4;
        let burst = 1_000;
        let mut jobs =
            JobRegistry::<&'static str, &'static str, usize>::with_event_capacity(capacity);
        jobs.spawn(
            "listener",
            "stream",
            1,
            None,
            "burst".to_string(),
            move |context| {
                for value in 0..burst {
                    context.send(value)?;
                }
                Ok(None)
            },
        );
        let _ = wait_for_terminal(&mut jobs);

        let stats = jobs.queue_stats();
        assert_eq!(stats.event_depth, capacity);
        assert_eq!(stats.overflow_total, (burst - capacity) as u64);
        assert_eq!(stats.overflow_delta, (burst - capacity) as u64);
        assert!(stats.dirty);
        let dirty = jobs.take_dirty_jobs();
        assert_eq!(dirty.len(), 1);
        assert_eq!(dirty[0].key, "stream");
        assert_eq!(jobs.drain_events(usize::MAX).len(), capacity);
        assert_eq!(jobs.queue_stats().event_depth, 0);
    }

    #[test]
    fn coalesced_overflow_has_two_slots_per_active_stream_and_clears_on_cancel() {
        let capacity = 1;
        let snapshots_per_facet = 100;
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(0);
        let mut jobs =
            JobRegistry::<&'static str, &'static str, usize>::with_event_capacity(capacity);
        let id = jobs.spawn(
            "listener",
            "stream",
            1,
            None,
            "coalesced-burst".to_string(),
            move |context| {
                context.send(usize::MAX)?;
                for value in 0..snapshots_per_facet {
                    context.send_coalesced(CoalescedFacet::Status, value)?;
                    context.send_coalesced(CoalescedFacet::Message, snapshots_per_facet + value)?;
                }
                ready_tx.send(()).unwrap();
                while !context.wait(Duration::from_secs(60)) {}
                Ok(None)
            },
        );
        ready_rx.recv().unwrap();

        let stats = jobs.queue_stats();
        assert_eq!(stats.event_depth, capacity);
        assert_eq!(stats.coalesced_depth, 2);
        assert_eq!(stats.coalesced_capacity, 2);
        assert_eq!(stats.coalesced_total, 2 * (snapshots_per_facet - 1) as u64);
        assert_eq!(stats.overflow_total, (2 * snapshots_per_facet) as u64);
        assert!(stats.dirty);
        let dirty = jobs.take_dirty_jobs();
        assert_eq!(dirty.len(), 1);
        assert_eq!(dirty[0].id, id);

        jobs.cancel(id);
        assert_eq!(jobs.queue_stats().coalesced_depth, 0);
        assert!(matches!(wait_for_terminal(&mut jobs), JobOutcome::Canceled));
    }

    #[test]
    fn coalesced_snapshot_keeps_its_position_before_a_later_ordered_event() {
        let (overflowed_tx, overflowed_rx) = std::sync::mpsc::sync_channel(0);
        let (continue_tx, continue_rx) = std::sync::mpsc::sync_channel(0);
        let (ordered_tx, ordered_rx) = std::sync::mpsc::sync_channel(0);
        let mut jobs = JobRegistry::<&'static str, &'static str, usize>::with_event_capacity(1);
        let id = jobs.spawn(
            "listener",
            "stream",
            1,
            None,
            "ordered-coalesced".to_string(),
            move |context| {
                context.send_coalesced(CoalescedFacet::Status, 1)?;
                context.send_coalesced(CoalescedFacet::Status, 2)?;
                overflowed_tx.send(()).unwrap();
                continue_rx.recv().unwrap();
                context.send(3)?;
                ordered_tx.send(()).unwrap();
                while !context.wait(Duration::from_secs(60)) {}
                Ok(None)
            },
        );
        overflowed_rx.recv().unwrap();

        assert!(matches!(
            jobs.take_stream_event(),
            Some(JobMessage::Payload { payload: 1, .. })
        ));
        continue_tx.send(()).unwrap();
        ordered_rx.recv().unwrap();

        assert!(matches!(
            jobs.take_stream_event(),
            Some(JobMessage::Payload { payload: 2, .. })
        ));
        assert!(matches!(
            jobs.take_stream_event(),
            Some(JobMessage::Payload { payload: 3, .. })
        ));
        jobs.cancel(id);
        assert!(matches!(wait_for_terminal(&mut jobs), JobOutcome::Canceled));
    }

    #[test]
    fn latest_results_coalesce_by_kind_key_and_generation() {
        let mut jobs = JobRegistry::<&'static str, &'static str, usize>::default();
        for value in 0..100 {
            jobs.spawn(
                "poll",
                "same",
                7,
                None,
                format!("poll-{value}"),
                move |_| Ok(Some(value)),
            );
        }
        wait_until_all_finished(&mut jobs);

        let stats = jobs.queue_stats();
        assert_eq!(stats.latest_depth, 1);
        assert_eq!(stats.coalesced_total, 99);
        let payload = jobs.take_latest_by(|_| 0).unwrap();
        assert!(matches!(payload, JobMessage::Payload { payload: 99, .. }));
    }

    #[test]
    fn reliable_payloads_keep_one_slot_per_terminal_outcome() {
        let mut jobs = JobRegistry::<&'static str, &'static str, usize>::default();
        for value in 0..2 {
            jobs.spawn_reliable(
                "control",
                "same",
                1,
                None,
                format!("control-{value}"),
                move |_| Ok(Some(value)),
            );
        }
        wait_until_all_finished(&mut jobs);

        let stats = jobs.queue_stats();
        assert_eq!(stats.latest_depth, 2);
        assert_eq!(stats.coalesced_total, 0);
        assert!(matches!(
            jobs.take_latest_by(|_| 0),
            Some(JobMessage::Payload { payload: 0, .. })
        ));
        assert!(matches!(
            jobs.take_latest_by(|_| 0),
            Some(JobMessage::Payload { payload: 1, .. })
        ));
    }

    #[test]
    fn terminal_budget_never_discards_outcomes() {
        let mut jobs = JobRegistry::<&'static str, usize, ()>::default();
        for key in 0..20 {
            jobs.spawn("poll", key, 1, None, format!("poll-{key}"), |_| Ok(None));
        }
        wait_until_all_finished(&mut jobs);

        assert_eq!(jobs.drain_terminals(3).len(), 3);
        assert_eq!(jobs.queue_stats().terminal_depth, 17);
        assert_eq!(jobs.drain_terminals(usize::MAX).len(), 17);
        assert!(!jobs.has_jobs());
    }

    #[test]
    fn dropping_registry_interrupts_a_listener_wait() {
        let (stopped_tx, stopped_rx) = std::sync::mpsc::sync_channel(0);
        let mut jobs = JobRegistry::<&'static str, String, ()>::default();
        jobs.spawn(
            "listener",
            "server".to_string(),
            0,
            None,
            "listener-job".to_string(),
            move |context| {
                while !context.wait(Duration::from_secs(60)) {}
                stopped_tx.send(()).unwrap();
                Ok(None)
            },
        );

        drop(jobs);

        stopped_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn canceled_job_terminates_and_joins_a_supervised_domain_command() {
        let marker = std::env::temp_dir().join(format!(
            "prism-canceled-job-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let job_marker = marker.clone();
        let mut jobs = JobRegistry::<&'static str, &'static str, ()>::default();
        jobs.spawn(
            "action",
            "command",
            0,
            Some(Duration::from_secs(30)),
            "supervised-command".to_string(),
            move |_| {
                crate::process::run_status(
                    Command::new("sh")
                        .arg("-c")
                        .arg("printf ready > \"$1\"; exec sleep 30")
                        .arg("job-command")
                        .arg(&job_marker),
                    crate::process::ProcessPolicy::WorkflowStep,
                )?;
                Ok(None)
            },
        );
        let marker_deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !marker.exists() {
            assert!(std::time::Instant::now() < marker_deadline);
            std::thread::sleep(Duration::from_millis(10));
        }

        let canceled_at = std::time::Instant::now();
        jobs.stop_accepting();
        jobs.cancel_all();

        assert!(matches!(wait_for_terminal(&mut jobs), JobOutcome::Canceled));
        assert!(canceled_at.elapsed() < Duration::from_secs(1));
        assert!(!jobs.has_jobs());
        std::fs::remove_file(marker).unwrap();
    }

    fn wait_until_all_finished<K, Q, P>(jobs: &mut JobRegistry<K, Q, P>)
    where
        K: Clone + Ord + Send + 'static,
        Q: Clone + Ord + Send + 'static,
        P: Send + 'static,
    {
        let started = std::time::Instant::now();
        loop {
            jobs.collect_finished();
            if jobs
                .jobs
                .values()
                .all(|entry| matches!(entry.state, super::JobState::Finished(_)))
            {
                return;
            }
            assert!(started.elapsed() < Duration::from_secs(1));
            std::thread::yield_now();
        }
    }

    fn wait_for_terminal<K, Q, P>(jobs: &mut JobRegistry<K, Q, P>) -> JobOutcome
    where
        K: Clone + Ord + Send + 'static,
        Q: Clone + Ord + Send + 'static,
        P: Send + 'static,
    {
        let started = std::time::Instant::now();
        loop {
            if let Some(outcome) =
                jobs.drain_terminals(1)
                    .into_iter()
                    .find_map(|message| match message {
                        JobMessage::Terminal { outcome, .. } => Some(outcome),
                        JobMessage::Payload { .. } => None,
                    })
            {
                return outcome;
            }
            assert!(started.elapsed() < Duration::from_secs(1));
            std::thread::yield_now();
        }
    }
}
