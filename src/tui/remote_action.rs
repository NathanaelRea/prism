use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};

use crate::remote::{PrCache, PrSummary};
use crate::session::WorktreeRepositoryKey;
use crate::tui_jobs::{JobContext, JobId};
use crate::tui_runtime::{RuntimeEvent, TerminalRuntime};
use crate::view;

use super::{
    REMOTE_MUTATION_RECONCILIATION_KEY, TUI_ACTION_JOB_TIMEOUT, Tui, TuiJobKey, TuiJobKind,
    TuiJobPayload, ctrl_key,
};

fn current_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
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
    pub effect: RemoteActionEffect,
}

#[derive(Clone)]
pub(crate) enum RemoteActionEffect {
    ReadOnly,
    LocalMutation,
    CoordinatedMutation {
        target: Box<RemoteMutationTarget>,
        ledger: Box<RemoteMutationLedgerContext>,
    },
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub(crate) struct RemoteMutationLedgerContext {
    pub repository: std::path::PathBuf,
    pub worktree: std::path::PathBuf,
    pub request_id: String,
    pub operation: crate::workflow::remote_operation::RemoteMutationOperation,
    pub subject: String,
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
    Fetch {
        change_request: crate::remote::CanonicalChangeRequestIdentity,
        branch: String,
        expected_head_sha: String,
    },
    Merge {
        change_request: crate::remote::CanonicalChangeRequestIdentity,
        expected_head_sha: String,
    },
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub(crate) struct RemoteMutationReconciliationMarker {
    pub(super) target: RemoteMutationTarget,
    pub(super) ledger: Option<RemoteMutationLedgerContext>,
    #[serde(skip)]
    pub(super) database_path: PathBuf,
    pub(super) job_id: JobId,
    pub(super) reason: String,
    pub(super) recorded_unix_ms: u64,
}

pub(super) fn update_persisted_remote_mutation_markers<T>(
    path: &Path,
    update: impl FnOnce(&mut Vec<RemoteMutationReconciliationMarker>) -> T,
) -> Result<T, String> {
    let mut output = None;
    crate::persistence::database::update_metadata(
        path,
        REMOTE_MUTATION_RECONCILIATION_KEY,
        |value| {
            let mut markers = value
                .map(|value| {
                    serde_json::from_str::<Vec<RemoteMutationReconciliationMarker>>(&value)
                })
                .transpose()
                .map_err(|error| format!("decode remote mutation reconciliation markers: {error}"))?
                .unwrap_or_default();
            output = Some(update(&mut markers));
            if markers.is_empty() {
                Ok(None)
            } else {
                serde_json::to_string(&markers).map(Some).map_err(|error| {
                    format!("encode remote mutation reconciliation markers: {error}")
                })
            }
        },
    )
    .map_err(|error| format!("update remote mutation reconciliation markers: {error}"))?;
    output.ok_or_else(|| "remote mutation marker update did not run".to_string())
}

#[derive(Clone)]
pub(super) struct RemoteActionReconciliationContext {
    pub(super) key: TuiJobKey,
    pub(super) target: RemoteMutationTarget,
    pub(super) ledger: RemoteMutationLedgerContext,
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
    Merge {
        cache: Box<PrCache>,
        outcome: crate::workflow::remote_operation::TuiRemoteMergeOutcome,
    },
    MergeRejected(String),
    Complete,
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
        Err(error) if crate::worker::remote_mutation_error_is_uncertain(error) => Some(error),
        Err(_) => None,
        Ok(RemoteActionValue::Merge {
            outcome: crate::workflow::remote_operation::TuiRemoteMergeOutcome::Uncertain,
            ..
        }) => Some("provider accepted the merge request but its outcome is not authoritative"),
        Ok(_) => None,
    }
}

pub(crate) struct LoadedRemoteMutationMarkers {
    pub repositories: std::collections::BTreeSet<WorktreeRepositoryKey>,
    pub markers: BTreeMap<WorktreeRepositoryKey, Vec<RemoteMutationReconciliationMarker>>,
    pub errors: Vec<RemoteMutationMarkerLoadError>,
}

pub(crate) struct RemoteMutationMarkerLoadError {
    pub repository: WorktreeRepositoryKey,
    pub database_path: PathBuf,
    pub message: String,
}

impl Tui {
    pub(crate) fn load_remote_mutation_reconciliation_markers(&mut self) {
        let candidates = self
            .repos
            .iter()
            .map(|managed| {
                (
                    managed.identity.clone(),
                    crate::observability::db_path(&managed.repo),
                )
            })
            .collect::<Vec<_>>();
        let repositories = candidates
            .into_iter()
            .filter(|(repository, _)| self.background.begin_marker_load(repository.clone()))
            .collect::<Vec<_>>();
        if repositories.is_empty() {
            return;
        }
        self.spawn_tui_job(
            TuiJobKind::RemoteReconciliation,
            TuiJobKey::System,
            0,
            None,
            "prism-load-remote-reconciliation".to_string(),
            move |_| {
                let mut loaded_repositories = std::collections::BTreeSet::new();
                let mut marked = BTreeMap::new();
                let mut errors = Vec::new();
                for (repository, database_path) in repositories {
                    loaded_repositories.insert(repository.clone());
                    let loaded = if database_path.exists() {
                        crate::persistence::database::load_metadata_readonly(
                            &database_path,
                            REMOTE_MUTATION_RECONCILIATION_KEY,
                        )
                    } else {
                        Ok(None)
                    }
                    .map_err(|error| format!("read remote mutation reconciliation marker: {error}"))
                    .and_then(|value| {
                        value
                            .map(|value| {
                                serde_json::from_str::<Vec<RemoteMutationReconciliationMarker>>(
                                    &value,
                                )
                                .map_err(|error| {
                                    format!("decode remote mutation reconciliation marker: {error}")
                                })
                            })
                            .transpose()
                            .map(|markers| markers.unwrap_or_default())
                    });
                    match loaded {
                        Ok(mut markers) => {
                            for marker in &mut markers {
                                marker.database_path = database_path.clone();
                            }
                            if !markers.is_empty() {
                                marked.insert(repository, markers);
                            }
                        }
                        Err(message) => errors.push(RemoteMutationMarkerLoadError {
                            repository,
                            database_path,
                            message,
                        }),
                    }
                }
                Ok(Some(TuiJobPayload::RemoteMarkersLoaded(
                    LoadedRemoteMutationMarkers {
                        repositories: loaded_repositories,
                        markers: marked,
                        errors,
                    },
                )))
            },
        );
    }

