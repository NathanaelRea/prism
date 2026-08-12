//! Production bridge from built-in Triggers to the shared provider request coordinator.

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::future::Future;
use std::pin::Pin;

use crate::remote::request_coordinator::{
    CoordinatedRemoteOperation, ObservationFreshness, RemoteMutationRequest, RemoteMutationResult,
    RemoteObservationKey, RemoteObservationResult, RemoteOperationExecutor, RemoteOperationFailure,
    RemoteOperationOutput, RemotePriority, RemoteRequestCoordinator,
};

use super::standard_triggers::{
    ChangeRequestObservation, MergeRelation, Mergeability, RequiredCheck, RequiredCheckState,
    ReviewThreadObservation, StandardMutationResult, StandardObservationResult, StandardProvider,
    StandardRemoteFuture, StandardTriggerRemote,
};
use super::step_trigger::{TriggerContext, TriggerError, TriggerSubject};

const OBSERVATION_MAX_AGE_MS: i64 = 5_000;

#[derive(Clone)]
pub struct ProductionStandardTriggerRemote {
    coordinator: RemoteRequestCoordinator,
    wake_sender: Option<tokio::sync::mpsc::UnboundedSender<String>>,
}

impl ProductionStandardTriggerRemote {
    pub fn new(coordinator: RemoteRequestCoordinator) -> Self {
        Self {
            coordinator,
            wake_sender: None,
        }
    }

    pub fn with_wake_sender(
        coordinator: RemoteRequestCoordinator,
        wake_sender: tokio::sync::mpsc::UnboundedSender<String>,
    ) -> Self {
        Self {
            coordinator,
            wake_sender: Some(wake_sender),
        }
    }

    pub fn coordinator(&self) -> &RemoteRequestCoordinator {
        &self.coordinator
    }
}

impl StandardTriggerRemote for ProductionStandardTriggerRemote {
    fn observe<'a>(
        &'a self,
        context: &'a TriggerContext,
    ) -> StandardRemoteFuture<'a, StandardObservationResult> {
        Box::pin(async move {
            let lane = lane_for_subject(&context.subject).await?;
            let change_request = context.subject.change_request.clone().ok_or_else(|| {
                TriggerError::Protocol("standard Trigger requires a Change Request".into())
            })?;
            let key =
                RemoteObservationKey::new(lane, "change_request.stabilization", change_request)
                    .map_err(|error| TriggerError::Protocol(error.to_string()))?;
            let freshness = ObservationFreshness::any(OBSERVATION_MAX_AGE_MS)
                .not_before(context.cycle_started_unix_ms);
            let payload = serde_json::to_value(&context.subject)
                .map_err(|error| TriggerError::Protocol(error.to_string()))?;
            // Subscribe before requesting so a fast coalesced completion cannot race the wake
            // registration.
            let mut subscription = self.coordinator.subscribe(&key).await;
            let result = self
                .coordinator
                .observe::<ChangeRequestObservation>(
                    key,
                    freshness,
                    RemotePriority::WorkflowObservation,
                    payload,
                )
                .await
                .map_err(|error| TriggerError::Protocol(error.to_string()))?;
            Ok(match result {
                RemoteObservationResult::Fresh(observation) => {
                    StandardObservationResult::Fresh(Box::new(observation.value))
                }
                RemoteObservationResult::Pending(wait) => {
                    if let Some(sender) = self.wake_sender.clone() {
                        let run_id = context.run_id.clone();
                        tokio::spawn(async move {
                            if subscription.changed().await.is_ok() {
                                let _ = sender.send(run_id);
                            }
                        });
                    }
                    StandardObservationResult::Wait {
                        summary: wait.summary,
                        wake_at_unix_ms: wait.wake_at_unix_ms,
                    }
                }
                RemoteObservationResult::Failed(reason) => StandardObservationResult::Fail(reason),
            })
        })
    }

    fn resolve_review_threads<'a>(
        &'a self,
        context: &'a TriggerContext,
        observation_revision: &'a str,
        thread_ids: &'a [String],
    ) -> StandardRemoteFuture<'a, StandardMutationResult> {
        Box::pin(async move {
            if thread_ids.is_empty() {
                return Ok(StandardMutationResult::Applied(
                    "no captured review threads needed resolution".into(),
                ));
            }
            let lane = lane_for_subject(&context.subject).await?;
            let change_request = context.subject.change_request.clone().ok_or_else(|| {
                TriggerError::Protocol("review resolution requires a Change Request".into())
            })?;
            let request = RemoteMutationRequest {
                lane,
                request_id: format!(
                    "{}:{}:{}",
                    context.run_id, context.attempt_id, observation_revision
                ),
                operation: "change_request.resolve_review_threads".into(),
                subject: change_request,
                priority: RemotePriority::WorkflowHook,
                payload: serde_json::to_value(ResolveThreadsPayload {
                    subject: context.subject.clone(),
                    observation_revision: observation_revision.to_string(),
                    thread_ids: thread_ids.to_vec(),
                })
                .map_err(|error| TriggerError::Protocol(error.to_string()))?,
            };
            Ok(
                match self
                    .coordinator
                    .mutate::<serde_json::Value>(request)
                    .await
                    .map_err(|error| TriggerError::Protocol(error.to_string()))?
                {
                    RemoteMutationResult::Applied(_) => StandardMutationResult::Applied(format!(
                        "resolved {} captured review thread(s)",
                        thread_ids.len()
                    )),
                    RemoteMutationResult::Pending(wait) => StandardMutationResult::Wait {
                        summary: wait.summary,
                        wake_at_unix_ms: wait.wake_at_unix_ms,
                    },
                    RemoteMutationResult::Failed(reason) => StandardMutationResult::Fail(reason),
                },
            )
        })
    }
}

