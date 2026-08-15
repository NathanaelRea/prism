//! Production bridge from built-in Triggers to the shared provider request coordinator.

use serde::Serialize;
use sha2::{Digest as _, Sha256};
use std::future::Future;
use std::pin::Pin;

use crate::remote::request_coordinator::{
    CoordinatedRemoteOperation, ObservationFreshness, RemoteMutationFailureDisposition,
    RemoteMutationRequest, RemoteMutationResult, RemoteObservationKey, RemoteObservationResult,
    RemoteOperationExecutor, RemoteOperationFailure, RemoteOperationOutput, RemotePriority,
    RemoteRequestCoordinator,
};

pub(crate) use super::remote_operation::{
    ResolveThreadsPayload, TuiRemoteCreatePreparation, TuiRemoteMergeOutcome, TuiRemoteMergeResult,
    TuiRemotePushResult,
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
            let lane = lane_for_subject(&context.subject)?;
            let change_request = context.subject.change_request.clone().ok_or_else(|| {
                TriggerError::Protocol("standard Trigger requires a Change Request".into())
            })?;
            let key =
                RemoteObservationKey::new(lane, "change_request.stabilization", change_request)
                    .map_err(|error| TriggerError::Protocol(error.to_string()))?;
            let subject = observation_subject(context);
            let freshness = observation_freshness(&subject, context.cycle_started_unix_ms);
            let payload = serde_json::to_value(subject)
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
            let lane = lane_for_subject(&context.subject)?;
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
                    RemoteMutationResult::Failed { reason, .. } => {
                        StandardMutationResult::Fail(reason)
                    }
                },
            )
        })
    }
}

fn observation_subject(context: &TriggerContext) -> TriggerSubject {
    let mut subject = context.subject.clone();
    if context.cycle > 1 {
        subject.change_request_head = None;
    }
    subject
}

fn observation_freshness(
    subject: &TriggerSubject,
    cycle_started_unix_ms: i64,
) -> ObservationFreshness {
    subject
        .change_request_head
        .as_ref()
        .map_or_else(
            || ObservationFreshness::any(OBSERVATION_MAX_AGE_MS),
            |head| ObservationFreshness::exact(head, OBSERVATION_MAX_AGE_MS),
        )
        .not_before(cycle_started_unix_ms)
}

fn lane_for_subject(
    subject: &TriggerSubject,
) -> Result<crate::remote::request_coordinator::RemoteLaneKey, TriggerError> {
    lane_for_remote_paths(&subject.repository, &subject.worktree).map_err(TriggerError::Protocol)
}

pub(crate) fn lane_for_remote_paths(
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
        Box::pin(async move {
            tokio::task::spawn_blocking(move || execute_blocking(operation))
                .await
                .map_err(|error| RemoteOperationFailure {
                    reason: format!("provider operation task failed: {error}"),
                    retryable: true,
                    mutation_disposition: RemoteMutationFailureDisposition::OutcomeUncertain,
                    retry_after_unix_ms: None,
                    rate_limit_reset_unix_ms: None,
                })?
        })
    }
}

fn execute_blocking(
    operation: CoordinatedRemoteOperation,
) -> Result<RemoteOperationOutput, RemoteOperationFailure> {
    let operation = match operation {
        CoordinatedRemoteOperation::Observe(request) => {
            super::remote_operation::decode_observation(&request.key.operation, request.payload)
                .map(TypedCoordinatedRemoteOperation::Observe)
                .map_err(|error| permanent(format!("invalid coordinated observation: {error}")))?
        }
        CoordinatedRemoteOperation::Mutate(request) => {
            super::remote_operation::decode_mutation(&request.operation, request.payload)
                .map(Box::new)
                .map(TypedCoordinatedRemoteOperation::Mutate)
                .map_err(|error| permanent(format!("invalid coordinated mutation: {error}")))?
        }
    };
    execute_typed(operation)
}

enum TypedCoordinatedRemoteOperation {
    Observe(super::remote_operation::RemoteObservationOperation),
    Mutate(Box<super::remote_operation::RemoteMutationOperation>),
}

