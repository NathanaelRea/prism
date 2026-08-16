use super::remote_action::{
    RemoteMutationReconciliationMarker, update_persisted_remote_mutation_markers,
};
use super::{RemoteMutationTarget, Tui, TuiJobKey, TuiJobKind, TuiJobPayload};

use crate::remote::{PrCache, PrSummary};
use crate::session::WorktreeRepositoryKey;
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub(crate) enum ReconciliationObservation {
    Cache(crate::workflow::remote_operation::TuiRemoteCachePayload),
    PushResult(crate::workflow::remote_operation::TuiRemotePushPayload),
    LocalBranch(crate::workflow::remote_operation::TuiLocalBranchHeadPayload),
}

#[derive(Clone, Debug)]
pub(crate) struct RemoteReconciliationCommand {
    pub repository: WorktreeRepositoryKey,
    pub marker: RemoteMutationReconciliationMarker,
    pub applied: serde_json::Value,
    pub observation: Option<ReconciliationObservation>,
}

pub(crate) struct RemoteReconciliationResult {
    pub repository: WorktreeRepositoryKey,
    pub marker_version: u64,
    pub marker_job_id: crate::tui_jobs::JobId,
    pub target: RemoteMutationTarget,
    pub result: Result<(), String>,
}

pub(crate) fn classify_summary_evidence(
    repository: &WorktreeRepositoryKey,
    markers: &[RemoteMutationReconciliationMarker],
    summaries: &[PrSummary],
    remote_branch_heads: &BTreeMap<(String, String), String>,
) -> Vec<RemoteReconciliationCommand> {
    markers
        .iter()
        .filter_map(|marker| {
            let ledger = marker.ledger.as_ref()?;
            let (applied, observation) = match &marker.target {
                RemoteMutationTarget::Push { remote, branch, .. }
                    if remote_branch_heads.contains_key(&(remote.clone(), branch.clone())) =>
                {
                    let operation = match &ledger.operation {
                        crate::workflow::remote_operation::RemoteMutationOperation::TuiPushBranch(
                            payload,
                        ) => payload,
                        _ => return None,
                    };
                    (
                        serde_json::Value::Null,
                        Some(ReconciliationObservation::PushResult(operation.clone())),
                    )
                }
                RemoteMutationTarget::Create {
                    source_provider,
                    source_host,
                    source_project,
                    source_branch,
                    expected_head_sha,
                    target_provider: Some(target_provider),
                    target_host,
                    target_project,
                    target_branch,
                    ..
                } => {
                    let operation = match &ledger.operation {
                        crate::workflow::remote_operation::RemoteMutationOperation::TuiCreateChangeRequest(payload) => payload,
                        _ => return None,
                    };
                    let source_repository = crate::remote::RemoteRepositoryId::new(
                        *source_provider,
                        crate::remote::HostIdentity::parse(source_host).ok()?,
                        source_project,
                    )
                    .ok()?;
                    let target_repository = crate::remote::RemoteRepositoryId::new(
                        *target_provider,
                        crate::remote::HostIdentity::parse(target_host).ok()?,
                        target_project,
                    )
                    .ok()?;
                    if operation.source_push.repository != source_repository
                        || operation.target_repository != target_repository
                        || operation.source_push.remote_branch != *source_branch
                        || operation.source_push.expected_head_sha != *expected_head_sha
                    {
                        return None;
                    }
                    summaries.iter().find(|summary| {
                        let Some(identity) = summary.change_request_identity.as_ref() else {
                            return false;
                        };
                        identity
                            .source_repository()
                            .is_ok_and(|repository| repository == source_repository)
                            && identity
                                .target_repository()
                                .is_ok_and(|repository| repository == target_repository)
                            && summary.head_ref == *source_branch
                            && summary.base_ref == *target_branch
                            && summary.head_sha == *expected_head_sha
                    })?;
                    (
                        serde_json::Value::Null,
                        Some(ReconciliationObservation::Cache(
                            crate::workflow::remote_operation::TuiRemoteCachePayload {
                                repository: operation.repository.clone(),
                                worktree: operation.worktree.clone(),
                                branch: operation.branch.clone(),
                                force_details: true,
                            },
                        )),
                    )
                }
                RemoteMutationTarget::Merge {
                    change_request,
                    expected_head_sha,
                } => {
                    let summary = summaries.iter().find(|summary| {
                        summary.change_request_identity.as_ref() == Some(change_request)
                            && summary.head_sha == *expected_head_sha
                            && (summary.merged || summary.merge_is_authoritatively_pending())
                    })?;
                    let outcome = if summary.merged {
                        crate::workflow::remote_operation::TuiRemoteMergeOutcome::Merged
                    } else {
                        crate::workflow::remote_operation::TuiRemoteMergeOutcome::Pending
                    };
                    (
                        serde_json::to_value(
                            crate::workflow::remote_operation::TuiRemoteMergeResult::Accepted {
                                outcome,
                                summary: Box::new(summary.clone()),
                            },
                        )
                        .ok()?,
                        None,
                    )
                }
                RemoteMutationTarget::Fetch {
                    branch,
                    expected_head_sha,
                    ..
                } => {
                    let operation = match &ledger.operation {
                        crate::workflow::remote_operation::RemoteMutationOperation::TuiFetchChangeRequest(payload) => payload,
                        _ => return None,
                    };
                    if operation.summary.head_sha != *expected_head_sha {
                        return None;
                    }
                    (
                        serde_json::json!(true),
                        Some(ReconciliationObservation::LocalBranch(
                            crate::workflow::remote_operation::TuiLocalBranchHeadPayload {
                                repository: operation.repository.clone(),
                                worktree: operation.worktree.clone(),
                                branch: branch.clone(),
                            },
                        )),
                    )
                }
                _ => return None,
            };
            Some(RemoteReconciliationCommand {
                repository: repository.clone(),
                marker: marker.clone(),
                applied,
                observation,
            })
        })
        .collect()
}