async fn lane_for_subject(
    subject: &TriggerSubject,
) -> Result<crate::remote::request_coordinator::RemoteLaneKey, TriggerError> {
    lane_for_remote_paths(&subject.repository, &subject.worktree)
        .await
        .map_err(TriggerError::Protocol)
}

pub(crate) async fn lane_for_remote_paths(
    repository: &std::path::Path,
    worktree: &std::path::Path,
) -> Result<crate::remote::request_coordinator::RemoteLaneKey, String> {
    let repository = crate::repo::Repository {
        root: repository.to_path_buf(),
    };
    let config = crate::config::Config::load(&repository);
    if !config.config_errors.is_empty() {
        return Err(config.config_errors.join("; "));
    }
    let discovered = crate::remote::discover_git_remote(
        worktree,
        &config,
        "origin",
        crate::remote::RemoteUrlKind::Fetch,
    )
    .await
    .map_err(|error| error.to_string())?;
    crate::remote::request_coordinator::RemoteLaneKey::new(
        discovered.repository.id.host().to_string(),
        credential_profile(&config, discovered.repository.id.provider()),
    )
    .map_err(|error| error.to_string())
}

fn credential_profile(
    config: &crate::config::Config,
    provider: crate::remote::ProviderKind,
) -> String {
    // Tool selection and user config form the credential profile for CLI-backed adapters. This is
    // deliberately stable and contains no credential bytes.
    let tool = match provider {
        crate::remote::ProviderKind::GitHub => config.tool("gh"),
        crate::remote::ProviderKind::GitLab => config.tool("glab"),
        crate::remote::ProviderKind::Forgejo => "forgejo-http".to_string(),
    };
    format!(
        "{}:{:016x}",
        provider.config_label(),
        crate::util::stable_hash(std::path::Path::new(&tool))
    )
}

#[derive(Clone, Default)]
pub struct PrismProviderExecutor;

impl RemoteOperationExecutor for PrismProviderExecutor {
    fn execute<'a>(
        &'a self,
        operation: CoordinatedRemoteOperation,
    ) -> Pin<
        Box<dyn Future<Output = Result<RemoteOperationOutput, RemoteOperationFailure>> + Send + 'a>,
    > {
        Box::pin(async move { execute_async(operation).await })
    }
}

