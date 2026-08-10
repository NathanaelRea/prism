//! Worker-owned pacing, coalescing, and freshness for Prism provider operations.
//!
//! Provider adapters translate provider-specific requests and responses. This module owns the
//! user-wide queue policy: callers never sleep for rate limits or run independent retry loops.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub type RemoteFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, RemoteCoordinatorError>> + Send + 'a>>;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct RemoteLaneKey {
    pub canonical_host: String,
    pub credential_profile: String,
}

impl RemoteLaneKey {
    pub fn new(
        canonical_host: impl Into<String>,
        credential_profile: impl Into<String>,
    ) -> Result<Self, RemoteCoordinatorError> {
        let canonical_host = canonical_host.into().trim().to_ascii_lowercase();
        let credential_profile = credential_profile.into().trim().to_string();
        if canonical_host.is_empty()
            || credential_profile.is_empty()
            || canonical_host.chars().any(char::is_control)
            || credential_profile.chars().any(char::is_control)
        {
            return Err(RemoteCoordinatorError::Invalid(
                "remote lane host and credential profile must be non-empty".into(),
            ));
        }
        Ok(Self {
            canonical_host,
            credential_profile,
        })
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct RemoteObservationKey {
    pub lane: RemoteLaneKey,
    /// Provider-neutral operation name, for example `change_request.readiness`.
    pub operation: String,
    /// Canonical provider subject identity.
    pub subject: String,
}

impl RemoteObservationKey {
    pub fn new(
        lane: RemoteLaneKey,
        operation: impl Into<String>,
        subject: impl Into<String>,
    ) -> Result<Self, RemoteCoordinatorError> {
        let operation = operation.into();
        let subject = subject.into();
        if operation.trim().is_empty()
            || subject.trim().is_empty()
            || operation.chars().any(char::is_control)
            || subject.chars().any(char::is_control)
        {
            return Err(RemoteCoordinatorError::Invalid(
                "remote observation operation and subject must be non-empty".into(),
            ));
        }
        Ok(Self {
            lane,
            operation,
            subject,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemotePriority {
    InteractiveMutation,
    WorkflowHook,
    WorkflowObservation,
    BackgroundRefresh,
}

impl RemotePriority {
    fn rank(self) -> i64 {
        match self {
            Self::InteractiveMutation => 0,
            Self::WorkflowHook => 1,
            Self::WorkflowObservation => 2,
            Self::BackgroundRefresh => 3,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservationFreshness {
    pub max_age_ms: i64,
    /// When supplied, cached observations for another head/revision are never reused.
    pub subject_revision: Option<String>,
    /// Cached evidence from before this lifecycle cycle is never reused.
    pub not_before_unix_ms: Option<i64>,
}

impl ObservationFreshness {
    pub fn exact(subject_revision: impl Into<String>, max_age_ms: i64) -> Self {
        Self {
            max_age_ms,
            subject_revision: Some(subject_revision.into()),
            not_before_unix_ms: None,
        }
    }

    pub fn any(max_age_ms: i64) -> Self {
        Self {
            max_age_ms,
            subject_revision: None,
            not_before_unix_ms: None,
        }
    }

    pub fn not_before(mut self, unix_ms: i64) -> Self {
        self.not_before_unix_ms = Some(unix_ms);
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FreshObservation<T> {
    pub value: T,
    pub observed_unix_ms: i64,
    pub subject_revision: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RemoteWait {
    pub summary: String,
    pub wake_at_unix_ms: i64,
    pub queue_position: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RemoteObservationResult<T> {
    Fresh(FreshObservation<T>),
    Pending(RemoteWait),
    Failed(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RemoteMutationResult<T> {
    Applied(T),
    Pending(RemoteWait),
    Failed(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemoteObservationRequest {
    pub key: RemoteObservationKey,
    pub freshness_max_age_ms: i64,
    pub requested_subject_revision: Option<String>,
    pub priority: RemotePriority,
    #[serde(default)]
    pub payload: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemoteMutationRequest {
    pub lane: RemoteLaneKey,
    /// Stable reconciliation identity. Reusing it queues only one mutation.
    pub request_id: String,
    pub operation: String,
    pub subject: String,
    pub priority: RemotePriority,
    #[serde(default)]
    pub payload: serde_json::Value,
}

#[derive(Clone, Debug)]
pub enum CoordinatedRemoteOperation {
    Observe(RemoteObservationRequest),
    Mutate(RemoteMutationRequest),
}

#[derive(Clone, Debug)]
pub struct RemoteOperationOutput {
    pub value: serde_json::Value,
    pub subject_revision: String,
    pub response_bytes: usize,
    pub retry_after_unix_ms: Option<i64>,
    pub rate_limit_reset_unix_ms: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct RemoteOperationFailure {
    pub reason: String,
    pub retryable: bool,
    pub retry_after_unix_ms: Option<i64>,
    pub rate_limit_reset_unix_ms: Option<i64>,
}

pub trait RemoteOperationExecutor: Send + Sync + 'static {
    fn execute<'a>(
        &'a self,
        operation: CoordinatedRemoteOperation,
    ) -> Pin<
        Box<dyn Future<Output = Result<RemoteOperationOutput, RemoteOperationFailure>> + Send + 'a>,
    >;
}

pub trait RemoteClock: Send + Sync + 'static {
    fn now_unix_ms(&self) -> i64;
}

#[derive(Clone, Default)]
pub struct SystemRemoteClock;

impl RemoteClock for SystemRemoteClock {
    fn now_unix_ms(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|duration| i64::try_from(duration.as_millis()).ok())
            .unwrap_or(0)
    }
}

#[derive(Clone, Default)]
pub struct FakeRemoteClock(Arc<AtomicI64>);

impl FakeRemoteClock {
    pub fn new(now_unix_ms: i64) -> Self {
        Self(Arc::new(AtomicI64::new(now_unix_ms)))
    }

    pub fn set(&self, now_unix_ms: i64) {
        self.0.store(now_unix_ms, Ordering::Release);
    }

    pub fn advance(&self, milliseconds: i64) {
        self.0.fetch_add(milliseconds, Ordering::AcqRel);
    }
}

impl RemoteClock for FakeRemoteClock {
    fn now_unix_ms(&self) -> i64 {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PersistedRemoteLane {
    pub key: RemoteLaneKey,
    pub next_request_unix_ms: i64,
    pub retry_count: u32,
    pub updated_unix_ms: i64,
}

pub trait RemoteCoordinatorStore: Send + Sync + 'static {
    fn load_lanes<'a>(&'a self) -> RemoteFuture<'a, Vec<PersistedRemoteLane>>;
    fn save_lane<'a>(&'a self, lane: &'a PersistedRemoteLane) -> RemoteFuture<'a, ()>;
}

#[derive(Default)]
pub struct MemoryRemoteCoordinatorStore {
    lanes: std::sync::Mutex<BTreeMap<RemoteLaneKey, PersistedRemoteLane>>,
}

impl RemoteCoordinatorStore for MemoryRemoteCoordinatorStore {
    fn load_lanes<'a>(&'a self) -> RemoteFuture<'a, Vec<PersistedRemoteLane>> {
        Box::pin(async move { Ok(self.lanes.lock().unwrap().values().cloned().collect()) })
    }

    fn save_lane<'a>(&'a self, lane: &'a PersistedRemoteLane) -> RemoteFuture<'a, ()> {
        Box::pin(async move {
            self.lanes
                .lock()
                .unwrap()
                .insert(lane.key.clone(), lane.clone());
            Ok(())
        })
    }
}

#[derive(Clone, Debug)]
pub struct RemoteCoordinatorConfig {
    pub minimum_start_delay_ms: i64,
    pub base_backoff_ms: i64,
    pub maximum_backoff_ms: i64,
    pub aging_interval_ms: i64,
    pub maximum_queue_length: usize,
    pub maximum_cache_entries: usize,
    pub maximum_response_bytes: usize,
    pub maximum_retries: u32,
}

impl Default for RemoteCoordinatorConfig {
    fn default() -> Self {
        Self {
            minimum_start_delay_ms: 250,
            base_backoff_ms: 1_000,
            maximum_backoff_ms: 60_000,
            aging_interval_ms: 5_000,
            maximum_queue_length: 1_024,
            maximum_cache_entries: 512,
            maximum_response_bytes: 4 * 1024 * 1024,
            maximum_retries: 6,
        }
    }
}

#[derive(Clone)]
pub struct RemoteRequestCoordinator {
    executor: Arc<dyn RemoteOperationExecutor>,
    clock: Arc<dyn RemoteClock>,
    store: Arc<dyn RemoteCoordinatorStore>,
    config: RemoteCoordinatorConfig,
    state: Arc<tokio::sync::Mutex<CoordinatorState>>,
}

#[derive(Default)]
struct CoordinatorState {
    lanes: BTreeMap<RemoteLaneKey, LaneState>,
    cache: BTreeMap<RemoteObservationKey, CachedObservation>,
    queued_reads: BTreeMap<RemoteObservationKey, QueuedRequest>,
    queued_mutations: BTreeMap<String, QueuedRequest>,
    in_flight_reads: BTreeSet<RemoteObservationKey>,
    subscriptions: BTreeMap<RemoteObservationKey, tokio::sync::watch::Sender<u64>>,
    sequence: u64,
}

#[derive(Clone, Debug, Default)]
struct LaneState {
    next_request_unix_ms: i64,
    retry_count: u32,
    in_flight: bool,
}

#[derive(Clone, Debug)]
struct QueuedRequest {
    lane: RemoteLaneKey,
    priority: RemotePriority,
    enqueued_unix_ms: i64,
    sequence: u64,
}

#[derive(Clone, Debug)]
struct CachedObservation {
    value: serde_json::Value,
    observed_unix_ms: i64,
    subject_revision: String,
}

impl RemoteRequestCoordinator {
    pub async fn new(
        executor: Arc<dyn RemoteOperationExecutor>,
        clock: Arc<dyn RemoteClock>,
        store: Arc<dyn RemoteCoordinatorStore>,
        config: RemoteCoordinatorConfig,
    ) -> Result<Self, RemoteCoordinatorError> {
        validate_config(&config)?;
        let persisted = store.load_lanes().await?;
        let mut state = CoordinatorState::default();
        for lane in persisted {
            state.lanes.insert(
                lane.key,
                LaneState {
                    next_request_unix_ms: lane.next_request_unix_ms,
                    retry_count: lane.retry_count,
                    in_flight: false,
                },
            );
        }
        Ok(Self {
            executor,
            clock,
            store,
            config,
            state: Arc::new(tokio::sync::Mutex::new(state)),
        })
    }

    pub async fn observe<T: DeserializeOwned>(
        &self,
        key: RemoteObservationKey,
        freshness: ObservationFreshness,
        priority: RemotePriority,
        payload: serde_json::Value,
    ) -> Result<RemoteObservationResult<T>, RemoteCoordinatorError> {
        if freshness.max_age_ms < 0 {
            return Err(RemoteCoordinatorError::Invalid(
                "observation freshness must not be negative".into(),
            ));
        }
        let now = self.clock.now_unix_ms();
        let (request, lane_to_save) = {
            let mut state = self.state.lock().await;
            if let Some(cached) = state.cache.get(&key)
                && cached_is_fresh(cached, &freshness, now)
            {
                return decode_fresh(cached);
            }
            if state.in_flight_reads.contains(&key) {
                return Ok(RemoteObservationResult::Pending(RemoteWait {
                    summary: format!(
                        "waiting for coalesced {} observation",
                        key.lane.canonical_host
                    ),
                    wake_at_unix_ms: now.saturating_add(self.config.minimum_start_delay_ms.max(1)),
                    queue_position: 1,
                }));
            }
            if !state.queued_reads.contains_key(&key) {
                ensure_queue_capacity(&state, &self.config)?;
                let sequence = next_sequence(&mut state);
                state.queued_reads.insert(
                    key.clone(),
                    QueuedRequest {
                        lane: key.lane.clone(),
                        priority,
                        enqueued_unix_ms: now,
                        sequence,
                    },
                );
            }
            let lane = state.lanes.entry(key.lane.clone()).or_default().clone();
            if lane.in_flight || lane.next_request_unix_ms > now {
                let wait = queue_wait(&state, &key.lane, Some(&key), now, &self.config);
                return Ok(RemoteObservationResult::Pending(wait));
            }
            let selected = select_next(&state, &key.lane, now, &self.config);
            if selected.as_deref() != Some(&queue_identity_for_read(&key)) {
                return Ok(RemoteObservationResult::Pending(queue_wait(
                    &state,
                    &key.lane,
                    Some(&key),
                    now,
                    &self.config,
                )));
            }
            state.queued_reads.remove(&key);
            state.in_flight_reads.insert(key.clone());
            let lane = state.lanes.entry(key.lane.clone()).or_default();
            lane.in_flight = true;
            lane.next_request_unix_ms = now.saturating_add(self.config.minimum_start_delay_ms);
            let persisted = persisted_lane(&key.lane, lane, now);
            (
                RemoteObservationRequest {
                    key: key.clone(),
                    freshness_max_age_ms: freshness.max_age_ms,
                    requested_subject_revision: freshness.subject_revision.clone(),
                    priority,
                    payload,
                },
                persisted,
            )
        };
        self.store.save_lane(&lane_to_save).await?;
        let result = self
            .executor
            .execute(CoordinatedRemoteOperation::Observe(request))
            .await;
        self.finish_observation(&key, result, now).await
    }

    pub async fn mutate<T: DeserializeOwned>(
        &self,
        request: RemoteMutationRequest,
    ) -> Result<RemoteMutationResult<T>, RemoteCoordinatorError> {
        if request.request_id.trim().is_empty()
            || request.operation.trim().is_empty()
            || request.subject.trim().is_empty()
        {
            return Err(RemoteCoordinatorError::Invalid(
                "remote mutation identity, operation, and subject must be non-empty".into(),
            ));
        }
        let now = self.clock.now_unix_ms();
        let persisted = {
            let mut state = self.state.lock().await;
            if !state.queued_mutations.contains_key(&request.request_id) {
                ensure_queue_capacity(&state, &self.config)?;
                let sequence = next_sequence(&mut state);
                state.queued_mutations.insert(
                    request.request_id.clone(),
                    QueuedRequest {
                        lane: request.lane.clone(),
                        priority: request.priority,
                        enqueued_unix_ms: now,
                        sequence,
                    },
                );
            }
            let lane = state.lanes.entry(request.lane.clone()).or_default().clone();
            if lane.in_flight || lane.next_request_unix_ms > now {
                return Ok(RemoteMutationResult::Pending(queue_wait(
                    &state,
                    &request.lane,
                    None,
                    now,
                    &self.config,
                )));
            }
            let selected = select_next(&state, &request.lane, now, &self.config);
            if selected.as_deref() != Some(&queue_identity_for_mutation(&request.request_id)) {
                return Ok(RemoteMutationResult::Pending(queue_wait(
                    &state,
                    &request.lane,
                    None,
                    now,
                    &self.config,
                )));
            }
            state.queued_mutations.remove(&request.request_id);
            let lane = state.lanes.entry(request.lane.clone()).or_default();
            lane.in_flight = true;
            lane.next_request_unix_ms = now.saturating_add(self.config.minimum_start_delay_ms);
            persisted_lane(&request.lane, lane, now)
        };
        self.store.save_lane(&persisted).await?;
        let lane_key = request.lane.clone();
        let result = self
            .executor
            .execute(CoordinatedRemoteOperation::Mutate(request))
            .await;
        self.finish_mutation(&lane_key, result, now).await
    }

    pub async fn subscribe(&self, key: &RemoteObservationKey) -> tokio::sync::watch::Receiver<u64> {
        let mut state = self.state.lock().await;
        state
            .subscriptions
            .entry(key.clone())
            .or_insert_with(|| tokio::sync::watch::channel(0).0)
            .subscribe()
    }

    pub async fn lane_cooldown(&self, key: &RemoteLaneKey) -> Option<i64> {
        self.state
            .lock()
            .await
            .lanes
            .get(key)
            .map(|lane| lane.next_request_unix_ms)
    }

    async fn finish_observation<T: DeserializeOwned>(
        &self,
        key: &RemoteObservationKey,
        result: Result<RemoteOperationOutput, RemoteOperationFailure>,
        started_unix_ms: i64,
    ) -> Result<RemoteObservationResult<T>, RemoteCoordinatorError> {
        let now = self.clock.now_unix_ms().max(started_unix_ms);
        match result {
            Ok(output) => {
                if output.response_bytes > self.config.maximum_response_bytes {
                    self.release_lane(&key.lane, now, None, false).await?;
                    return Ok(RemoteObservationResult::Failed(format!(
                        "provider response exceeded {} bytes",
                        self.config.maximum_response_bytes
                    )));
                }
                let cooldown =
                    maximum_time([output.retry_after_unix_ms, output.rate_limit_reset_unix_ms]);
                self.release_lane(&key.lane, now, cooldown, true).await?;
                let cached = CachedObservation {
                    value: output.value,
                    observed_unix_ms: now,
                    subject_revision: output.subject_revision,
                };
                {
                    let mut state = self.state.lock().await;
                    state.in_flight_reads.remove(key);
                    if state.cache.len() >= self.config.maximum_cache_entries
                        && let Some(oldest) = state
                            .cache
                            .iter()
                            .min_by_key(|(_, value)| value.observed_unix_ms)
                            .map(|(key, _)| key.clone())
                    {
                        state.cache.remove(&oldest);
                    }
                    state.cache.insert(key.clone(), cached.clone());
                    notify_subscribers(&mut state, key);
                }
                decode_fresh(&cached)
            }
            Err(failure) if failure.retryable => {
                let wait = self
                    .retry_lane(
                        &key.lane,
                        now,
                        failure.retry_after_unix_ms,
                        failure.rate_limit_reset_unix_ms,
                        &failure.reason,
                    )
                    .await?;
                let mut state = self.state.lock().await;
                state.in_flight_reads.remove(key);
                notify_subscribers(&mut state, key);
                Ok(match wait {
                    Some(wait) => RemoteObservationResult::Pending(wait),
                    None => RemoteObservationResult::Failed(format!(
                        "provider retry limit exhausted: {}",
                        bounded_reason(&failure.reason)
                    )),
                })
            }
            Err(failure) => {
                self.release_lane(&key.lane, now, None, false).await?;
                let mut state = self.state.lock().await;
                state.in_flight_reads.remove(key);
                notify_subscribers(&mut state, key);
                Ok(RemoteObservationResult::Failed(bounded_reason(
                    &failure.reason,
                )))
            }
        }
    }

    async fn finish_mutation<T: DeserializeOwned>(
        &self,
        lane: &RemoteLaneKey,
        result: Result<RemoteOperationOutput, RemoteOperationFailure>,
        started_unix_ms: i64,
    ) -> Result<RemoteMutationResult<T>, RemoteCoordinatorError> {
        let now = self.clock.now_unix_ms().max(started_unix_ms);
        match result {
            Ok(output) => {
                if output.response_bytes > self.config.maximum_response_bytes {
                    self.release_lane(lane, now, None, false).await?;
                    return Ok(RemoteMutationResult::Failed(format!(
                        "provider response exceeded {} bytes",
                        self.config.maximum_response_bytes
                    )));
                }
                self.release_lane(
                    lane,
                    now,
                    maximum_time([output.retry_after_unix_ms, output.rate_limit_reset_unix_ms]),
                    true,
                )
                .await?;
                serde_json::from_value(output.value)
                    .map(RemoteMutationResult::Applied)
                    .map_err(|error| RemoteCoordinatorError::Decode(error.to_string()))
            }
            Err(failure) if failure.retryable => Ok(
                match self
                    .retry_lane(
                        lane,
                        now,
                        failure.retry_after_unix_ms,
                        failure.rate_limit_reset_unix_ms,
                        &failure.reason,
                    )
                    .await?
                {
                    Some(wait) => RemoteMutationResult::Pending(wait),
                    None => RemoteMutationResult::Failed(format!(
                        "provider retry limit exhausted: {}",
                        bounded_reason(&failure.reason)
                    )),
                },
            ),
            Err(failure) => {
                self.release_lane(lane, now, None, false).await?;
                Ok(RemoteMutationResult::Failed(bounded_reason(
                    &failure.reason,
                )))
            }
        }
    }

    async fn retry_lane(
        &self,
        key: &RemoteLaneKey,
        now: i64,
        retry_after: Option<i64>,
        reset: Option<i64>,
        reason: &str,
    ) -> Result<Option<RemoteWait>, RemoteCoordinatorError> {
        let persisted = {
            let mut state = self.state.lock().await;
            let lane = state.lanes.entry(key.clone()).or_default();
            lane.in_flight = false;
            lane.retry_count = lane.retry_count.saturating_add(1);
            if lane.retry_count > self.config.maximum_retries {
                lane.next_request_unix_ms = now;
                persisted_lane(key, lane, now)
            } else {
                let exponential = self
                    .config
                    .base_backoff_ms
                    .saturating_mul(
                        1_i64
                            .checked_shl(lane.retry_count.min(30))
                            .unwrap_or(i64::MAX),
                    )
                    .min(self.config.maximum_backoff_ms);
                let jitter_bound = (exponential / 5).max(1);
                let jitter = i64::try_from(
                    stable_hash(&format!(
                        "{}:{}:{}",
                        key.canonical_host, key.credential_profile, lane.retry_count
                    )) % u64::try_from(jitter_bound).unwrap_or(1),
                )
                .unwrap_or(0);
                lane.next_request_unix_ms = maximum_time([
                    Some(now.saturating_add(exponential).saturating_add(jitter)),
                    retry_after,
                    reset,
                ])
                .unwrap_or(now.saturating_add(exponential));
                persisted_lane(key, lane, now)
            }
        };
        self.store.save_lane(&persisted).await?;
        if persisted.retry_count > self.config.maximum_retries {
            Ok(None)
        } else {
            Ok(Some(RemoteWait {
                summary: format!(
                    "{}; {} request delayed until {}",
                    bounded_reason(reason),
                    key.canonical_host,
                    persisted.next_request_unix_ms
                ),
                wake_at_unix_ms: persisted.next_request_unix_ms,
                queue_position: 1,
            }))
        }
    }

    async fn release_lane(
        &self,
        key: &RemoteLaneKey,
        now: i64,
        cooldown: Option<i64>,
        succeeded: bool,
    ) -> Result<(), RemoteCoordinatorError> {
        let persisted = {
            let mut state = self.state.lock().await;
            let lane = state.lanes.entry(key.clone()).or_default();
            lane.in_flight = false;
            if succeeded {
                lane.retry_count = 0;
            }
            if let Some(cooldown) = cooldown {
                lane.next_request_unix_ms = lane.next_request_unix_ms.max(cooldown);
            }
            persisted_lane(key, lane, now)
        };
        self.store.save_lane(&persisted).await
    }
}

fn validate_config(config: &RemoteCoordinatorConfig) -> Result<(), RemoteCoordinatorError> {
    if config.minimum_start_delay_ms < 0
        || config.base_backoff_ms <= 0
        || config.maximum_backoff_ms < config.base_backoff_ms
        || config.aging_interval_ms <= 0
        || config.maximum_queue_length == 0
        || config.maximum_cache_entries == 0
        || config.maximum_response_bytes == 0
        || config.maximum_retries == 0
    {
        return Err(RemoteCoordinatorError::Invalid(
            "remote coordinator limits must be positive and internally consistent".into(),
        ));
    }
    Ok(())
}

fn ensure_queue_capacity(
    state: &CoordinatorState,
    config: &RemoteCoordinatorConfig,
) -> Result<(), RemoteCoordinatorError> {
    if state.queued_reads.len() + state.queued_mutations.len() >= config.maximum_queue_length {
        Err(RemoteCoordinatorError::QueueFull(
            config.maximum_queue_length,
        ))
    } else {
        Ok(())
    }
}

fn next_sequence(state: &mut CoordinatorState) -> u64 {
    state.sequence = state.sequence.saturating_add(1);
    state.sequence
}

fn cached_is_fresh(cached: &CachedObservation, freshness: &ObservationFreshness, now: i64) -> bool {
    now.saturating_sub(cached.observed_unix_ms) <= freshness.max_age_ms
        && freshness
            .not_before_unix_ms
            .is_none_or(|minimum| cached.observed_unix_ms >= minimum)
        && freshness
            .subject_revision
            .as_deref()
            .is_none_or(|revision| revision == cached.subject_revision)
}

fn decode_fresh<T: DeserializeOwned>(
    cached: &CachedObservation,
) -> Result<RemoteObservationResult<T>, RemoteCoordinatorError> {
    serde_json::from_value(cached.value.clone())
        .map(|value| {
            RemoteObservationResult::Fresh(FreshObservation {
                value,
                observed_unix_ms: cached.observed_unix_ms,
                subject_revision: cached.subject_revision.clone(),
            })
        })
        .map_err(|error| RemoteCoordinatorError::Decode(error.to_string()))
}

fn select_next(
    state: &CoordinatorState,
    lane: &RemoteLaneKey,
    now: i64,
    config: &RemoteCoordinatorConfig,
) -> Option<String> {
    state
        .queued_reads
        .iter()
        .filter(|(_, queued)| &queued.lane == lane)
        .map(|(key, queued)| (queue_identity_for_read(key), queued))
        .chain(
            state
                .queued_mutations
                .iter()
                .filter(|(_, queued)| &queued.lane == lane)
                .map(|(id, queued)| (queue_identity_for_mutation(id), queued)),
        )
        .min_by_key(|(_, queued)| effective_order(queued, now, config))
        .map(|(identity, _)| identity)
}

fn effective_order(
    queued: &QueuedRequest,
    now: i64,
    config: &RemoteCoordinatorConfig,
) -> (i64, u64) {
    let age = now
        .saturating_sub(queued.enqueued_unix_ms)
        .checked_div(config.aging_interval_ms)
        .unwrap_or(0);
    (
        queued.priority.rank().saturating_mul(4) - age,
        queued.sequence,
    )
}

fn queue_wait(
    state: &CoordinatorState,
    lane: &RemoteLaneKey,
    read: Option<&RemoteObservationKey>,
    now: i64,
    config: &RemoteCoordinatorConfig,
) -> RemoteWait {
    let mut ordered = state
        .queued_reads
        .iter()
        .filter(|(_, queued)| &queued.lane == lane)
        .map(|(key, queued)| (queue_identity_for_read(key), queued))
        .chain(
            state
                .queued_mutations
                .iter()
                .filter(|(_, queued)| &queued.lane == lane)
                .map(|(id, queued)| (queue_identity_for_mutation(id), queued)),
        )
        .collect::<Vec<_>>();
    ordered.sort_by_key(|(_, queued)| effective_order(queued, now, config));
    let identity = read.map(queue_identity_for_read);
    let position = identity
        .as_ref()
        .and_then(|identity| ordered.iter().position(|(item, _)| item == identity))
        .map_or(ordered.len().max(1), |position| position + 1);
    let lane_state = state.lanes.get(lane).cloned().unwrap_or_default();
    let wake_at = lane_state
        .next_request_unix_ms
        .max(now.saturating_add(config.minimum_start_delay_ms.max(1)));
    RemoteWait {
        summary: if lane_state.next_request_unix_ms > now {
            format!(
                "waiting for {} request slot; position {position}",
                lane.canonical_host
            )
        } else {
            format!(
                "queued for {} request slot; position {position}",
                lane.canonical_host
            )
        },
        wake_at_unix_ms: wake_at,
        queue_position: position,
    }
}

fn queue_identity_for_read(key: &RemoteObservationKey) -> String {
    format!("read:{}:{}", key.operation, key.subject)
}

fn queue_identity_for_mutation(id: &str) -> String {
    format!("mutation:{id}")
}

fn persisted_lane(key: &RemoteLaneKey, lane: &LaneState, now: i64) -> PersistedRemoteLane {
    PersistedRemoteLane {
        key: key.clone(),
        next_request_unix_ms: lane.next_request_unix_ms,
        retry_count: lane.retry_count,
        updated_unix_ms: now,
    }
}

fn notify_subscribers(state: &mut CoordinatorState, key: &RemoteObservationKey) {
    if let Some(sender) = state.subscriptions.get(key) {
        let _ = sender.send(sender.borrow().saturating_add(1));
    }
}

fn maximum_time<const N: usize>(values: [Option<i64>; N]) -> Option<i64> {
    values.into_iter().flatten().max()
}

fn bounded_reason(reason: &str) -> String {
    const LIMIT: usize = 512;
    let mut reason = reason.split_whitespace().collect::<Vec<_>>().join(" ");
    if reason.len() > LIMIT {
        reason.truncate(LIMIT);
        while !reason.is_char_boundary(reason.len()) {
            reason.pop();
        }
    }
    reason
}

fn stable_hash(value: &str) -> u64 {
    value.bytes().fold(0xcbf29ce484222325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    })
}

#[derive(Debug)]
pub enum RemoteCoordinatorError {
    Invalid(String),
    QueueFull(usize),
    Decode(String),
    Persistence(String),
    Io(std::io::Error),
}

impl std::fmt::Display for RemoteCoordinatorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(error) | Self::Decode(error) | Self::Persistence(error) => {
                formatter.write_str(error)
            }
            Self::QueueFull(limit) => write!(formatter, "remote request queue is full ({limit})"),
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for RemoteCoordinatorError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct RecordingExecutor {
        calls: AtomicUsize,
        retry_after: AtomicI64,
    }

    impl RemoteOperationExecutor for RecordingExecutor {
        fn execute<'a>(
            &'a self,
            operation: CoordinatedRemoteOperation,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<RemoteOperationOutput, RemoteOperationFailure>>
                    + Send
                    + 'a,
            >,
        > {
            self.calls.fetch_add(1, Ordering::AcqRel);
            let retry_after = self.retry_after.swap(0, Ordering::AcqRel);
            Box::pin(async move {
                let revision = match operation {
                    CoordinatedRemoteOperation::Observe(request) => request
                        .requested_subject_revision
                        .unwrap_or_else(|| "head".into()),
                    CoordinatedRemoteOperation::Mutate(_) => "mutation".into(),
                };
                if retry_after > 0 {
                    return Err(RemoteOperationFailure {
                        reason: "rate limited".into(),
                        retryable: true,
                        retry_after_unix_ms: Some(retry_after),
                        rate_limit_reset_unix_ms: None,
                    });
                }
                Ok(RemoteOperationOutput {
                    value: serde_json::json!({"head": revision}),
                    subject_revision: revision,
                    response_bytes: 32,
                    retry_after_unix_ms: None,
                    rate_limit_reset_unix_ms: None,
                })
            })
        }
    }

    fn lane() -> RemoteLaneKey {
        RemoteLaneKey::new("github.com", "default").unwrap()
    }

    fn key(subject: &str) -> RemoteObservationKey {
        RemoteObservationKey::new(lane(), "change_request", subject).unwrap()
    }

    async fn coordinator(
        executor: Arc<RecordingExecutor>,
        clock: Arc<FakeRemoteClock>,
        store: Arc<MemoryRemoteCoordinatorStore>,
    ) -> RemoteRequestCoordinator {
        RemoteRequestCoordinator::new(
            executor,
            clock,
            store,
            RemoteCoordinatorConfig {
                minimum_start_delay_ms: 10,
                aging_interval_ms: 10,
                ..RemoteCoordinatorConfig::default()
            },
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn exact_reads_are_cached_and_duplicate_requests_coalesce() {
        let executor = Arc::new(RecordingExecutor::default());
        let clock = Arc::new(FakeRemoteClock::new(100));
        let coordinator = coordinator(
            executor.clone(),
            clock.clone(),
            Arc::new(MemoryRemoteCoordinatorStore::default()),
        )
        .await;
        let first = coordinator
            .observe::<serde_json::Value>(
                key("pr:1"),
                ObservationFreshness::exact("abc", 100),
                RemotePriority::WorkflowObservation,
                serde_json::Value::Null,
            )
            .await
            .unwrap();
        assert!(matches!(first, RemoteObservationResult::Fresh(_)));
        let second = coordinator
            .observe::<serde_json::Value>(
                key("pr:1"),
                ObservationFreshness::exact("abc", 100),
                RemotePriority::BackgroundRefresh,
                serde_json::Value::Null,
            )
            .await
            .unwrap();
        assert!(matches!(second, RemoteObservationResult::Fresh(_)));
        assert_eq!(executor.calls.load(Ordering::Acquire), 1);

        clock.advance(10);
        let new_cycle = coordinator
            .observe::<serde_json::Value>(
                key("pr:1"),
                ObservationFreshness::exact("abc", 100).not_before(110),
                RemotePriority::WorkflowObservation,
                serde_json::Value::Null,
            )
            .await
            .unwrap();
        assert!(matches!(new_cycle, RemoteObservationResult::Fresh(_)));
        assert_eq!(executor.calls.load(Ordering::Acquire), 2);

        clock.advance(101);
        let stale = coordinator
            .observe::<serde_json::Value>(
                key("pr:1"),
                ObservationFreshness::exact("abc", 100),
                RemotePriority::WorkflowObservation,
                serde_json::Value::Null,
            )
            .await
            .unwrap();
        assert!(matches!(stale, RemoteObservationResult::Fresh(_)));
        assert_eq!(executor.calls.load(Ordering::Acquire), 3);
    }

    #[tokio::test]
    async fn one_host_lane_paces_repositories_and_interactive_queue_wins() {
        let executor = Arc::new(RecordingExecutor::default());
        let clock = Arc::new(FakeRemoteClock::new(100));
        let coordinator = coordinator(
            executor.clone(),
            clock.clone(),
            Arc::new(MemoryRemoteCoordinatorStore::default()),
        )
        .await;
        assert!(matches!(
            coordinator
                .observe::<serde_json::Value>(
                    key("repo-a:1"),
                    ObservationFreshness::any(0),
                    RemotePriority::BackgroundRefresh,
                    serde_json::Value::Null,
                )
                .await
                .unwrap(),
            RemoteObservationResult::Fresh(_)
        ));
        assert!(matches!(
            coordinator
                .observe::<serde_json::Value>(
                    key("repo-b:2"),
                    ObservationFreshness::any(0),
                    RemotePriority::WorkflowObservation,
                    serde_json::Value::Null,
                )
                .await
                .unwrap(),
            RemoteObservationResult::Pending(_)
        ));
        clock.advance(10);
        assert!(matches!(
            coordinator
                .mutate::<serde_json::Value>(RemoteMutationRequest {
                    lane: lane(),
                    request_id: "interactive".into(),
                    operation: "resolve".into(),
                    subject: "repo-a:1".into(),
                    priority: RemotePriority::InteractiveMutation,
                    payload: serde_json::Value::Null,
                })
                .await
                .unwrap(),
            RemoteMutationResult::Applied(_)
        ));
        assert_eq!(executor.calls.load(Ordering::Acquire), 2);
    }

    #[tokio::test]
    async fn retry_after_is_durable_across_restart() {
        let executor = Arc::new(RecordingExecutor::default());
        executor.retry_after.store(1_000, Ordering::Release);
        let clock = Arc::new(FakeRemoteClock::new(100));
        let store = Arc::new(MemoryRemoteCoordinatorStore::default());
        let first = coordinator(executor.clone(), clock.clone(), store.clone()).await;
        let wait = first
            .observe::<serde_json::Value>(
                key("pr:1"),
                ObservationFreshness::exact("abc", 0),
                RemotePriority::WorkflowObservation,
                serde_json::Value::Null,
            )
            .await
            .unwrap();
        let wake_at = match wait {
            RemoteObservationResult::Pending(wait) => wait.wake_at_unix_ms,
            other => panic!("expected a durable Wait, got {other:?}"),
        };
        assert!(wake_at >= 1_000);
        drop(first);
        let second = coordinator(executor.clone(), clock, store).await;
        assert_eq!(second.lane_cooldown(&lane()).await, Some(wake_at));
        assert!(matches!(
            second
                .observe::<serde_json::Value>(
                    key("pr:2"),
                    ObservationFreshness::any(0),
                    RemotePriority::WorkflowObservation,
                    serde_json::Value::Null,
                )
                .await
                .unwrap(),
            RemoteObservationResult::Pending(RemoteWait {
                wake_at_unix_ms,
                ..
            }) if wake_at_unix_ms == wake_at
        ));
        assert_eq!(executor.calls.load(Ordering::Acquire), 1);
    }
}