    pub(super) fn apply_loaded_remote_mutation_markers(
        &mut self,
        loaded: LoadedRemoteMutationMarkers,
    ) {
        let LoadedRemoteMutationMarkers {
            repositories,
            markers,
            errors,
        } = loaded;
        self.background.finish_marker_loads(&repositories);
        let current_repositories = self
            .repos
            .iter()
            .map(|managed| managed.identity.clone())
            .collect::<std::collections::BTreeSet<_>>();
        let loaded_repositories = repositories
            .intersection(&current_repositories)
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let markers = markers
            .into_iter()
            .filter(|(repository, _)| loaded_repositories.contains(repository))
            .collect::<BTreeMap<_, _>>();
        let errors = errors
            .into_iter()
            .filter(|error| loaded_repositories.contains(&error.repository))
            .collect::<Vec<_>>();
        for session in &mut self.sessions {
            if self.repos.get(session.repo_index).is_some_and(|managed| {
                markers.contains_key(&managed.identity)
                    || errors
                        .iter()
                        .any(|error| error.repository == managed.identity)
            }) {
                session.pr.require_reconciliation(
                    "remote mutation completion is unknown; authoritative re-observation required",
                );
            }
        }
        self.background.apply_loaded_markers(
            loaded_repositories,
            markers
                .into_iter()
                .map(|(repository, markers)| (repository.root, markers))
                .collect(),
        );
        for error in errors {
            self.background.push_shutdown_error(error.message.clone());
            self.background.upsert_marker(
                error.repository.root,
                RemoteMutationReconciliationMarker {
                    target: RemoteMutationTarget::Unknown {
                        marker_id: format!("marker-load-error:{}", error.database_path.display()),
                    },
                    ledger: None,
                    database_path: error.database_path,
                    job_id: 0,
                    reason: error.message,
                    recorded_unix_ms: 0,
                },
            );
        }
        if current_repositories
            .iter()
            .any(|repository| !self.background.markers_are_loaded(repository))
        {
            self.load_remote_mutation_reconciliation_markers();
        }
    }