async fn execute_async(
    operation: CoordinatedRemoteOperation,
) -> Result<RemoteOperationOutput, RemoteOperationFailure> {
    match operation {
        CoordinatedRemoteOperation::Observe(request)
            if request.key.operation == "change_request.stabilization" =>
        {
            let subject: TriggerSubject = serde_json::from_value(request.payload)
                .map_err(|error| permanent(format!("invalid observation subject: {error}")))?;
            let observation = observe_change_request(&subject)
                .await
                .map_err(classify_failure)?;
            let value = serde_json::to_value(&observation)
                .map_err(|error| permanent(format!("serialize provider observation: {error}")))?;
            let response_bytes = serde_json::to_vec(&value)
                .map_err(|error| permanent(format!("measure provider observation: {error}")))?
                .len();
            Ok(RemoteOperationOutput {
                value,
                subject_revision: observation.head_sha.clone(),
                response_bytes,
                retry_after_unix_ms: None,
                rate_limit_reset_unix_ms: None,
            })
        }
        CoordinatedRemoteOperation::Observe(request)
            if request.key.operation == "tui.change_requests" =>
        {
            let payload: TuiRemoteListPayload = serde_json::from_value(request.payload)
                .map_err(|error| permanent(format!("invalid TUI remote list request: {error}")))?;
            let repository = crate::repo::Repository {
                root: payload.repository.clone(),
            };
            let config = crate::config::Config::load(&repository);
            let summaries =
                crate::remote::dispatcher::list_change_requests(&payload.worktree, &config)
                    .await
                    .map_err(classify_failure)?;
            output(summaries, "list")
        }
        CoordinatedRemoteOperation::Observe(request)
            if request.key.operation == "tui.repository_policy" =>
        {
            let payload: TuiRemoteListPayload = serde_json::from_value(request.payload)
                .map_err(|error| permanent(format!("invalid TUI policy request: {error}")))?;
            let repository = crate::repo::Repository {
                root: payload.repository.clone(),
            };
            let config = crate::config::Config::load(&repository);
            crate::remote::dispatcher::refresh_repository_policy(
                &repository,
                &payload.worktree,
                &config,
            )
            .await
            .map_err(classify_failure)?;
            output(true, "policy")
        }
        CoordinatedRemoteOperation::Observe(request)
            if request.key.operation == "tui.remote_branch_head" =>
        {
            let payload: TuiRemoteBranchHeadPayload = serde_json::from_value(request.payload)
                .map_err(|error| {
                    permanent(format!("invalid TUI remote branch request: {error}"))
                })?;
            let repository = crate::repo::Repository {
                root: payload.repository,
            };
            let config = crate::config::Config::load(&repository);
            let head = crate::git::push_remote_branch_head_sha(
                &payload.worktree,
                &payload.remote,
                &payload.branch,
                &config,
            )
            .await
            .map_err(classify_failure)?;
            output(head, "remote-branch-head")
        }
        CoordinatedRemoteOperation::Observe(request)
            if request.key.operation == "tui.change_request_cache" =>
        {
            let payload: TuiRemoteCachePayload = serde_json::from_value(request.payload)
                .map_err(|error| permanent(format!("invalid TUI remote cache request: {error}")))?;
            let repository = crate::repo::Repository {
                root: payload.repository.clone(),
            };
            let config = crate::config::Config::load(&repository);
            let mut cache = crate::remote::load_pr_cache(&repository, &payload.branch);
            crate::remote::dispatcher::refresh_change_request_cache(
                &repository,
                &payload.branch,
                &mut cache,
                &payload.worktree,
                &config,
                payload.force_details,
            )
            .await
            .map_err(classify_failure)?;
            let revision = cache
                .summary()
                .map(|summary| summary.head_sha.clone())
                .unwrap_or_else(|| "absent".into());
            output(
                crate::remote::WorkerPrCacheSnapshot::capture(&cache),
                &revision,
            )
        }
        CoordinatedRemoteOperation::Mutate(request)
            if request.operation == "change_request.resolve_review_threads" =>
        {
            let payload: ResolveThreadsPayload =
                serde_json::from_value(request.payload).map_err(|error| {
                    permanent(format!("invalid review resolution request: {error}"))
                })?;
            resolve_threads(payload).await.map_err(classify_failure)?;
            output(serde_json::json!({"resolved": true}), "mutation")
        }
        CoordinatedRemoteOperation::Mutate(request)
            if request.operation == "tui.resolve_review_threads" =>
        {
            let payload: TuiRemoteResolvePayload = serde_json::from_value(request.payload)
                .map_err(|error| permanent(format!("invalid TUI review resolution: {error}")))?;
            let repository = crate::repo::Repository {
                root: payload.repository.clone(),
            };
            let config = crate::config::Config::load(&repository);
            for thread_id in &payload.thread_ids {
                crate::remote::dispatcher::resolve_review_thread(
                    &payload.worktree,
                    &config,
                    &payload.summary,
                    thread_id,
                )
                .await
                .map_err(classify_failure)?;
            }
            output(payload.thread_ids.len(), "mutation")
        }
        CoordinatedRemoteOperation::Mutate(request) if request.operation == "tui.push_branch" => {
            let payload: TuiRemotePushPayload = serde_json::from_value(request.payload)
                .map_err(|error| permanent(format!("invalid TUI push request: {error}")))?;
            let repository = crate::repo::Repository {
                root: payload.repository.clone(),
            };
            let config = crate::config::Config::load(&repository);
            let current = crate::remote::dispatcher::prepare_push(
                &payload.worktree,
                &config,
                &payload.branch,
            )
            .await
            .map_err(classify_failure)?;
            if !crate::remote::dispatcher::same_push_target(&payload.expected, &current) {
                return Err(permanent(
                    "push remote, branch, or HEAD changed during push preparation".into(),
                ));
            }
            crate::lifecycle::push_branch(
                &config,
                &payload.worktree,
                &payload.branch,
                current.set_upstream,
            )
            .await
            .map_err(classify_failure)?;
            let mut cache = crate::remote::load_pr_cache(&repository, &payload.branch);
            crate::remote::dispatcher::refresh_change_request_cache(
                &repository,
                &payload.branch,
                &mut cache,
                &payload.worktree,
                &config,
                true,
            )
            .await
            .map_err(classify_failure)?;
            output(
                crate::remote::WorkerPrCacheSnapshot::capture(&cache),
                "mutation",
            )
        }
        CoordinatedRemoteOperation::Mutate(request)
            if request.operation == "tui.fetch_change_request" =>
        {
            let payload: TuiRemoteFetchPayload = serde_json::from_value(request.payload)
                .map_err(|error| permanent(format!("invalid TUI fetch request: {error}")))?;
            let repository = crate::repo::Repository {
                root: payload.repository,
            };
            let config = crate::config::Config::load(&repository);
            crate::remote::dispatcher::fetch_change_request_branch(
                &payload.worktree,
                &config,
                &payload.summary,
                &payload.branch,
            )
            .await
            .map_err(classify_failure)?;
            output(true, "mutation")
        }
        CoordinatedRemoteOperation::Mutate(request) if request.operation == "tui.submit_review" => {
            let payload: TuiRemoteReviewPayload = serde_json::from_value(request.payload)
                .map_err(|error| permanent(format!("invalid TUI review submission: {error}")))?;
            let repository = crate::repo::Repository {
                root: payload.repository,
            };
            let config = crate::config::Config::load(&repository);
            crate::remote::dispatcher::submit_review(
                &payload.worktree,
                &config,
                &payload.summary,
                payload.kind,
                payload.body,
            )
            .await
            .map_err(classify_failure)?;
            output(serde_json::json!({"submitted": true}), "mutation")
        }
        CoordinatedRemoteOperation::Observe(request) => Err(permanent(format!(
            "unsupported coordinated observation '{}'",
            request.key.operation
        ))),
        CoordinatedRemoteOperation::Mutate(request) => Err(permanent(format!(
            "unsupported coordinated mutation '{}'",
            request.operation
        ))),
    }
}

