use std::collections::BTreeMap;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};

use crate::auto_flow::PersistedAutoRun;
use crate::remote::{PrCache, PrSummary};
use crate::session::{Session, WorktreeRepositoryKey};
use crate::tui_jobs::JobId;
use crate::tui_runtime::{RuntimeEvent, TerminalRuntime};
use crate::view;

use super::{
    REMOTE_MUTATION_RECONCILIATION_KEY, TUI_ACTION_JOB_TIMEOUT, Tui, TuiJobKey, TuiJobKind,
    TuiJobPayload, ctrl_key,
};

fn merge_is_authoritatively_pending(queue_state: &str) -> bool {
    matches!(
        queue_state.trim().to_ascii_lowercase().as_str(),
        "queued" | "running" | "blocked"
    )
}

pub(crate) struct RemoteActionDelivery {
    pub id: JobId,
    pub result: Result<RemoteActionValue, String>,
}

pub(crate) struct RemoteActionRequest<'a> {
    pub key: TuiJobKey,
    pub generation: u64,
    pub name: &'static str,
    pub title: &'a str,
    pub message: &'a str,
    pub abandon_cancelable: bool,
    pub mutation: Option<RemoteMutationTarget>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub(crate) enum RemoteMutationTarget {
    Unknown {
        marker_id: String,
    },
    Push {
        remote: String,
        branch: String,
        expected_head_sha: String,
        #[serde(default)]
        repository_provider: Option<crate::remote::ProviderKind>,
        #[serde(default)]
        repository_host: String,
        #[serde(default)]
        repository_project: String,
    },
    Create {
        source_provider: crate::remote::ProviderKind,
        source_host: String,
        source_project: String,
        source_branch: String,
        expected_head_sha: String,
        #[serde(default)]
        target_provider: Option<crate::remote::ProviderKind>,
        #[serde(default)]
        target_host: String,
        #[serde(default)]
        target_project: String,
        #[serde(default)]
        target_branch: String,
        #[serde(default)]
        expected_base_sha: String,
    },
    Review {
        change_request: crate::remote::CanonicalChangeRequestIdentity,
        expected_state: String,
        expected_body: String,
        prior_review_ids: Vec<String>,
    },
    Resolve {
        change_request: crate::remote::CanonicalChangeRequestIdentity,
        thread_ids: Vec<String>,
    },
    Merge {
        change_request: crate::remote::CanonicalChangeRequestIdentity,
        expected_head_sha: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub(super) struct RemoteMutationReconciliationMarker {
    pub(super) target: RemoteMutationTarget,
    job_id: JobId,
    reason: String,
    recorded_unix_ms: u64,
}

#[derive(Clone)]
pub(super) struct RemoteActionReconciliationContext {
    pub(super) key: TuiJobKey,
    pub(super) target: RemoteMutationTarget,
}

pub(super) fn remote_action_abandon_requested(abandon_cancelable: bool, event: KeyEvent) -> bool {
    abandon_cancelable
        && event.kind == KeyEventKind::Press
        && (event.code == KeyCode::Esc || (event.code == KeyCode::Char('c') && ctrl_key(event)))
}

pub(super) fn remote_action_timeout(abandon_cancelable: bool) -> Option<Duration> {
    abandon_cancelable.then_some(TUI_ACTION_JOB_TIMEOUT)
}

pub(crate) enum RemoteActionValue {
    WorktrunkUserConfig(crate::worktrunk::UserConfigLocation),
    ChangeRequests(Vec<PrSummary>),
    Cache(Box<PrCache>),
    Resolved {
        cache: Box<PrCache>,
        count: usize,
    },
    PushPrepared(Box<RemotePushPrepared>),
    CreatePrepared(Box<crate::remote::dispatcher::CreateChangeRequestGuard>),
    GuardedPush {
        persisted: Box<PersistedAutoRun>,
        cache: Box<PrCache>,
        progress: Option<crate::auto_flow::stabilization_execute::GuardedPushProgress>,
    },
    ReviewResolutionPrepared {
        persisted: Box<PersistedAutoRun>,
        cache: Box<PrCache>,
        thread_ids: Vec<String>,
        summary: Box<PrSummary>,
    },
    ReviewResolutionFinished {
        persisted: Box<PersistedAutoRun>,
        cache: Box<PrCache>,
        resolved: usize,
    },
    MergeAuthorization {
        session: Box<Session>,
        authorization: Box<crate::auto_flow::stabilization_execute::MergeAuthorization>,
    },
    MergeExecution {
        session: Box<Session>,
        result: Result<RemoteMergeOutcome, String>,
    },
    NotApplicable,
    Complete,
}

pub(crate) struct RemotePushPrepared {
    pub cache: PrCache,
    pub origin_repository: Option<crate::remote::RemoteRepositoryId>,
    pub upstream_repository: Option<crate::remote::RemoteRepositoryId>,
    pub push_guard: Option<crate::remote::dispatcher::PushGuard>,
}

pub(crate) struct RemoteMergeOutcome {
    pub execution: crate::auto_flow::stabilization_execute::ManualMergeExecution,
    pub verification: Option<Result<bool, String>>,
}

pub(super) fn remote_mutation_targets_overlap(
    existing: &RemoteMutationTarget,
    requested: &RemoteMutationTarget,
) -> bool {
    match (existing, requested) {
        (
            RemoteMutationTarget::Push {
                remote: existing_remote,
                branch: existing_branch,
                expected_head_sha: existing_head,
                repository_provider: existing_provider,
                repository_host: existing_host,
                repository_project: existing_project,
            },
            RemoteMutationTarget::Push {
                remote: requested_remote,
                branch: requested_branch,
                expected_head_sha: requested_head,
                repository_provider: requested_provider,
                repository_host: requested_host,
                repository_project: requested_project,
            },
        ) => {
            existing_remote == requested_remote
                && existing_branch == requested_branch
                && existing_head == requested_head
                && optional_repository_fields_match(
                    *existing_provider,
                    existing_host,
                    existing_project,
                    *requested_provider,
                    requested_host,
                    requested_project,
                )
        }
        (
            RemoteMutationTarget::Create {
                source_provider: existing_source_provider,
                source_host: existing_source_host,
                source_project: existing_source_project,
                source_branch: existing_source_branch,
                expected_head_sha: existing_head,
                target_provider: existing_target_provider,
                target_host: existing_target_host,
                target_project: existing_target_project,
                target_branch: existing_target_branch,
                expected_base_sha: existing_base,
            },
            RemoteMutationTarget::Create {
                source_provider: requested_source_provider,
                source_host: requested_source_host,
                source_project: requested_source_project,
                source_branch: requested_source_branch,
                expected_head_sha: requested_head,
                target_provider: requested_target_provider,
                target_host: requested_target_host,
                target_project: requested_target_project,
                target_branch: requested_target_branch,
                expected_base_sha: requested_base,
            },
        ) => {
            existing_source_provider == requested_source_provider
                && existing_source_host == requested_source_host
                && existing_source_project == requested_source_project
                && existing_source_branch == requested_source_branch
                && existing_head == requested_head
                && optional_repository_fields_match(
                    *existing_target_provider,
                    existing_target_host,
                    existing_target_project,
                    *requested_target_provider,
                    requested_target_host,
                    requested_target_project,
                )
                && (existing_target_branch.is_empty()
                    || requested_target_branch.is_empty()
                    || existing_target_branch == requested_target_branch)
                && (existing_base.is_empty()
                    || requested_base.is_empty()
                    || existing_base == requested_base)
        }
        _ => existing == requested,
    }
}

pub(super) fn optional_repository_fields_match(
    left_provider: Option<crate::remote::ProviderKind>,
    left_host: &str,
    left_project: &str,
    right_provider: Option<crate::remote::ProviderKind>,
    right_host: &str,
    right_project: &str,
) -> bool {
    left_provider.is_none()
        || right_provider.is_none()
        || (left_provider == right_provider
            && left_host == right_host
            && left_project == right_project)
}

pub(super) fn uncertain_remote_mutation_error(
    result: &Result<RemoteActionValue, String>,
) -> Option<&str> {
    match result {
        Err(error) => Some(error),
        Ok(RemoteActionValue::MergeExecution {
            result: Err(error), ..
        }) => Some(error),
        Ok(RemoteActionValue::MergeExecution {
            result:
                Ok(RemoteMergeOutcome {
                    verification: Some(Err(error)),
                    ..
                }),
            ..
        }) => Some(error),
        Ok(RemoteActionValue::MergeExecution {
            result:
                Ok(RemoteMergeOutcome {
                    execution:
                        crate::auto_flow::stabilization_execute::ManualMergeExecution::Uncertain {
                            ..
                        },
                    ..
                }),
            ..
        }) => Some("provider did not confirm that the merge was accepted"),
        Ok(_) => None,
    }
}

impl Tui {
    pub(super) fn load_remote_mutation_reconciliation_markers(&mut self) {
        let marked = self
            .repos
            .iter()
            .filter_map(|managed| {
                let markers = (|| {
                    let value = crate::persistence::database::load_metadata(
                        &crate::observability::db_path(&managed.repo),
                        REMOTE_MUTATION_RECONCILIATION_KEY,
                    )
                    .map_err(|error| {
                        format!("read remote mutation reconciliation marker: {error}")
                    })?;
                    value
                        .map(|value| {
                            serde_json::from_str::<Vec<RemoteMutationReconciliationMarker>>(&value)
                                .map_err(|error| {
                                    format!("decode remote mutation reconciliation marker: {error}")
                                })
                        })
                        .transpose()
                        .map(|markers| markers.unwrap_or_default())
                })()
                .unwrap_or_else(|error| {
                    vec![RemoteMutationReconciliationMarker {
                        target: RemoteMutationTarget::Unknown {
                            marker_id: "unreadable-persisted-marker".to_string(),
                        },
                        job_id: 0,
                        reason: error,
                        recorded_unix_ms: crate::auto_flow::unix_ms(),
                    }]
                });
                (!markers.is_empty()).then(|| (managed.repo.root.clone(), markers))
            })
            .collect::<BTreeMap<_, _>>();
        if marked.is_empty() {
            return;
        }
        for session in &mut self.sessions {
            if self
                .repos
                .get(session.repo_index)
                .is_some_and(|managed| marked.contains_key(&managed.repo.root))
            {
                session.pr.require_reconciliation(
                    "remote mutation completion is unknown; authoritative re-observation required",
                );
            }
        }
        self.remote_mutations_requiring_reconciliation = marked;
    }

    pub(super) fn record_remote_mutation_reconciliation(
        &mut self,
        key: &TuiJobKey,
        job_id: JobId,
        reason: &str,
        target: &RemoteMutationTarget,
    ) -> Result<(), String> {
        let root = self
            .repository_root_for_job_key(key)
            .ok_or_else(|| "remote mutation has no repository reconciliation target".to_string())?;
        let repo = self
            .repos
            .iter()
            .find(|managed| managed.repo.root == root)
            .map(|managed| managed.repo.clone())
            .ok_or_else(|| {
                format!(
                    "remote mutation repository is no longer managed: {}",
                    root.display()
                )
            })?;
        let markers = self
            .remote_mutations_requiring_reconciliation
            .entry(root.clone())
            .or_default();
        let marker = RemoteMutationReconciliationMarker {
            target: target.clone(),
            job_id,
            reason: reason.to_string(),
            recorded_unix_ms: crate::auto_flow::unix_ms(),
        };
        if let Some(existing) = markers
            .iter_mut()
            .find(|existing| existing.target == marker.target)
        {
            *existing = marker;
        } else {
            markers.push(marker);
        }
        let value = serde_json::to_string(markers)
            .map_err(|error| format!("encode remote mutation reconciliation marker: {error}"))?;
        for session in &mut self.sessions {
            if self
                .repos
                .get(session.repo_index)
                .is_some_and(|managed| managed.repo.root == root)
            {
                session.pr.require_reconciliation(
                    "remote mutation completion is unknown; authoritative re-observation required",
                );
            }
        }
        crate::persistence::database::upsert_metadata(
            &crate::observability::db_path(&repo),
            REMOTE_MUTATION_RECONCILIATION_KEY,
            &value,
        )
        .map_err(|error| format!("write remote mutation reconciliation marker: {error}"))?;
        Ok(())
    }

    pub(super) fn persist_remote_mutation_reconciliation_markers(
        &mut self,
        repository: &WorktreeRepositoryKey,
    ) -> Result<(), String> {
        let Some(managed) = self
            .repos
            .iter()
            .find(|managed| managed.identity == *repository)
        else {
            return Ok(());
        };
        let markers = self
            .remote_mutations_requiring_reconciliation
            .get(&repository.root)
            .cloned()
            .unwrap_or_default();
        let path = crate::observability::db_path(&managed.repo);
        if markers.is_empty() {
            crate::persistence::database::delete_metadata(
                &path,
                REMOTE_MUTATION_RECONCILIATION_KEY,
            )
            .map_err(|error| format!("clear remote mutation reconciliation marker: {error}"))?;
        } else {
            let value = serde_json::to_string(&markers).map_err(|error| {
                format!("encode remote mutation reconciliation marker: {error}")
            })?;
            crate::persistence::database::upsert_metadata(
                &path,
                REMOTE_MUTATION_RECONCILIATION_KEY,
                &value,
            )
            .map_err(|error| format!("write remote mutation reconciliation marker: {error}"))?;
        }
        if markers.is_empty() {
            self.remote_mutations_requiring_reconciliation
                .remove(&repository.root);
        }
        Ok(())
    }

    pub(crate) fn remote_push_reconciliation_refs(
        &self,
        repository: &WorktreeRepositoryKey,
    ) -> Vec<(String, String)> {
        self.remote_mutations_requiring_reconciliation
            .get(&repository.root)
            .into_iter()
            .flatten()
            .filter_map(|marker| match &marker.target {
                RemoteMutationTarget::Push { remote, branch, .. } => {
                    Some((remote.clone(), branch.clone()))
                }
                RemoteMutationTarget::Unknown { .. }
                | RemoteMutationTarget::Create { .. }
                | RemoteMutationTarget::Review { .. }
                | RemoteMutationTarget::Resolve { .. }
                | RemoteMutationTarget::Merge { .. } => None,
            })
            .collect()
    }

    pub(crate) fn reconcile_remote_mutation_summaries(
        &mut self,
        repository: &WorktreeRepositoryKey,
        summaries: &[PrSummary],
        remote_branch_heads: &BTreeMap<(String, String), String>,
    ) {
        self.retain_remote_mutation_markers(repository, |target| match target {
            RemoteMutationTarget::Unknown { .. } => true,
            RemoteMutationTarget::Push {
                remote,
                branch,
                expected_head_sha,
                ..
            } => {
                remote_branch_heads.get(&(remote.clone(), branch.clone()))
                    != Some(expected_head_sha)
            }
            RemoteMutationTarget::Create {
                source_provider,
                source_host,
                source_project,
                source_branch,
                expected_head_sha,
                target_provider,
                target_host,
                target_project,
                target_branch,
                ..
            } => !summaries.iter().any(|summary| {
                summary.head_ref == *source_branch
                    && summary.head_sha == *expected_head_sha
                    && (target_branch.is_empty() || summary.base_ref == *target_branch)
                    && summary
                        .change_request_identity
                        .as_ref()
                        .is_some_and(|identity| {
                            identity.source_provider() == *source_provider
                                && identity.source_canonical_host() == source_host
                                && identity.source_project_path() == source_project
                                && target_provider.is_none_or(|provider| {
                                    identity.target_provider() == provider
                                        && identity.target_canonical_host() == target_host
                                        && identity.target_project_path() == target_project
                                })
                        })
            }),
            RemoteMutationTarget::Merge {
                change_request,
                expected_head_sha,
            } => !summaries.iter().any(|summary| {
                summary.change_request_identity.as_ref() == Some(change_request)
                    && summary.head_sha == *expected_head_sha
                    && (summary.merged || merge_is_authoritatively_pending(&summary.queue_state))
            }),
            RemoteMutationTarget::Review { .. } | RemoteMutationTarget::Resolve { .. } => true,
        });
    }

    pub(crate) fn reconcile_remote_mutation_details(
        &mut self,
        repository: &WorktreeRepositoryKey,
        cache: &PrCache,
    ) {
        let Ok(Some(summary)) = cache.trusted_summary() else {
            return;
        };
        let Ok(Some(details)) = cache.trusted_details() else {
            return;
        };
        self.retain_remote_mutation_markers(repository, |target| match target {
            RemoteMutationTarget::Review {
                change_request,
                expected_state,
                expected_body,
                prior_review_ids,
            } => {
                summary.change_request_identity.as_ref() != Some(change_request)
                    || !details.reviews.iter().any(|review| {
                        review.state.eq_ignore_ascii_case(expected_state)
                            && review.body.trim() == expected_body.trim()
                            && !review.id.trim().is_empty()
                            && !prior_review_ids.contains(&review.id)
                    })
            }
            RemoteMutationTarget::Resolve {
                change_request,
                thread_ids,
            } => {
                summary.change_request_identity.as_ref() != Some(change_request)
                    || thread_ids.iter().any(|thread_id| {
                        !details
                            .review_comments
                            .iter()
                            .any(|comment| comment.thread_id == *thread_id && comment.resolved)
                    })
            }
            _ => true,
        });
    }

    pub(super) fn retain_remote_mutation_markers(
        &mut self,
        repository: &WorktreeRepositoryKey,
        mut retain: impl FnMut(&RemoteMutationTarget) -> bool,
    ) {
        let Some(previous) = self
            .remote_mutations_requiring_reconciliation
            .get(&repository.root)
            .cloned()
        else {
            return;
        };
        let retained = previous
            .iter()
            .filter(|marker| retain(&marker.target))
            .cloned()
            .collect::<Vec<_>>();
        if retained.len() == previous.len() {
            return;
        }
        self.remote_mutations_requiring_reconciliation
            .insert(repository.root.clone(), retained);
        if self
            .persist_remote_mutation_reconciliation_markers(repository)
            .is_err()
        {
            self.remote_mutations_requiring_reconciliation
                .insert(repository.root.clone(), previous);
        }
    }

    pub(super) fn retain_uncertain_remote_action_result(
        &mut self,
        key: &TuiJobKey,
        job_id: JobId,
        result: &Result<RemoteActionValue, String>,
        target: &RemoteMutationTarget,
    ) -> Result<(), String> {
        if let Some(error) = uncertain_remote_mutation_error(result) {
            self.record_remote_mutation_reconciliation(key, job_id, error, target)?;
        }
        Ok(())
    }

    pub(super) fn remote_action_reconciliation_blocked(
        &self,
        key: &TuiJobKey,
        target: &RemoteMutationTarget,
    ) -> bool {
        self.repository_root_for_job_key(key).is_some_and(|root| {
            self.remote_mutations_requiring_reconciliation
                .get(&root)
                .is_some_and(|markers| {
                    markers.iter().any(|marker| {
                        matches!(marker.target, RemoteMutationTarget::Unknown { .. })
                            || remote_mutation_targets_overlap(&marker.target, target)
                    })
                })
        })
    }

    pub(crate) fn run_remote_action<F>(
        &mut self,
        runtime: &mut TerminalRuntime,
        request: RemoteActionRequest<'_>,
        action: F,
    ) -> Result<RemoteActionValue, String>
    where
        F: FnOnce() -> Result<RemoteActionValue, String> + Send + 'static,
    {
        if request
            .mutation
            .as_ref()
            .is_some_and(|target| self.remote_action_reconciliation_blocked(&request.key, target))
        {
            return Err(
                "remote mutation blocked until the previous uncertain mutation is re-observed"
                    .to_string(),
            );
        }
        self.dialog = Some(view::DialogModel::Progress {
            title: request.title.to_string(),
            message: request.message.to_string(),
        });
        self.draw(runtime)?;
        let timeout = remote_action_timeout(request.abandon_cancelable);
        let reconciliation =
            request
                .mutation
                .clone()
                .map(|target| RemoteActionReconciliationContext {
                    key: request.key.clone(),
                    target,
                });
        let id = self.spawn_tui_job(
            TuiJobKind::RemoteAction,
            request.key,
            request.generation,
            timeout,
            request.name.to_string(),
            move |context| {
                Ok(Some(TuiJobPayload::RemoteAction(Box::new(
                    RemoteActionDelivery {
                        id: context.id(),
                        result: action(),
                    },
                ))))
            },
        );
        if let Some(reconciliation) = reconciliation.clone() {
            self.remote_actions_requiring_reconciliation.insert(id);
            self.remote_action_reconciliation_contexts
                .insert(id, reconciliation);
        }
        loop {
            self.tick_tui_action_jobs();
            while let Ok(delivery) = self.remote_action_rx.try_recv() {
                if delivery.id == id {
                    let result = delivery.result;
                    let reconciliation_error = if let Some(reconciliation) = &reconciliation {
                        self.retain_uncertain_remote_action_result(
                            &reconciliation.key,
                            id,
                            &result,
                            &reconciliation.target,
                        )
                        .err()
                    } else {
                        None
                    };
                    self.remote_actions_requiring_reconciliation.remove(&id);
                    self.remote_action_reconciliation_contexts.remove(&id);
                    self.dialog = None;
                    self.draw(runtime)?;
                    return match (result, reconciliation_error) {
                        (Err(error), Some(marker_error)) => Err(format!("{error}; {marker_error}")),
                        (result, _) => result,
                    };
                }
            }
            if let Some(error) = self.remote_action_failures.remove(&id) {
                self.remote_actions_requiring_reconciliation.remove(&id);
                self.remote_action_reconciliation_contexts.remove(&id);
                self.dialog = None;
                self.draw(runtime)?;
                return Err(error);
            }
            if let Some(event) = runtime.poll_event(Duration::from_millis(100))? {
                match event {
                    RuntimeEvent::Key(event)
                        if remote_action_abandon_requested(request.abandon_cancelable, event) =>
                    {
                        self.jobs.cancel(id);
                        self.dialog = None;
                        self.draw(runtime)?;
                        return Err("remote action canceled".to_string());
                    }
                    RuntimeEvent::Resize => self.draw(runtime)?,
                    RuntimeEvent::Key(_)
                    | RuntimeEvent::Mouse(_)
                    | RuntimeEvent::FocusGained
                    | RuntimeEvent::FocusLost => {}
                }
            }
        }
    }
}