fn execute_typed(
    operation: TypedCoordinatedRemoteOperation,
) -> Result<RemoteOperationOutput, RemoteOperationFailure> {
    use super::remote_operation::{
        RemoteMutationOperation as Mutation, RemoteObservationOperation as Observation,
    };
    match operation {
        TypedCoordinatedRemoteOperation::Observe(Observation::ChangeRequestStabilization(
            subject,
        )) => {
            let observation = observe_change_request(&subject).map_err(classify_failure)?;
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
        TypedCoordinatedRemoteOperation::Observe(Observation::TuiChangeRequests(payload)) => {
            let repository = crate::repo::Repository {
                root: payload.repository.clone(),
            };
            let config = crate::config::Config::load(&repository);
            let summaries =
                crate::remote::dispatcher::list_change_requests(&payload.worktree, &config)
                    .map_err(classify_failure)?;
            output(summaries, "list")
        }
        TypedCoordinatedRemoteOperation::Observe(Observation::TuiRepositoryPolicy(payload)) => {
            let repository = crate::repo::Repository {
                root: payload.repository.clone(),
            };
            let config = crate::config::Config::load(&repository);
            crate::remote::dispatcher::refresh_repository_policy(
                &repository,
                &payload.worktree,
                &config,
            )
            .map_err(classify_failure)?;
            output(true, "policy")
        }
        TypedCoordinatedRemoteOperation::Observe(Observation::TuiRemoteBranchHead(payload)) => {
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
            .map_err(classify_failure)?;
            output(head, "remote-branch-head")
        }
        TypedCoordinatedRemoteOperation::Observe(Observation::TuiPushBranchResult(payload)) => {
            let result = tui_push_reconciliation_result(&payload).map_err(classify_failure)?;
            output(result, "push-result")
        }
        TypedCoordinatedRemoteOperation::Observe(Observation::TuiLocalBranchHead(payload)) => {
            let repository = crate::repo::Repository {
                root: payload.repository,
            };
            let config = crate::config::Config::load(&repository);
            let output_value = crate::process::run_output_named(
                std::process::Command::new(config.tool("git"))
                    .arg("-C")
                    .arg(&payload.worktree)
                    .args(["rev-parse", "--verify"])
                    .arg(format!("refs/heads/{}^{{commit}}", payload.branch)),
                crate::process::ProcessPolicy::Metadata,
                crate::process::ProcessDescriptor::new("git.reconciliation_local_branch_head"),
            )
            .map_err(classify_failure)?;
            let head = output_value
                .status
                .success()
                .then(|| output_value.stdout.trim().to_string());
            output(head, "local-branch-head")
        }
        TypedCoordinatedRemoteOperation::Observe(Observation::TuiChangeRequestCache(payload)) => {
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
        TypedCoordinatedRemoteOperation::Mutate(operation) => match *operation {
            Mutation::ChangeRequestResolveReviewThreads(payload) => {
                resolve_threads(payload).map_err(classify_failure)?;
                output(serde_json::json!({"resolved": true}), "mutation")
            }
            Mutation::TuiResolveReviewThreads(payload) => {
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
                    .map_err(classify_failure)?;
                }
                output(payload.thread_ids.len(), "mutation")
            }
            Mutation::TuiPushBranch(payload) => {
                let repository = crate::repo::Repository {
                    root: payload.repository.clone(),
                };
                let config = crate::config::Config::load(&repository);
                let current = crate::remote::dispatcher::prepare_push(
                    &payload.worktree,
                    &config,
                    &payload.branch,
                )
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
                .map_err(classify_failure)?;
                let result = tui_push_result(&payload).map_err(uncertain)?;
                output(result, "mutation")
            }
            Mutation::TuiCreateChangeRequest(payload) => {
                let repository = crate::repo::Repository {
                    root: payload.repository,
                };
                let config = crate::config::Config::load(&repository);
                let mut cache = crate::remote::load_pr_cache(&repository, &payload.branch);
                crate::remote::dispatcher::refresh_change_request_cache(
                    &repository,
                    &payload.branch,
                    &mut cache,
                    &payload.worktree,
                    &config,
                    false,
                )
                .map_err(classify_failure)?;
                if cache.has_summary() {
                    return output(
                        crate::remote::WorkerPrCacheSnapshot::capture(&cache),
                        "mutation",
                    );
                }
                let guard = crate::remote::dispatcher::prepare_create_change_request(
                    &payload.worktree,
                    &config,
                    &payload.branch,
                    &payload.target_repository,
                    &payload.source_push,
                )
                .map_err(classify_failure)?;
                crate::remote::dispatcher::create_change_request(
                    &repository,
                    &config,
                    &payload.worktree,
                    &payload.body,
                    &guard,
                    &mut cache,
                )
                .map_err(classify_failure)?;
                output(
                    crate::remote::WorkerPrCacheSnapshot::capture(&cache),
                    "mutation",
                )
            }
            Mutation::TuiFetchChangeRequest(payload) => {
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
                .map_err(classify_failure)?;
                output(true, "mutation")
            }
            Mutation::TuiSubmitReview(payload) => {
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
                .map_err(classify_failure)?;
                output(serde_json::json!({"submitted": true}), "mutation")
            }
            Mutation::TuiMergeChangeRequest(payload) => {
                let repository = crate::repo::Repository {
                    root: payload.repository,
                };
                let config = crate::config::Config::load(&repository);
                let prepared = match crate::remote::dispatcher::prepare_merge_change_request(
                    &config,
                    &payload.worktree,
                    &payload.change_request,
                    payload.display_number,
                    &payload.expected_head_sha,
                ) {
                    Ok(prepared) => prepared,
                    Err(reason) => {
                        return output(TuiRemoteMergeResult::Rejected { reason }, "mutation");
                    }
                };
                let result = match crate::remote::dispatcher::execute_guarded_merge_reconciled(
                    &config,
                    &payload.worktree,
                    &prepared,
                ) {
                    crate::remote::dispatcher::GuardedMergeExecution::Applied(result) => *result,
                    crate::remote::dispatcher::GuardedMergeExecution::Rejected(reason) => {
                        return output(TuiRemoteMergeResult::Rejected { reason }, "mutation");
                    }
                    crate::remote::dispatcher::GuardedMergeExecution::Uncertain(reason) => {
                        return Err(uncertain(reason));
                    }
                };
                let outcome = match result.outcome {
                    crate::remote::MergeMutationOutcome::Merged => TuiRemoteMergeOutcome::Merged,
                    crate::remote::MergeMutationOutcome::Pending => TuiRemoteMergeOutcome::Pending,
                    crate::remote::MergeMutationOutcome::Uncertain => {
                        TuiRemoteMergeOutcome::Uncertain
                    }
                };
                let summary = crate::remote::dispatcher::legacy_summary(result.summary)
                    .map_err(classify_failure)?;
                output(
                    TuiRemoteMergeResult::Accepted {
                        outcome,
                        summary: Box::new(summary),
                    },
                    "mutation",
                )
            }
        },
    }
}

fn tui_push_result(
    payload: &super::remote_operation::TuiRemotePushPayload,
) -> Result<TuiRemotePushResult, String> {
    let repository = crate::repo::Repository {
        root: payload.repository.clone(),
    };
    let config = crate::config::Config::load(&repository);
    let pushed =
        crate::remote::dispatcher::prepare_push(&payload.worktree, &config, &payload.branch)?;
    if !crate::remote::dispatcher::same_push_target(&payload.expected, &pushed) {
        return Err("push destination changed while pushing".into());
    }
    let mut cache = crate::remote::load_pr_cache(&repository, &payload.branch);
    crate::remote::dispatcher::refresh_change_request_cache(
        &repository,
        &payload.branch,
        &mut cache,
        &payload.worktree,
        &config,
        false,
    )?;
    let create = if cache.has_summary() {
        None
    } else {
        let (origin_repository, upstream_repository) =
            crate::remote::dispatcher::create_change_request_targets(&payload.worktree, &config)?;
        Some(TuiRemoteCreatePreparation {
            source_push: pushed,
            origin_repository,
            upstream_repository,
        })
    };
    Ok(TuiRemotePushResult {
        cache: crate::remote::WorkerPrCacheSnapshot::capture(&cache),
        create,
    })
}

fn tui_push_reconciliation_result(
    payload: &super::remote_operation::TuiRemotePushPayload,
) -> Result<TuiRemotePushResult, String> {
    let repository = crate::repo::Repository {
        root: payload.repository.clone(),
    };
    let config = crate::config::Config::load(&repository);
    let current =
        crate::remote::dispatcher::prepare_push(&payload.worktree, &config, &payload.branch)?;
    let expected = &payload.expected;
    if current.repository != expected.repository
        || current.remote != expected.remote
        || current.remote_branch != expected.remote_branch
        || current.local_branch != expected.local_branch
    {
        return Err("push destination changed before reconciliation".into());
    }
    let remote_head = crate::git::push_remote_branch_head_sha(
        &payload.worktree,
        &expected.remote,
        &expected.remote_branch,
        &config,
    )?;
    if remote_head.as_deref() != Some(expected.expected_head_sha.as_str()) {
        return Err("pushed branch no longer has the expected authoritative head".into());
    }
    let mut cache = crate::remote::load_pr_cache(&repository, &payload.branch);
    crate::remote::dispatcher::refresh_change_request_cache(
        &repository,
        &payload.branch,
        &mut cache,
        &payload.worktree,
        &config,
        false,
    )?;
    let create = if cache.has_summary() || current.expected_head_sha != expected.expected_head_sha {
        None
    } else {
        let (origin_repository, upstream_repository) =
            crate::remote::dispatcher::create_change_request_targets(&payload.worktree, &config)?;
        Some(TuiRemoteCreatePreparation {
            source_push: current,
            origin_repository,
            upstream_repository,
        })
    };
    Ok(TuiRemotePushResult {
        cache: crate::remote::WorkerPrCacheSnapshot::capture(&cache),
        create,
    })
}

fn observe_change_request(subject: &TriggerSubject) -> Result<ChangeRequestObservation, String> {
    let repository = crate::repo::Repository {
        root: subject.repository.clone(),
    };
    let config = crate::config::Config::load(&repository);
    if !config.config_errors.is_empty() {
        return Err(config.config_errors.join("; "));
    }
    let branch = crate::git::current_branch_name(&subject.worktree, &config)?
        .ok_or_else(|| "standard Workflow Triggers do not support detached HEAD".to_string())?;
    let mut cache = crate::remote::load_pr_cache(&repository, &branch);
    crate::remote::dispatcher::refresh_change_request_cache(
        &repository,
        &branch,
        &mut cache,
        &subject.worktree,
        &config,
        true,
    )?;
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
    )?;
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
    if subject
        .change_request_head
        .as_deref()
        .is_some_and(|expected| expected != summary.head_sha)
    {
        return Err("Change Request head changed before workflow observation".into());
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
    )?;
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
    let policy_blockers = Vec::new();
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
        merge_queue_required: policy.merge_queue_required,
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
    let _ = (provider, details);
    // An authoritative empty observation means there are no checks, not that the provider lacks
    // the capability. Unsupported or failed observations must carry explicit evidence.
    None
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

fn resolve_threads(payload: ResolveThreadsPayload) -> Result<(), String> {
    let repository = crate::repo::Repository {
        root: payload.subject.repository.clone(),
    };
    let config = crate::config::Config::load(&repository);
    let branch = crate::git::current_branch_name(&payload.subject.worktree, &config)?
        .ok_or_else(|| "cannot resolve review threads from detached HEAD".to_string())?;
    let mut cache = crate::remote::load_pr_cache(&repository, &branch);
    crate::remote::dispatcher::refresh_change_request_cache(
        &repository,
        &branch,
        &mut cache,
        &payload.subject.worktree,
        &config,
        true,
    )?;
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
            )?;
        }
    }
    Ok(())
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
    // Dispatcher compatibility APIs still return display strings. Fail closed unless the retained
    // classified metadata or a narrow transport allowlist explicitly proves the failure retryable.
    let explicitly_retryable = normalized.contains("retry=retryable")
        || normalized.contains("rate_limited")
        || normalized.contains("timed out")
        || normalized.contains("timeout")
        || normalized.contains("transport");
    let explicitly_permanent = normalized.contains("retry=not_retryable")
        || normalized.contains("retry=unknown")
        || normalized.contains("unsupported")
        || normalized.contains("not support")
        || normalized.contains("unauthorized")
        || normalized.contains("authorization")
        || normalized.contains("authentication")
        || normalized.contains("validation")
        || normalized.contains("conflict")
        || normalized.contains("not_found")
        || normalized.contains("no open change request")
        || normalized.contains("different change request")
        || normalized.contains("change request head changed")
        || normalized.contains("detached head")
        || normalized.contains("configuration");
    RemoteOperationFailure {
        reason,
        retryable: explicitly_retryable && !explicitly_permanent,
        // Compatibility operations may perform more than one provider effect before returning an
        // error. Without explicit pre-effect evidence, fail closed as uncertain for mutations.
        mutation_disposition: RemoteMutationFailureDisposition::OutcomeUncertain,
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
        mutation_disposition: RemoteMutationFailureDisposition::RejectedBeforeEffect,
        retry_after_unix_ms: None,
        rate_limit_reset_unix_ms: None,
    }
}