async fn observe_change_request(
    subject: &TriggerSubject,
) -> Result<ChangeRequestObservation, String> {
    let repository = crate::repo::Repository {
        root: subject.repository.clone(),
    };
    let config = crate::config::Config::load(&repository);
    if !config.config_errors.is_empty() {
        return Err(config.config_errors.join("; "));
    }
    let branch = crate::git::current_branch_name(&subject.worktree, &config)
        .await?
        .ok_or_else(|| "standard Workflow Triggers do not support detached HEAD".to_string())?;
    let mut cache = crate::remote::load_pr_cache(&repository, &branch);
    crate::remote::dispatcher::refresh_change_request_cache(
        &repository,
        &branch,
        &mut cache,
        &subject.worktree,
        &config,
        true,
    )
    .await?;
    let summary = cache.summary().cloned().ok_or_else(|| {
        "no open Change Request is associated with the selected worktree".to_string()
    })?;
    let details = cache
        .details()
        .cloned()
        .ok_or_else(|| "Change Request details are temporarily unavailable".to_string())?;
    let policy = crate::remote::dispatcher::refresh_repository_policy(
        &repository,
        &subject.worktree,
        &config,
    )
    .await?;
    let identity = summary
        .change_request_identity
        .as_ref()
        .ok_or_else(|| "Change Request has no canonical provider identity".to_string())?;
    let observed_change_request = format!(
        "{}:{}:{}:change_request:{}",
        identity.provider().config_label(),
        identity.canonical_host(),
        identity.project_path(),
        identity.native_id()
    );
    if subject
        .change_request
        .as_deref()
        .is_some_and(|expected| expected != observed_change_request)
    {
        return Err("a different Change Request is now associated with the worktree".into());
    }
    let provider = match identity.provider() {
        crate::remote::ProviderKind::GitHub => StandardProvider::GitHub,
        crate::remote::ProviderKind::GitLab => StandardProvider::GitLab,
        crate::remote::ProviderKind::Forgejo => StandardProvider::Forgejo,
    };
    let target_repository = identity
        .target_repository()
        .map_err(|error| error.to_string())?;
    let target_remote = crate::remote::dispatcher::fetch_remote_name_for_repository(
        &subject.worktree,
        &config,
        &target_repository,
    )
    .await?;
    let merge_relation = match summary
        .merge_state_status
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "behind" => MergeRelation::Behind,
        "dirty" | "conflicting" | "conflict" => MergeRelation::Conflicting,
        "clean" | "blocked" | "unstable" | "has_hooks" => MergeRelation::Current,
        _ => MergeRelation::Unknown,
    };
    let mergeability = match summary
        .merge_state_status
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "clean" | "unstable" | "has_hooks" => Mergeability::Mergeable,
        "dirty" | "conflicting" | "conflict" => Mergeability::Conflicting,
        "blocked" => Mergeability::Blocked,
        _ => Mergeability::Unknown,
    };
    let unresolved_threads = details
        .review_comments
        .iter()
        .filter(|thread| !thread.resolved && !thread.thread_id.trim().is_empty())
        .map(|thread| ReviewThreadObservation {
            id: thread.thread_id.clone(),
            revision: thread_revision(thread),
        })
        .collect::<Vec<_>>();
    let required_checks = required_checks(&policy.required_checks, &details.check_contexts);
    let mut policy_blockers = Vec::new();
    if policy.merge_queue_required {
        policy_blockers
            .push("merge queue is required and automatic queueing is not supported".into());
    }
    let unsupported = capability_gap(identity.provider(), &policy, &details);
    let observation_revision =
        observation_revision(&summary, &unresolved_threads, &required_checks);
    Ok(ChangeRequestObservation {
        provider,
        change_request: subject
            .change_request
            .clone()
            .unwrap_or_else(|| format!("{}#{}", target_repository, summary.number)),
        head_sha: summary.head_sha,
        observation_revision,
        target_remote,
        target_branch: summary.base_ref,
        merge_relation,
        mergeability,
        unresolved_threads,
        required_review_pending: matches!(
            summary.review_decision.trim().to_ascii_lowercase().as_str(),
            "review_required" | "review required" | "pending"
        ),
        required_checks,
        draft: summary.draft,
        lifecycle_open: summary.state.eq_ignore_ascii_case("open") && !summary.merged,
        policy_blockers,
        unsupported,
    })
}