pub(crate) fn classify_details_evidence(
    repository: &WorktreeRepositoryKey,
    markers: &[RemoteMutationReconciliationMarker],
    cache: &PrCache,
) -> Vec<RemoteReconciliationCommand> {
    let Ok(Some(summary)) = cache.trusted_summary() else {
        return Vec::new();
    };
    let Ok(Some(details)) = cache.trusted_details() else {
        return Vec::new();
    };
    markers
        .iter()
        .filter_map(|marker| {
            let applied = match &marker.target {
                RemoteMutationTarget::Review {
                    change_request,
                    expected_state,
                    expected_body,
                    prior_review_ids,
                } if summary.change_request_identity.as_ref() == Some(change_request)
                    && details.reviews.iter().any(|review| {
                        review.state.eq_ignore_ascii_case(expected_state)
                            && review.body.trim() == expected_body.trim()
                            && !review.id.trim().is_empty()
                            && !prior_review_ids.contains(&review.id)
                    }) =>
                {
                    serde_json::json!({"submitted": true})
                }
                RemoteMutationTarget::Resolve {
                    change_request,
                    thread_ids,
                } if summary.change_request_identity.as_ref() == Some(change_request)
                    && thread_ids.iter().all(|thread_id| {
                        details
                            .review_comments
                            .iter()
                            .any(|comment| comment.thread_id == *thread_id && comment.resolved)
                    }) =>
                {
                    serde_json::json!(thread_ids.len())
                }
                _ => return None,
            };
            Some(RemoteReconciliationCommand {
                repository: repository.clone(),
                marker: marker.clone(),
                applied,
                observation: None,
            })
        })
        .collect()
}

impl Tui {
    pub(crate) fn enqueue_summary_reconciliation(
        &mut self,
        repository: &WorktreeRepositoryKey,
        summaries: &[PrSummary],
        remote_branch_heads: &BTreeMap<(String, String), String>,
    ) {
        let markers = self
            .background
            .markers(&repository.root)
            .into_iter()
            .flatten()
            .filter(|marker| {
                self.background.marker_is_persisted(&(
                    repository.root.clone(),
                    marker.recorded_unix_ms,
                    marker.job_id,
                ))
            })
            .cloned()
            .collect::<Vec<_>>();
        let commands =
            classify_summary_evidence(repository, &markers, summaries, remote_branch_heads);
        self.enqueue_reconciliation_commands(commands);
    }

    pub(crate) fn enqueue_details_reconciliation(
        &mut self,
        repository: &WorktreeRepositoryKey,
        cache: &PrCache,
    ) {
        let markers = self
            .background
            .markers(&repository.root)
            .into_iter()
            .flatten()
            .filter(|marker| {
                self.background.marker_is_persisted(&(
                    repository.root.clone(),
                    marker.recorded_unix_ms,
                    marker.job_id,
                ))
            })
            .cloned()
            .collect::<Vec<_>>();
        let commands = classify_details_evidence(repository, &markers, cache);
        self.enqueue_reconciliation_commands(commands);
    }

