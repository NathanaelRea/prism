#![allow(dead_code)] // Adapters are registered by the generalized worker at cutover.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;

use super::{EffectAdapter, EffectIntent, ReconciliationResult};

pub(crate) struct GitEffectAdapter {
    config: Arc<crate::config::Config>,
    workspaces: BTreeMap<String, PathBuf>,
}

impl GitEffectAdapter {
    pub(crate) fn new(
        config: Arc<crate::config::Config>,
        workspaces: BTreeMap<String, PathBuf>,
    ) -> Result<Self, String> {
        if workspaces.values().any(|path| !path.is_absolute()) {
            return Err("Git effect workspace paths must be absolute".to_string());
        }
        Ok(Self { config, workspaces })
    }

    fn target<'a>(&'a self, intent: &'a EffectIntent) -> Result<(&'a Path, GitTarget), String> {
        let target: GitTarget =
            serde_json::from_value(intent.target.clone()).map_err(|error| error.to_string())?;
        let path = self
            .workspaces
            .get(&target.workspace_id)
            .ok_or_else(|| format!("unknown Execution Workspace '{}'", target.workspace_id))?;
        Ok((path, target))
    }

    fn reconcile_head(
        &self,
        intent: &EffectIntent,
        path: &Path,
    ) -> Result<ReconciliationResult, String> {
        let current = crate::git::current_head_sha(path, &self.config)?;
        if intent.kind == "git_commit" && intent.exact_head.is_none() {
            let expected = intent
                .expected_pre_state
                .get("head")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| "git_commit intent requires expected head".to_string())?;
            if current == expected {
                return Ok(ReconciliationResult::NotApplied {
                    preconditions_still_hold: true,
                    evidence: serde_json::json!({"head":current}),
                });
            }
            let (_, target) = self.target(intent)?;
            let expected_message = target
                .message
                .as_deref()
                .ok_or_else(|| "git_commit target requires message".to_string())?;
            let (parent, message) = crate::git::commit_parent_and_message(path, &self.config)?;
            if parent.as_deref() == Some(expected) && message == expected_message {
                return Ok(ReconciliationResult::Applied {
                    evidence: serde_json::json!({"head":current,"parent":parent,"message":message}),
                });
            }
            return Ok(ReconciliationResult::Diverged {
                reason: "local Git HEAD is not the broker-recorded commit".into(),
                evidence: serde_json::json!({"head":current,"parent":parent,"message":message}),
            });
        }
        if intent.exact_head.as_deref() == Some(current.as_str()) {
            return Ok(ReconciliationResult::Applied {
                evidence: serde_json::json!({"head":current}),
            });
        }
        let expected = intent
            .expected_pre_state
            .get("head")
            .and_then(serde_json::Value::as_str);
        if expected == Some(current.as_str()) {
            Ok(ReconciliationResult::NotApplied {
                preconditions_still_hold: true,
                evidence: serde_json::json!({"head":current}),
            })
        } else {
            Ok(ReconciliationResult::Diverged {
                reason: "local Git HEAD differs from both expected and desired state".into(),
                evidence: serde_json::json!({"head":current}),
            })
        }
    }

    fn reconcile_push(
        &self,
        intent: &EffectIntent,
        path: &Path,
        target: &GitTarget,
    ) -> Result<ReconciliationResult, String> {
        let branch = target
            .branch
            .as_deref()
            .ok_or_else(|| "push target requires branch".to_string())?;
        let remote = target.remote.as_deref().unwrap_or("origin");
        let current = crate::git::push_remote_branch_head_sha(path, remote, branch, &self.config)?;
        if current.as_deref() == intent.exact_head.as_deref() {
            return Ok(ReconciliationResult::Applied {
                evidence: serde_json::json!({"remote":remote,"branch":branch,"head":current}),
            });
        }
        let expected = intent
            .expected_pre_state
            .get("head")
            .and_then(serde_json::Value::as_str);
        if current.as_deref() == expected {
            Ok(ReconciliationResult::NotApplied {
                preconditions_still_hold: true,
                evidence: serde_json::json!({"remote":remote,"branch":branch,"head":current}),
            })
        } else {
            Ok(ReconciliationResult::Diverged {
                reason: "remote ref differs from both expected and desired head".into(),
                evidence: serde_json::json!({"remote":remote,"branch":branch,"head":current}),
            })
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GitTarget {
    workspace_id: String,
    remote: Option<String>,
    branch: Option<String>,
    message: Option<String>,
    base_head: Option<String>,
}

impl EffectAdapter for GitEffectAdapter {
    fn dispatch(&self, intent: &EffectIntent) -> Result<ReconciliationResult, String> {
        let (path, target) = self.target(intent)?;
        let before = match intent.kind.as_str() {
            "push" => self.reconcile_push(intent, path, &target)?,
            "git_commit" | "git_ref" => self.reconcile_head(intent, path)?,
            kind => return Err(format!("Git adapter does not support effect kind '{kind}'")),
        };
        if let ReconciliationResult::Applied { evidence } = before {
            return Ok(ReconciliationResult::ExternallySatisfied { evidence });
        }
        if !matches!(
            before,
            ReconciliationResult::NotApplied {
                preconditions_still_hold: true,
                ..
            }
        ) {
            return Ok(before);
        }
        match intent.kind.as_str() {
            "push" => {
                if target.remote.as_deref().unwrap_or("origin") != "origin" {
                    return Ok(ReconciliationResult::Diverged {
                        reason: "the reusable push operation currently requires remote 'origin'"
                            .into(),
                        evidence: serde_json::json!({"remote":target.remote}),
                    });
                }
                let desired = intent
                    .exact_head
                    .as_deref()
                    .ok_or_else(|| "push intent requires exact_head".to_string())?;
                let local = crate::git::current_head_sha(path, &self.config)?;
                if local != desired {
                    return Ok(ReconciliationResult::Diverged {
                        reason: "local HEAD no longer equals the exact push head".into(),
                        evidence: serde_json::json!({"local_head":local,"desired_head":desired}),
                    });
                }
                crate::lifecycle::push_branch(
                    &self.config,
                    path,
                    target
                        .branch
                        .as_deref()
                        .ok_or_else(|| "push target requires branch".to_string())?,
                    true,
                )?;
                self.reconcile_push(intent, path, &target)
            }
            "git_commit" => {
                let message = target
                    .message
                    .as_deref()
                    .ok_or_else(|| "git_commit target requires message".to_string())?;
                crate::git::commit_if_dirty(path, &self.config, message)?;
                self.reconcile_head(intent, path)
            }
            "git_ref" => {
                let remote = target.remote.as_deref().unwrap_or("origin");
                let branch = target
                    .branch
                    .as_deref()
                    .ok_or_else(|| "git_ref target requires branch".to_string())?;
                let expected_head = intent
                    .expected_pre_state
                    .get("head")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| "git_ref intent requires expected head".to_string())?;
                let base_head = target
                    .base_head
                    .as_deref()
                    .ok_or_else(|| "git_ref target requires base_head".to_string())?;
                crate::git::merge_remote_branch_guarded(
                    path,
                    remote,
                    branch,
                    expected_head,
                    base_head,
                    &self.config,
                )?;
                self.reconcile_head(intent, path)
            }
            _ => unreachable!(),
        }
    }

    fn reconcile(&self, intent: &EffectIntent) -> Result<ReconciliationResult, String> {
        let (path, target) = self.target(intent)?;
        if intent.kind == "push" {
            self.reconcile_push(intent, path, &target)
        } else {
            self.reconcile_head(intent, path)
        }
    }
}

pub(crate) struct WorktrunkEffectAdapter {
    config: Arc<crate::config::Config>,
    repositories: BTreeMap<String, crate::repo::Repository>,
    workspaces: BTreeMap<String, PathBuf>,
}

pub(crate) struct ProviderEffectAdapter {
    config: Arc<crate::config::Config>,
    repositories: BTreeMap<String, crate::repo::Repository>,
    workspaces: BTreeMap<String, PathBuf>,
}

impl ProviderEffectAdapter {
    pub(crate) fn new(
        config: Arc<crate::config::Config>,
        repositories: BTreeMap<String, crate::repo::Repository>,
        workspaces: BTreeMap<String, PathBuf>,
    ) -> Result<Self, String> {
        if workspaces.values().any(|path| !path.is_absolute()) {
            return Err("Provider effect workspace paths must be absolute".to_string());
        }
        Ok(Self {
            config,
            repositories,
            workspaces,
        })
    }

    fn workspace(&self, workspace_id: &str) -> Result<&Path, String> {
        self.workspaces
            .get(workspace_id)
            .map(PathBuf::as_path)
            .ok_or_else(|| format!("unknown Execution Workspace '{workspace_id}'"))
    }

    fn observe_merge(
        &self,
        path: &Path,
        target: &MergeProviderTarget,
    ) -> Result<ReconciliationResult, String> {
        let summary = crate::remote::dispatcher::observe_change_request_identity(
            path,
            &self.config,
            &target.identity,
            target.display_number,
        )?;
        if summary.change_request.head_sha != target.expected_head {
            return Ok(ReconciliationResult::Diverged {
                reason: "change request head changed before merge".into(),
                evidence: serde_json::json!({
                    "display_number":target.display_number,
                    "head":summary.change_request.head_sha
                }),
            });
        }
        if summary.lifecycle == crate::remote::LifecycleState::Merged {
            return Ok(ReconciliationResult::Applied {
                evidence: serde_json::json!({
                    "display_number":target.display_number,
                    "head":summary.change_request.head_sha,
                    "lifecycle":"merged"
                }),
            });
        }
        Ok(ReconciliationResult::NotApplied {
            preconditions_still_hold: true,
            evidence: serde_json::json!({
                "display_number":target.display_number,
                "head":summary.change_request.head_sha,
                "lifecycle":format!("{:?}",summary.lifecycle)
            }),
        })
    }

    fn observe_create(
        &self,
        path: &Path,
        target: &CreateProviderTarget,
    ) -> Result<ReconciliationResult, String> {
        let repository = crate::remote::RemoteRepositoryId::new(
            target.target_provider,
            crate::remote::HostIdentity::new(&target.target_host, None)
                .map_err(|error| error.to_string())?,
            target.target_project.clone(),
        )
        .map_err(|error| error.to_string())?;
        match crate::remote::dispatcher::observe_change_request_for_source(
            path,
            &self.config,
            &repository,
            &target.branch,
            &target.expected_head,
        )? {
            Some(summary) => Ok(ReconciliationResult::Applied {
                evidence: serde_json::json!({
                    "head":summary.change_request.head_sha,
                    "branch":summary.change_request.source_branch,
                    "lifecycle":format!("{:?}",summary.lifecycle)
                }),
            }),
            None => Ok(ReconciliationResult::NotApplied {
                preconditions_still_hold: crate::git::current_head_sha(path, &self.config)?
                    == target.expected_head,
                evidence: serde_json::json!({"branch":target.branch,"head":target.expected_head}),
            }),
        }
    }

    fn issue_repository(
        target: &ProviderItemEffectTarget,
    ) -> Result<crate::remote::RemoteRepositoryId, String> {
        crate::remote::RemoteRepositoryId::new(
            target.provider,
            crate::remote::HostIdentity::parse(&target.host).map_err(|error| error.to_string())?,
            &target.project,
        )
        .map_err(|error| error.to_string())
    }

    fn observe_issue_effect(
        &self,
        path: &Path,
        target: &ProviderItemEffectTarget,
        intent: &EffectIntent,
    ) -> Result<ReconciliationResult, String> {
        let repository = Self::issue_repository(target)?;
        if intent.kind == "issue_comment" {
            let marker = &intent.reconciliation_key;
            if crate::remote::dispatcher::issue_has_comment_marker(
                path,
                &self.config,
                &repository,
                &target.native_id,
                marker,
            )? {
                return Ok(ReconciliationResult::Applied {
                    evidence: serde_json::json!({"provider_item":target.native_id,"comment_marker":marker}),
                });
            }
        }
        let observed = crate::remote::dispatcher::observe_issue(
            path,
            &self.config,
            &repository,
            &target.native_id,
        )?;
        let revision = observed.revision();
        if intent.kind == "issue_labels" {
            let desired = target
                .labels
                .iter()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>();
            let actual = observed
                .labels
                .values()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>();
            if actual == desired {
                return Ok(ReconciliationResult::Applied {
                    evidence: serde_json::json!({"provider_item":target.native_id,"observation_revision":revision,"labels":actual}),
                });
            }
        } else if intent.kind == "issue_assignment" {
            let desired = target
                .assignees
                .iter()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>();
            let actual = observed
                .assignees
                .iter()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>();
            if actual == desired {
                return Ok(ReconciliationResult::Applied {
                    evidence: serde_json::json!({"provider_item":target.native_id,"observation_revision":revision,"assignees":actual}),
                });
            }
        } else if intent.kind == "issue_lifecycle"
            && target.lifecycle.as_deref() == Some(observed.lifecycle.as_str())
        {
            return Ok(ReconciliationResult::Applied {
                evidence: serde_json::json!({"provider_item":target.native_id,"observation_revision":revision,"lifecycle":observed.lifecycle}),
            });
        }
        if revision != target.expected_observation_revision {
            return Ok(ReconciliationResult::Diverged {
                reason: "Provider Item changed since effect authorization".into(),
                evidence: serde_json::json!({"expected_observation_revision":target.expected_observation_revision,"actual_observation_revision":revision}),
            });
        }
        Ok(ReconciliationResult::NotApplied {
            preconditions_still_hold: true,
            evidence: serde_json::json!({"provider_item":target.native_id,"observation_revision":revision}),
        })
    }

    fn observe_thread(
        &self,
        path: &Path,
        target: &ThreadProviderTarget,
    ) -> Result<ReconciliationResult, String> {
        match crate::remote::dispatcher::review_thread_resolution_state(
            path,
            &self.config,
            &target.identity,
            target.display_number,
            &target.expected_head,
            &target.thread_id,
        )? {
            Some(true) => Ok(ReconciliationResult::Applied {
                evidence: serde_json::json!({"thread_id":target.thread_id,"resolved":true}),
            }),
            Some(false) => Ok(ReconciliationResult::NotApplied {
                preconditions_still_hold: true,
                evidence: serde_json::json!({"thread_id":target.thread_id,"resolved":false}),
            }),
            None => Ok(ReconciliationResult::Diverged {
                reason: "recorded review thread is no longer present".into(),
                evidence: serde_json::json!({"thread_id":target.thread_id}),
            }),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MergeProviderTarget {
    workspace_id: String,
    identity: crate::remote::CanonicalChangeRequestIdentity,
    display_number: u64,
    expected_head: String,
    submission_mode: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateProviderTarget {
    workspace_id: String,
    repository_id: String,
    branch: String,
    expected_head: String,
    target_provider: crate::remote::ProviderKind,
    target_host: String,
    target_project: String,
    body: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ThreadProviderTarget {
    workspace_id: String,
    identity: crate::remote::CanonicalChangeRequestIdentity,
    display_number: u64,
    expected_head: String,
    thread_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderItemEffectTarget {
    workspace_id: Option<String>,
    repository_id: String,
    provider: crate::remote::ProviderKind,
    host: String,
    project: String,
    native_id: String,
    expected_observation_revision: String,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    comment: Option<String>,
    #[serde(default)]
    assignees: Vec<String>,
    #[serde(default)]
    lifecycle: Option<String>,
}

impl EffectAdapter for ProviderEffectAdapter {
    fn dispatch(&self, intent: &EffectIntent) -> Result<ReconciliationResult, String> {
        if matches!(
            intent.kind.as_str(),
            "issue_labels" | "issue_comment" | "issue_assignment" | "issue_lifecycle"
        ) {
            let target: ProviderItemEffectTarget =
                serde_json::from_value(intent.target.clone()).map_err(|error| error.to_string())?;
            let path = match target.workspace_id.as_deref() {
                Some(id) => self.workspace(id)?,
                None => self
                    .repositories
                    .get(&target.repository_id)
                    .map(|repo| repo.root.as_path())
                    .ok_or_else(|| format!("unknown RepositoryId '{}'", target.repository_id))?,
            };
            let before = self.observe_issue_effect(path, &target, intent)?;
            if let ReconciliationResult::Applied { evidence } = before {
                return Ok(ReconciliationResult::ExternallySatisfied { evidence });
            }
            if !matches!(
                before,
                ReconciliationResult::NotApplied {
                    preconditions_still_hold: true,
                    ..
                }
            ) {
                return Ok(before);
            }
            let repository = Self::issue_repository(&target)?;
            match intent.kind.as_str() {
                "issue_labels" => {
                    crate::remote::dispatcher::set_issue_labels(
                        path,
                        &self.config,
                        &repository,
                        &target.native_id,
                        &target.labels,
                    )?;
                }
                "issue_assignment" => {
                    crate::remote::dispatcher::set_issue_assignees(
                        path,
                        &self.config,
                        &repository,
                        &target.native_id,
                        &target.assignees,
                    )?;
                }
                "issue_lifecycle" => {
                    let lifecycle = target.lifecycle.as_deref().ok_or_else(|| {
                        "issue_lifecycle effect requires a desired lifecycle".to_string()
                    })?;
                    crate::remote::dispatcher::set_issue_lifecycle(
                        path,
                        &self.config,
                        &repository,
                        &target.native_id,
                        lifecycle,
                    )?;
                }
                "issue_comment" => {
                    let body = target.comment.as_deref().ok_or_else(|| {
                        "issue_comment effect requires a comment body".to_string()
                    })?;
                    crate::remote::dispatcher::create_issue_comment(
                        path,
                        &self.config,
                        &repository,
                        &target.native_id,
                        body,
                        &intent.reconciliation_key,
                    )?;
                }
                _ => unreachable!(),
            }
            return match self.observe_issue_effect(path, &target, intent)? {
                ReconciliationResult::NotApplied { .. } => {
                    Ok(ReconciliationResult::Indeterminate {
                        reason:
                            "provider accepted Issue mutation but desired state was not observed"
                                .into(),
                    })
                }
                result => Ok(result),
            };
        }
        if intent.kind == "create_change_request" {
            let target: CreateProviderTarget =
                serde_json::from_value(intent.target.clone()).map_err(|error| error.to_string())?;
            let path = self.workspace(&target.workspace_id)?;
            let before = self.observe_create(path, &target)?;
            if let ReconciliationResult::Applied { evidence } = before {
                return Ok(ReconciliationResult::ExternallySatisfied { evidence });
            }
            if !matches!(
                before,
                ReconciliationResult::NotApplied {
                    preconditions_still_hold: true,
                    ..
                }
            ) {
                return Ok(before);
            }
            let target_repository = crate::remote::RemoteRepositoryId::new(
                target.target_provider,
                crate::remote::HostIdentity::new(&target.target_host, None)
                    .map_err(|error| error.to_string())?,
                target.target_project.clone(),
            )
            .map_err(|error| error.to_string())?;
            let source_push =
                crate::remote::dispatcher::prepare_push(path, &self.config, &target.branch)?;
            let guard = crate::remote::dispatcher::prepare_create_change_request(
                path,
                &self.config,
                &target.branch,
                &target_repository,
                &source_push,
            )?;
            let repo = self
                .repositories
                .get(&target.repository_id)
                .ok_or_else(|| format!("unknown RepositoryId '{}'", target.repository_id))?;
            let mut cache = crate::remote::PrCache::default();
            crate::remote::dispatcher::create_change_request(
                repo,
                &self.config,
                path,
                &target.body,
                &guard,
                &mut cache,
            )?;
            return self.observe_create(path, &target);
        }
        if intent.kind == "resolve_review_thread" {
            let target: ThreadProviderTarget =
                serde_json::from_value(intent.target.clone()).map_err(|error| error.to_string())?;
            let path = self.workspace(&target.workspace_id)?;
            let before = self.observe_thread(path, &target)?;
            if let ReconciliationResult::Applied { evidence } = before {
                return Ok(ReconciliationResult::ExternallySatisfied { evidence });
            }
            if !matches!(
                before,
                ReconciliationResult::NotApplied {
                    preconditions_still_hold: true,
                    ..
                }
            ) {
                return Ok(before);
            }
            crate::remote::dispatcher::resolve_review_thread_identity(
                path,
                &self.config,
                &target.identity,
                target.display_number,
                &target.expected_head,
                &target.thread_id,
            )?;
            return self.observe_thread(path, &target);
        }
        if intent.kind != "merge" {
            return Err(format!(
                "Provider adapter does not support effect kind '{}'",
                intent.kind
            ));
        }
        let target: MergeProviderTarget =
            serde_json::from_value(intent.target.clone()).map_err(|error| error.to_string())?;
        let path = self.workspace(&target.workspace_id)?;
        if intent.exact_head.as_deref() != Some(target.expected_head.as_str()) {
            return Ok(ReconciliationResult::Diverged {
                reason:
                    "provider merge target head does not match the broker-authorized exact head"
                        .into(),
                evidence: serde_json::json!({
                    "authorized_head":intent.exact_head,
                    "target_head":target.expected_head
                }),
            });
        }
        let before = self.observe_merge(path, &target)?;
        if let ReconciliationResult::Applied { evidence } = before {
            return Ok(ReconciliationResult::ExternallySatisfied { evidence });
        }
        if !matches!(
            before,
            ReconciliationResult::NotApplied {
                preconditions_still_hold: true,
                ..
            }
        ) {
            return Ok(before);
        }
        let mode = match target.submission_mode.as_str() {
            "immediate" => crate::remote::MergeSubmissionMode::Immediate,
            "native_queue" => crate::remote::MergeSubmissionMode::NativeQueue,
            value => return Err(format!("invalid merge submission mode '{value}'")),
        };
        crate::remote::dispatcher::merge_change_request(
            &self.config,
            path,
            &target.identity,
            target.display_number,
            &target.expected_head,
            mode,
        )?;
        match self.observe_merge(path, &target)? {
            ReconciliationResult::NotApplied { .. } => Ok(ReconciliationResult::Indeterminate {
                reason: "provider accepted merge but has not reported a terminal state".into(),
            }),
            result => Ok(result),
        }
    }

    fn reconcile(&self, intent: &EffectIntent) -> Result<ReconciliationResult, String> {
        match intent.kind.as_str() {
            "merge" => {
                let target: MergeProviderTarget = serde_json::from_value(intent.target.clone())
                    .map_err(|error| error.to_string())?;
                self.observe_merge(self.workspace(&target.workspace_id)?, &target)
            }
            "create_change_request" => {
                let target: CreateProviderTarget = serde_json::from_value(intent.target.clone())
                    .map_err(|error| error.to_string())?;
                self.observe_create(self.workspace(&target.workspace_id)?, &target)
            }
            "resolve_review_thread" => {
                let target: ThreadProviderTarget = serde_json::from_value(intent.target.clone())
                    .map_err(|error| error.to_string())?;
                self.observe_thread(self.workspace(&target.workspace_id)?, &target)
            }
            "issue_labels" | "issue_comment" | "issue_assignment" | "issue_lifecycle" => {
                let target: ProviderItemEffectTarget =
                    serde_json::from_value(intent.target.clone())
                        .map_err(|error| error.to_string())?;
                let path = match target.workspace_id.as_deref() {
                    Some(id) => self.workspace(id)?,
                    None => self
                        .repositories
                        .get(&target.repository_id)
                        .map(|repo| repo.root.as_path())
                        .ok_or_else(|| {
                            format!("unknown RepositoryId '{}'", target.repository_id)
                        })?,
                };
                self.observe_issue_effect(path, &target, intent)
            }
            kind => Err(format!(
                "Provider adapter does not support effect kind '{kind}'"
            )),
        }
    }
}

impl WorktrunkEffectAdapter {
    pub(crate) fn new(
        config: Arc<crate::config::Config>,
        repositories: BTreeMap<String, crate::repo::Repository>,
        workspaces: BTreeMap<String, PathBuf>,
    ) -> Result<Self, String> {
        if workspaces.values().any(|path| !path.is_absolute()) {
            return Err("Worktrunk effect workspace paths must be absolute".to_string());
        }
        Ok(Self {
            config,
            repositories,
            workspaces,
        })
    }

    fn target<'a>(
        &'a self,
        intent: &'a EffectIntent,
    ) -> Result<(&'a crate::repo::Repository, WorktrunkTarget), String> {
        let target: WorktrunkTarget =
            serde_json::from_value(intent.target.clone()).map_err(|error| error.to_string())?;
        let repo = self
            .repositories
            .get(&target.repository_id)
            .ok_or_else(|| format!("unknown RepositoryId '{}'", target.repository_id))?;
        if !self.workspaces.contains_key(&target.workspace_id) {
            return Err(format!(
                "unknown Execution Workspace '{}'",
                target.workspace_id
            ));
        }
        Ok((repo, target))
    }

    fn observe(
        &self,
        intent: &EffectIntent,
    ) -> Result<(crate::repo::Repository, WorktrunkTarget, bool), String> {
        let (repo, target) = self.target(intent)?;
        let path = self
            .workspaces
            .get(&target.workspace_id)
            .expect("validated");
        let snapshot = crate::worktrunk::observe_repository(repo, &self.config)
            .map_err(|error| error.to_string())?;
        let present = snapshot.by_path.contains_key(path);
        Ok((repo.clone(), target, present))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorktrunkTarget {
    repository_id: String,
    workspace_id: String,
    operation: String,
    branch: Option<String>,
    create: Option<bool>,
    base: Option<String>,
}

impl EffectAdapter for WorktrunkEffectAdapter {
    fn dispatch(&self, intent: &EffectIntent) -> Result<ReconciliationResult, String> {
        let (repo, target, present) = self.observe(intent)?;
        let path = self
            .workspaces
            .get(&target.workspace_id)
            .expect("validated");
        match target.operation.as_str() {
            "switch" if present => Ok(ReconciliationResult::ExternallySatisfied {
                evidence: serde_json::json!({"workspace_id":target.workspace_id,"present":true}),
            }),
            "switch" => {
                let outcome = crate::worktrunk::switch_worktree(crate::worktrunk::SwitchRequest {
                    repo: &repo,
                    config: &self.config,
                    branch: target
                        .branch
                        .as_deref()
                        .ok_or_else(|| "switch target requires branch".to_string())?,
                    create: target.create.unwrap_or(false),
                    base: target.base.as_deref(),
                })
                .map_err(|error| error.to_string())?;
                Ok(ReconciliationResult::Applied {
                    evidence: serde_json::json!({"path":outcome.path,"branch":outcome.branch}),
                })
            }
            "remove" if !present => Ok(ReconciliationResult::ExternallySatisfied {
                evidence: serde_json::json!({"workspace_id":target.workspace_id,"present":false}),
            }),
            "remove" => {
                let outcome = crate::worktrunk::remove_worktree(crate::worktrunk::RemoveRequest {
                    repo: &repo,
                    config: &self.config,
                    path,
                })
                .map_err(|error| error.to_string())?;
                Ok(ReconciliationResult::Applied {
                    evidence: serde_json::json!({"path":outcome.path,"branch":outcome.branch}),
                })
            }
            operation => Err(format!("unsupported Worktrunk operation '{operation}'")),
        }
    }

    fn reconcile(&self, intent: &EffectIntent) -> Result<ReconciliationResult, String> {
        let (_, target, present) = self.observe(intent)?;
        let applied =
            (target.operation == "switch" && present) || (target.operation == "remove" && !present);
        if applied {
            Ok(ReconciliationResult::Applied {
                evidence: serde_json::json!({"workspace_id":target.workspace_id,"present":present}),
            })
        } else {
            Ok(ReconciliationResult::NotApplied {
                preconditions_still_hold: true,
                evidence: serde_json::json!({"workspace_id":target.workspace_id,"present":present}),
            })
        }
    }
}