fn required_checks(
    required_names: &[String],
    checks: &[crate::remote::PrCheckContext],
) -> Vec<RequiredCheck> {
    let selected = if required_names.is_empty() {
        checks.to_vec()
    } else {
        required_names
            .iter()
            .map(|name| {
                checks
                    .iter()
                    .find(|check| check.name == *name)
                    .cloned()
                    .unwrap_or(crate::remote::PrCheckContext {
                        name: name.clone(),
                        state: crate::remote::PrCheckState::Unknown,
                    })
            })
            .collect::<Vec<_>>()
    };
    selected
        .into_iter()
        .map(|check| RequiredCheck {
            name: check.name,
            state: match check.state {
                crate::remote::PrCheckState::Pending => RequiredCheckState::Pending,
                crate::remote::PrCheckState::Success => RequiredCheckState::Passed,
                crate::remote::PrCheckState::Failed => RequiredCheckState::Failed,
                crate::remote::PrCheckState::Mixed => RequiredCheckState::Failed,
                crate::remote::PrCheckState::Unknown => RequiredCheckState::Unknown,
            },
        })
        .collect()
}

fn capability_gap(
    provider: crate::remote::ProviderKind,
    policy: &crate::remote::RepoPolicyCache,
    details: &crate::remote::PrDetails,
) -> Option<String> {
    if let Some(error) = policy.error.as_deref() {
        return Some(format!(
            "provider policy observation is unavailable: {error}"
        ));
    }
    match provider {
        crate::remote::ProviderKind::GitHub => None,
        crate::remote::ProviderKind::GitLab if details.check_contexts.is_empty() => Some(
            "GitLab required-check observations are not supported for this Change Request".into(),
        ),
        crate::remote::ProviderKind::Forgejo if details.check_contexts.is_empty() => Some(
            "Forgejo required-check observations are not supported for this Change Request".into(),
        ),
        _ => None,
    }
}