    pub(super) fn record_remote_mutation_reconciliation(
        &mut self,
        key: &TuiJobKey,
        job_id: JobId,
        reason: &str,
        target: &RemoteMutationTarget,
        ledger: Option<&RemoteMutationLedgerContext>,
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
        let marker = RemoteMutationReconciliationMarker {
            target: target.clone(),
            ledger: ledger.cloned(),
            database_path: crate::observability::db_path(&repo),
            job_id,
            reason: reason.to_string(),
            recorded_unix_ms: current_unix_ms(),
        };
        let marker_to_persist = marker.clone();
        self.background.upsert_marker(root.clone(), marker);
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
        self.background.persist_marker(root, marker_to_persist)
    }

    pub(crate) fn remote_push_reconciliation_refs(
        &self,
        repository: &WorktreeRepositoryKey,
    ) -> Vec<(String, String)> {
        self.background
            .markers(&repository.root)
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
                | RemoteMutationTarget::Fetch { .. }
                | RemoteMutationTarget::Merge { .. } => None,
            })
            .collect()
    }

    pub(super) fn retain_uncertain_remote_action_result(
        &mut self,
        key: &TuiJobKey,
        job_id: JobId,
        result: &Result<RemoteActionValue, String>,
        target: &RemoteMutationTarget,
        ledger: &RemoteMutationLedgerContext,
    ) -> Result<(), String> {
        if let Some(error) = uncertain_remote_mutation_error(result) {
            self.record_remote_mutation_reconciliation(key, job_id, error, target, Some(ledger))?;
        }
        Ok(())
    }

    pub(super) fn remote_action_reconciliation_blocked(
        &self,
        key: &TuiJobKey,
        target: &RemoteMutationTarget,
    ) -> bool {
        self.repository_key_for_job_key(key)
            .is_some_and(|repository| self.background.marker_blocks(repository, target))
    }

    pub(crate) fn run_remote_action<F>(
        &mut self,
        runtime: &mut TerminalRuntime,
        request: RemoteActionRequest<'_>,
        action: F,
    ) -> Result<RemoteActionValue, String>
    where
        F: FnOnce(
                JobContext<TuiJobKind, TuiJobKey, TuiJobPayload>,
            ) -> Result<RemoteActionValue, String>
            + Send
            + 'static,
    {
        if matches!(
            &request.effect,
            RemoteActionEffect::CoordinatedMutation { target, .. }
                if self.remote_action_reconciliation_blocked(&request.key, target)
        ) {
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
        let reconciliation = match &request.effect {
            RemoteActionEffect::CoordinatedMutation { target, ledger } => {
                Some(RemoteActionReconciliationContext {
                    key: request.key.clone(),
                    target: target.as_ref().clone(),
                    ledger: ledger.as_ref().clone(),
                })
            }
            RemoteActionEffect::ReadOnly | RemoteActionEffect::LocalMutation => None,
        };
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
                        result: action(context),
                    },
                ))))
            },
        );
        if let Some(reconciliation) = reconciliation.clone() {
            self.background.track_remote_action(id, reconciliation);
        }
        loop {
            self.tick_tui_action_jobs();
            self.draw(runtime)?;
            while let Some(delivery) = self.background.receive_remote_action() {
                if delivery.id == id {
                    let result = delivery.result;
                    let reconciliation_error = if let Some(reconciliation) = &reconciliation {
                        self.retain_uncertain_remote_action_result(
                            &reconciliation.key,
                            id,
                            &result,
                            &reconciliation.target,
                            &reconciliation.ledger,
                        )
                        .err()
                    } else {
                        None
                    };
                    self.background.finish_remote_action(id);
                    self.dialog = None;
                    self.draw(runtime)?;
                    return match (result, reconciliation_error) {
                        (Err(error), Some(marker_error)) => Err(format!("{error}; {marker_error}")),
                        (result, _) => result,
                    };
                }
            }
            if let Some(error) = self.background.take_remote_failure(id) {
                self.background.finish_remote_action(id);
                self.dialog = None;
                self.draw(runtime)?;
                return Err(error);
            }
            if let Some(event) = runtime.poll_event(Duration::from_millis(100))? {
                match event {
                    RuntimeEvent::Key(event)
                        if remote_action_abandon_requested(request.abandon_cancelable, event) =>
                    {
                        self.background.cancel(id);
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