fn uncertain(reason: String) -> RemoteOperationFailure {
    RemoteOperationFailure {
        reason,
        retryable: false,
        mutation_disposition: RemoteMutationFailureDisposition::OutcomeUncertain,
        retry_after_unix_ms: None,
        rate_limit_reset_unix_ms: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::remote_operation::TuiRemoteMergePayload;

    fn context(cycle: u64) -> TriggerContext {
        TriggerContext {
            run_id: "run-1".into(),
            step_key: "review".into(),
            attempt_id: "attempt-1".into(),
            cycle,
            cycle_started_unix_ms: 1,
            subject: TriggerSubject {
                repository: "/repo".into(),
                worktree: "/repo/wt".into(),
                change_request: Some("github:github.com:example/repo:change_request:PR_42".into()),
                change_request_head: Some("launch-head".into()),
            },
            cancellation_requested: false,
        }
    }

    #[test]
    fn merge_payload_round_trip_preserves_canonical_identity_and_exact_head() {
        let identity = crate::remote::test_change_request_identity();
        let payload = TuiRemoteMergePayload {
            repository: "/repo".into(),
            worktree: "/repo/wt".into(),
            change_request: identity.clone(),
            display_number: 42,
            expected_head_sha: "abc123".into(),
        };
        let decoded: TuiRemoteMergePayload =
            serde_json::from_value(serde_json::to_value(payload).unwrap()).unwrap();
        assert_eq!(decoded.change_request, identity);
        assert_eq!(decoded.display_number, 42);
        assert_eq!(decoded.expected_head_sha, "abc123");
    }

    #[test]
    fn tui_push_result_round_trip_preserves_create_preparation() {
        let repository = crate::remote::RemoteRepositoryId::new(
            crate::remote::ProviderKind::GitHub,
            crate::remote::HostIdentity::new("github.com", None).unwrap(),
            "example/repo",
        )
        .unwrap();
        let source_push = crate::remote::dispatcher::PushGuard {
            repository: repository.clone(),
            remote: "origin".into(),
            remote_branch: "feature".into(),
            local_branch: "feature".into(),
            expected_head_sha: "abc123".into(),
            set_upstream: true,
        };
        let result = TuiRemotePushResult {
            cache: crate::remote::WorkerPrCacheSnapshot::capture(&crate::remote::PrCache::default()),
            create: Some(TuiRemoteCreatePreparation {
                source_push,
                origin_repository: repository,
                upstream_repository: None,
            }),
        };

        let encoded = serde_json::to_value(&result).unwrap();
        let decoded: TuiRemotePushResult = serde_json::from_value(encoded).unwrap();
        let create = decoded.create.unwrap();
        assert_eq!(create.source_push.remote_branch, "feature");
        assert_eq!(create.origin_repository.project_path(), "example/repo");
    }

    #[test]
    fn post_effect_failures_are_classified_uncertain() {
        let failure = uncertain("push destination changed while pushing".into());
        assert_eq!(
            failure.mutation_disposition,
            RemoteMutationFailureDisposition::OutcomeUncertain
        );
    }

    #[test]
    fn launch_head_is_checked_only_during_the_initial_observation_cycle() {
        assert_eq!(
            observation_subject(&context(1))
                .change_request_head
                .as_deref(),
            Some("launch-head")
        );
        assert_eq!(observation_subject(&context(2)).change_request_head, None);
    }

    #[test]
    fn initial_observation_requires_the_launch_head_revision() {
        let context = context(1);
        let subject = observation_subject(&context);
        let freshness = observation_freshness(&subject, context.cycle_started_unix_ms);
        assert_eq!(freshness.subject_revision.as_deref(), Some("launch-head"));
        assert_eq!(freshness.not_before_unix_ms, Some(1));
    }

    #[test]
    fn launch_head_mismatch_is_permanent() {
        assert!(
            !classify_failure("Change Request head changed before workflow observation".into())
                .retryable
        );
    }

    #[test]
    fn unclassified_failures_are_not_retried_without_positive_evidence() {
        for reason in [
            "provider authorization failed",
            "validation failed",
            "conflict",
            "provider failed: retry=unknown",
            "opaque provider failure",
        ] {
            assert!(!classify_failure(reason.into()).retryable, "{reason}");
        }
        assert!(classify_failure("provider transport timeout".into()).retryable);
    }

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
}