fn output(
    value: impl Serialize,
    subject_revision: &str,
) -> Result<RemoteOperationOutput, RemoteOperationFailure> {
    let value = serde_json::to_value(value)
        .map_err(|error| permanent(format!("serialize provider response: {error}")))?;
    let response_bytes = serde_json::to_vec(&value)
        .map_err(|error| permanent(format!("measure provider response: {error}")))?
        .len();
    Ok(RemoteOperationOutput {
        value,
        subject_revision: subject_revision.to_string(),
        response_bytes,
        retry_after_unix_ms: None,
        rate_limit_reset_unix_ms: None,
    })
}

async fn resolve_threads(payload: ResolveThreadsPayload) -> Result<(), String> {
    let repository = crate::repo::Repository {
        root: payload.subject.repository.clone(),
    };
    let config = crate::config::Config::load(&repository);
    let branch = crate::git::current_branch_name(&payload.subject.worktree, &config)
        .await?
        .ok_or_else(|| "cannot resolve review threads from detached HEAD".to_string())?;
    let mut cache = crate::remote::load_pr_cache(&repository, &branch);
    crate::remote::dispatcher::refresh_change_request_cache(
        &repository,
        &branch,
        &mut cache,
        &payload.subject.worktree,
        &config,
        true,
    )
    .await?;
    let summary = cache
        .summary()
        .ok_or_else(|| "Change Request summary is unavailable for review resolution".to_string())?;
    let identity = summary.change_request_identity.as_ref().ok_or_else(|| {
        "Change Request identity is unavailable for review resolution".to_string()
    })?;
    let observed_change_request = format!(
        "{}:{}:{}:change_request:{}",
        identity.provider().config_label(),
        identity.canonical_host(),
        identity.project_path(),
        identity.native_id()
    );
    if payload
        .subject
        .change_request
        .as_deref()
        .is_some_and(|expected| expected != observed_change_request)
    {
        return Err("a different Change Request is now associated with the worktree".into());
    }
    let unresolved = cache
        .details()
        .into_iter()
        .flat_map(|details| &details.review_comments)
        .filter(|thread| !thread.resolved)
        .map(|thread| thread.thread_id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    for thread_id in &payload.thread_ids {
        if unresolved.contains(thread_id.as_str()) {
            crate::remote::dispatcher::resolve_review_thread(
                &payload.subject.worktree,
                &config,
                summary,
                thread_id,
            )
            .await?;
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct TuiRemoteListPayload {
    pub repository: std::path::PathBuf,
    pub worktree: std::path::PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct TuiRemoteBranchHeadPayload {
    pub repository: std::path::PathBuf,
    pub worktree: std::path::PathBuf,
    pub remote: String,
    pub branch: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct TuiRemoteCachePayload {
    pub repository: std::path::PathBuf,
    pub worktree: std::path::PathBuf,
    pub branch: String,
    pub force_details: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct TuiRemotePushPayload {
    pub repository: std::path::PathBuf,
    pub worktree: std::path::PathBuf,
    pub branch: String,
    pub expected: crate::remote::dispatcher::PushGuard,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct TuiRemoteFetchPayload {
    pub repository: std::path::PathBuf,
    pub worktree: std::path::PathBuf,
    pub branch: String,
    pub summary: crate::remote::PrSummary,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct TuiRemoteResolvePayload {
    pub repository: std::path::PathBuf,
    pub worktree: std::path::PathBuf,
    pub summary: crate::remote::PrSummary,
    pub thread_ids: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct TuiRemoteReviewPayload {
    pub repository: std::path::PathBuf,
    pub worktree: std::path::PathBuf,
    pub summary: crate::remote::PrSummary,
    pub kind: crate::remote::ReviewSubmissionKind,
    pub body: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ResolveThreadsPayload {
    subject: TriggerSubject,
    observation_revision: String,
    thread_ids: Vec<String>,
}

fn thread_revision(thread: &crate::remote::PrReviewComment) -> String {
    format!(
        "sha256:{:x}",
        Sha256::digest(
            serde_json::to_vec(&(
                &thread.thread_id,
                &thread.id,
                &thread.author,
                &thread.path,
                &thread.line,
                &thread.body,
                thread.resolved,
            ))
            .unwrap_or_default()
        )
    )
}

fn observation_revision(
    summary: &crate::remote::PrSummary,
    threads: &[ReviewThreadObservation],
    checks: &[RequiredCheck],
) -> String {
    format!(
        "sha256:{:x}",
        Sha256::digest(
            serde_json::to_vec(&(summary.signature(), threads, checks)).unwrap_or_default()
        )
    )
}

fn classify_failure(reason: String) -> RemoteOperationFailure {
    let normalized = reason.to_ascii_lowercase();
    let retry_after_unix_ms = retry_after_delay_ms(&normalized)
        .map(|delay| crate::workflow::prompt_worker::now_unix_ms().saturating_add(delay));
    let retryable = !normalized.contains("unsupported")
        && !normalized.contains("not support")
        && !normalized.contains("unauthorized")
        && !normalized.contains("authentication")
        && !normalized.contains("no open change request")
        && !normalized.contains("different change request")
        && !normalized.contains("detached head")
        && !normalized.contains("configuration")
        && !normalized.contains("canceled")
        && !normalized.contains("cancelled");
    RemoteOperationFailure {
        reason,
        retryable,
        retry_after_unix_ms,
        rate_limit_reset_unix_ms: None,
    }
}

fn retry_after_delay_ms(message: &str) -> Option<i64> {
    let marker = "hint=after_";
    let start = message.find(marker)? + marker.len();
    let value = message[start..].split("ms").next()?;
    value.parse::<i64>().ok().filter(|value| *value >= 0)
}

fn permanent(reason: String) -> RemoteOperationFailure {
    RemoteOperationFailure {
        reason,
        retryable: false,
        retry_after_unix_ms: None,
        rate_limit_reset_unix_ms: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_failure_preserves_provider_retry_after_hint() {
        let before = crate::workflow::prompt_worker::now_unix_ms();
        let failure = classify_failure(
            "GitHub observe failed: rate_limited; retry=retryable; hint=after_2500ms".into(),
        );
        assert!(failure.retryable);
        assert!(
            failure
                .retry_after_unix_ms
                .is_some_and(|wake| wake >= before + 2_500)
        );
    }

    #[test]
    fn provider_cancellation_is_not_retryable() {
        let failure =
            classify_failure("GitHub observe failed: cancelled; retry=not_retryable".into());

        assert!(!failure.retryable);
        assert!(failure.reason.contains("cancelled"));
        assert!(failure.retry_after_unix_ms.is_none());
    }
}