    fn enqueue_reconciliation_commands(&mut self, commands: Vec<RemoteReconciliationCommand>) {
        for command in commands {
            let key = (
                command.repository.root.clone(),
                command.marker.recorded_unix_ms,
                command.marker.job_id,
            );
            if !self.background.begin_reconciliation(key.clone()) {
                continue;
            }
            let repository = command.repository.clone();
            let marker_version = command.marker.recorded_unix_ms;
            let marker_job_id = command.marker.job_id;
            let job_id = self.spawn_tui_job(
                TuiJobKind::RemoteReconciliation,
                TuiJobKey::Repository(repository.clone()),
                marker_version,
                None,
                "prism-remote-reconciliation".to_string(),
                move |_| {
                    let target = command.marker.target.clone();
                    let result = (|| {
                        let mut applied = command.applied;
                        match command.observation {
                            Some(ReconciliationObservation::Cache(payload)) => {
                                applied = serde_json::to_value(crate::worker::observe_remote::<
                                    crate::remote::WorkerPrCacheSnapshot,
                                >(
                                    &payload.repository,
                                    &payload.worktree,
                                    crate::workflow::remote_operation::RemoteObservationOperation::TuiChangeRequestCache(payload.clone()),
                                    &format!("{}:{}:reconcile", payload.worktree.display(), payload.branch),
                                )?)
                                .map_err(|error| format!("encode authoritative cache snapshot: {error}"))?;
                            }
                            Some(ReconciliationObservation::PushResult(payload)) => {
                                applied = serde_json::to_value(crate::worker::observe_remote::<
                                    crate::workflow::remote_operation::TuiRemotePushResult,
                                >(
                                    &payload.repository,
                                    &payload.worktree,
                                    crate::workflow::remote_operation::RemoteObservationOperation::TuiPushBranchResult(Box::new(payload.clone())),
                                    &format!("{}:{}:push-reconcile", payload.worktree.display(), payload.branch),
                                )?)
                                .map_err(|error| format!("encode authoritative push result: {error}"))?;
                            }
                            Some(ReconciliationObservation::LocalBranch(payload)) => {
                                let expected = match &target {
                                    RemoteMutationTarget::Fetch { expected_head_sha, .. } => expected_head_sha,
                                    _ => return Err("local branch evidence requested for non-fetch mutation".to_string()),
                                };
                                let head = crate::worker::observe_remote::<Option<String>>(
                                    &payload.repository,
                                    &payload.worktree,
                                    crate::workflow::remote_operation::RemoteObservationOperation::TuiLocalBranchHead(payload.clone()),
                                    &format!("{}:{}:local-ref", payload.worktree.display(), payload.branch),
                                )?;
                                if head.as_deref() != Some(expected.as_str()) {
                                    return Err("fetched local branch has not reached the expected authoritative head".to_string());
                                }
                            }
                            None => {}
                        }
                        let ledger = command.marker.ledger.as_ref().ok_or_else(|| "remote reconciliation marker has no durable ledger identity".to_string())?;
                        crate::worker::reconcile_remote_mutation(
                            &ledger.repository,
                            &ledger.worktree,
                            &ledger.request_id,
                            ledger.operation.clone(),
                            &ledger.subject,
                            crate::RemoteMutationReconciliation::Applied(applied),
                        )?;
                        update_persisted_remote_mutation_markers(
                            &command.marker.database_path,
                            |markers| {
                                markers.retain(|existing| {
                                    !(existing.target == target
                                        && existing.recorded_unix_ms == marker_version
                                        && existing.job_id == marker_job_id)
                                });
                            },
                        )
                    })();
                    Ok(Some(TuiJobPayload::RemoteReconciliation(
                        RemoteReconciliationResult {
                            repository,
                            marker_version,
                            marker_job_id,
                            target,
                            result,
                        },
                    )))
                },
            );
            self.background.track_reconciliation_job(job_id, key);
        }
    }

    pub(super) fn apply_remote_reconciliation_result(
        &mut self,
        result: RemoteReconciliationResult,
    ) {
        if let Err(error) = &result.result {
            crate::flight_recorder::record(
                "remote",
                "reconciliation_failed",
                None,
                vec![
                    crate::flight_recorder::text(
                        "repository",
                        result.repository.root.display().to_string(),
                    ),
                    crate::flight_recorder::text("error", crate::util::single_line(error)),
                ],
            );
        }
        self.background.finish_reconciliation(
            &result.repository.root,
            result.marker_version,
            result.marker_job_id,
            &result.target,
            result.result.is_ok(),
        );
    }
}
