use crate::process::Command;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest as _, Sha256};

use crate::config::Config;
use crate::repo::Repository;

#[cfg(test)]
use super::HostIdentity;
use super::cache::{
    self, CiFailure as LegacyCiFailure, PrCache, PrCheckContext, PrCheckState, PrComment, PrReview,
    PrReviewComment, PrSummary, ProviderDetailsObservation, RepoPolicyCache,
};
#[cfg(test)]
use super::cache::{PrDetails, PrObservationQuality};
use super::forgejo::ForgejoAdapter;
use super::github::GitHubAdapter;
use super::gitlab::GitLabAdapter;
use super::{
    CanonicalChangeRequestIdentity, Capabilities, ChangeRequest, ChangeRequestDetails,
    ChangeRequestId, ChangeRequestSummary, CheckState, CreateChangeRequest, DiscoveredRemote,
    GuardedMerge, LifecycleState, MergeMethod, MergeMutationOutcome, MergeMutationResult,
    MergeabilityState, NativeReviewThreadId, Observation, ProviderItemObservation, ProviderKind,
    QueueState, RemoteError, RemoteErrorClass, RemoteOperation, RemoteRepositoryId, RemoteUrlKind,
    RepositoryPolicy, ResolveReviewThread, Retryability, ReviewDecision, ReviewSubmissionKind,
    SubmitReview, discover_git_remote,
};

const MERGE_VERIFY_ATTEMPTS: usize = 6;
const MERGE_VERIFY_INTERVAL: Duration = Duration::from_millis(500);

enum Adapter<'a> {
    GitHub(GitHubAdapter<'a>),
    GitLab(GitLabAdapter),
    Forgejo(Box<ForgejoAdapter>),
}

impl<'a> Adapter<'a> {
    async fn resolve(
        path: &'a Path,
        config: &'a Config,
    ) -> Result<(Self, DiscoveredRemote), String> {
        let discovered = discover_git_remote(path, config, "origin", RemoteUrlKind::Fetch)
            .await
            .map_err(|error| error.to_string())?;
        let adapter = match discovered.repository.id.provider() {
            ProviderKind::GitHub => Self::GitHub(
                GitHubAdapter::new(path, config, discovered.repository.id.clone())
                    .map_err(|error| error.to_string())?,
            ),
            ProviderKind::GitLab => Self::GitLab(
                GitLabAdapter::new(config, discovered.repository.id.clone())
                    .map_err(|error| error.to_string())?,
            ),
            ProviderKind::Forgejo => {
                let discovery = config.remote_discovery()?;
                let profile = discovery
                    .profile(discovered.repository.id.host())
                    .cloned()
                    .ok_or_else(|| {
                        "Forgejo host profile disappeared after discovery".to_string()
                    })?;
                Self::Forgejo(Box::new(
                    ForgejoAdapter::new(profile).map_err(|error| error.to_string())?,
                ))
            }
        };
        Ok((adapter, discovered))
    }

    fn capabilities(&self) -> Capabilities {
        match self {
            Self::GitHub(adapter) => adapter.capabilities(),
            Self::GitLab(adapter) => adapter.capabilities(),
            Self::Forgejo(adapter) => adapter.capabilities(),
        }
    }

    fn for_repository(
        path: &'a Path,
        config: &'a Config,
        repository: &RemoteRepositoryId,
    ) -> Result<Self, String> {
        match repository.provider() {
            ProviderKind::GitHub => GitHubAdapter::new(path, config, repository.clone())
                .map(Self::GitHub)
                .map_err(|error| error.to_string()),
            ProviderKind::GitLab => GitLabAdapter::new(config, repository.clone())
                .map(Self::GitLab)
                .map_err(|error| error.to_string()),
            ProviderKind::Forgejo => {
                let profile = config
                    .remote_discovery()?
                    .profile(repository.host())
                    .cloned()
                    .ok_or_else(|| "Forgejo host profile is unavailable".to_string())?;
                ForgejoAdapter::new(profile)
                    .map(|adapter| Self::Forgejo(Box::new(adapter)))
                    .map_err(|error| error.to_string())
            }
        }
    }

    async fn discover_issues(&self) -> Result<Vec<ProviderItemObservation>, RemoteError> {
        match self {
            Self::GitHub(adapter) => adapter.discover_issues("all").await,
            Self::GitLab(_) => Err(RemoteError::new(
                ProviderKind::GitLab,
                RemoteOperation::DiscoverIssues,
                RemoteErrorClass::Unsupported,
                Retryability::NotRetryable,
                "GitLab issue discovery is not implemented",
            )),
            Self::Forgejo(_) => Err(RemoteError::new(
                ProviderKind::Forgejo,
                RemoteOperation::DiscoverIssues,
                RemoteErrorClass::Unsupported,
                Retryability::NotRetryable,
                "Forgejo issue discovery is not implemented",
            )),
        }
    }

    async fn observe_issue(&self, native_id: &str) -> Result<ProviderItemObservation, RemoteError> {
        match self {
            Self::GitHub(adapter) => adapter.observe_issue(native_id).await,
            Self::GitLab(_) => Err(unsupported_issue_operation(
                ProviderKind::GitLab,
                RemoteOperation::DiscoverIssues,
            )),
            Self::Forgejo(_) => Err(unsupported_issue_operation(
                ProviderKind::Forgejo,
                RemoteOperation::DiscoverIssues,
            )),
        }
    }

    async fn set_issue_labels(
        &self,
        native_id: &str,
        labels: &[String],
    ) -> Result<ProviderItemObservation, RemoteError> {
        match self {
            Self::GitHub(adapter) => adapter.set_issue_labels(native_id, labels).await,
            Self::GitLab(_) => Err(unsupported_issue_operation(
                ProviderKind::GitLab,
                RemoteOperation::MutateLabels,
            )),
            Self::Forgejo(_) => Err(unsupported_issue_operation(
                ProviderKind::Forgejo,
                RemoteOperation::MutateLabels,
            )),
        }
    }

    async fn set_issue_assignees(
        &self,
        native_id: &str,
        assignees: &[String],
    ) -> Result<ProviderItemObservation, RemoteError> {
        match self {
            Self::GitHub(adapter) => adapter.set_issue_assignees(native_id, assignees).await,
            Self::GitLab(_) => Err(unsupported_issue_operation(
                ProviderKind::GitLab,
                RemoteOperation::MutateAssignment,
            )),
            Self::Forgejo(_) => Err(unsupported_issue_operation(
                ProviderKind::Forgejo,
                RemoteOperation::MutateAssignment,
            )),
        }
    }

    async fn set_issue_lifecycle(
        &self,
        native_id: &str,
        lifecycle: &str,
    ) -> Result<ProviderItemObservation, RemoteError> {
        match self {
            Self::GitHub(adapter) => adapter.set_issue_lifecycle(native_id, lifecycle).await,
            Self::GitLab(_) => Err(unsupported_issue_operation(
                ProviderKind::GitLab,
                RemoteOperation::MutateIssueLifecycle,
            )),
            Self::Forgejo(_) => Err(unsupported_issue_operation(
                ProviderKind::Forgejo,
                RemoteOperation::MutateIssueLifecycle,
            )),
        }
    }

    async fn issue_has_comment_marker(
        &self,
        native_id: &str,
        marker: &str,
    ) -> Result<bool, RemoteError> {
        match self {
            Self::GitHub(adapter) => adapter.issue_has_comment_marker(native_id, marker).await,
            Self::GitLab(_) => Err(unsupported_issue_operation(
                ProviderKind::GitLab,
                RemoteOperation::CreateIssueComment,
            )),
            Self::Forgejo(_) => Err(unsupported_issue_operation(
                ProviderKind::Forgejo,
                RemoteOperation::CreateIssueComment,
            )),
        }
    }

    async fn create_issue_comment(
        &self,
        native_id: &str,
        body: &str,
        marker: &str,
    ) -> Result<(), RemoteError> {
        match self {
            Self::GitHub(adapter) => adapter.create_issue_comment(native_id, body, marker).await,
            Self::GitLab(_) => Err(unsupported_issue_operation(
                ProviderKind::GitLab,
                RemoteOperation::CreateIssueComment,
            )),
            Self::Forgejo(_) => Err(unsupported_issue_operation(
                ProviderKind::Forgejo,
                RemoteOperation::CreateIssueComment,
            )),
        }
    }

    async fn list_change_requests(
        &self,
        repository: &RemoteRepositoryId,
        head_ref: Option<&str>,
    ) -> Result<Vec<ChangeRequestSummary>, RemoteError> {
        match self {
            Self::GitHub(adapter) => adapter.list_change_requests(head_ref).await,
            Self::GitLab(adapter) => adapter.list_change_requests().await,
            Self::Forgejo(adapter) => {
                let adapter = (**adapter).clone();
                let repository = repository.clone();
                forgejo_call(RemoteOperation::ListChangeRequests, move || {
                    adapter.list_change_requests(&repository)
                })
                .await
            }
        }
    }

    async fn observe_change_request(
        &self,
        id: &ChangeRequestId,
    ) -> Result<ChangeRequestSummary, RemoteError> {
        match self {
            Self::GitHub(adapter) => adapter.observe_change_request(id).await,
            Self::GitLab(adapter) => adapter.observe_change_request(id).await,
            Self::Forgejo(adapter) => {
                let adapter = (**adapter).clone();
                let id = id.clone();
                forgejo_call(RemoteOperation::ObserveChangeRequest, move || {
                    adapter.change_request_summary(&id)
                })
                .await
            }
        }
    }

    async fn lookup_change_request(
        &self,
        id: &ChangeRequestId,
    ) -> Result<Option<ChangeRequestSummary>, RemoteError> {
        match self {
            Self::GitHub(adapter) => adapter.lookup_change_request(id).await,
            Self::GitLab(adapter) => adapter.observe_change_request(id).await.map(Some),
            Self::Forgejo(adapter) => {
                let adapter = (**adapter).clone();
                let id = id.clone();
                forgejo_call(RemoteOperation::ObserveChangeRequest, move || {
                    adapter.change_request_summary(&id).map(Some)
                })
                .await
            }
        }
    }

    async fn change_request_details(
        &self,
        change_request: &ChangeRequest,
    ) -> Result<ChangeRequestDetails, RemoteError> {
        match self {
            Self::GitHub(adapter) => adapter.change_request_details(change_request).await,
            Self::GitLab(adapter) => adapter.change_request_details(change_request).await,
            Self::Forgejo(adapter) => {
                let adapter = (**adapter).clone();
                let change_request = change_request.clone();
                forgejo_call(RemoteOperation::ObserveChangeRequest, move || {
                    adapter.change_request_details(&change_request)
                })
                .await
            }
        }
    }

    async fn repository_policy(
        &self,
        repository: &RemoteRepositoryId,
        target_branch: &str,
    ) -> Result<RepositoryPolicy, RemoteError> {
        match self {
            Self::GitHub(adapter) => adapter.repository_policy(target_branch).await,
            Self::GitLab(adapter) => adapter.repository_policy(target_branch).await,
            Self::Forgejo(adapter) => {
                let adapter = (**adapter).clone();
                let repository = repository.clone();
                let target_branch = target_branch.to_string();
                forgejo_call(RemoteOperation::ObserveRepositoryPolicy, move || {
                    adapter.repository_policy(&repository, &target_branch)
                })
                .await
            }
        }
    }

    async fn create_change_request(
        &self,
        request: &CreateChangeRequest,
    ) -> Result<ChangeRequestSummary, RemoteError> {
        match self {
            Self::GitHub(adapter) => adapter.create_change_request(request).await,
            Self::GitLab(adapter) => adapter.create_change_request(request).await,
            Self::Forgejo(adapter) => {
                let adapter = (**adapter).clone();
                let request = request.clone();
                forgejo_call(RemoteOperation::CreateChangeRequest, move || {
                    adapter.create_change_request(request)
                })
                .await
            }
        }
    }

    async fn merge_change_request(
        &self,
        request: &GuardedMerge,
    ) -> Result<MergeMutationResult, RemoteError> {
        match self {
            Self::GitHub(adapter) => adapter.merge_change_request(request).await,
            Self::GitLab(adapter) => adapter.merge_change_request(request).await,
            Self::Forgejo(adapter) => {
                let adapter = (**adapter).clone();
                let request = request.clone();
                forgejo_call(RemoteOperation::MergeChangeRequest, move || {
                    adapter.merge_change_request(request)
                })
                .await
            }
        }
    }

    async fn resolve_review_thread(
        &self,
        request: &ResolveReviewThread,
    ) -> Result<(), RemoteError> {
        match self {
            Self::GitHub(adapter) => adapter.resolve_review_thread(request).await,
            Self::GitLab(adapter) => adapter.resolve_review_thread(request).await.map(|_| ()),
            Self::Forgejo(adapter) => {
                let adapter = (**adapter).clone();
                let request = request.clone();
                forgejo_call(RemoteOperation::ResolveReviewThread, move || {
                    adapter.resolve_review_thread(request)
                })
                .await
            }
        }
    }

    async fn submit_review(&self, request: &SubmitReview) -> Result<(), RemoteError> {
        match self {
            Self::GitHub(adapter) => adapter.submit_review(request).await,
            Self::GitLab(adapter) => adapter.submit_review(request),
            Self::Forgejo(adapter) => adapter.submit_review(request),
        }
    }
}

async fn forgejo_call<T, F>(operation: RemoteOperation, call: F) -> Result<T, RemoteError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, RemoteError> + Send + 'static,
{
    tokio::task::spawn_blocking(call).await.map_err(|error| {
        RemoteError::new(
            ProviderKind::Forgejo,
            operation,
            RemoteErrorClass::Provider,
            Retryability::Retryable,
            format!("Forgejo HTTP task failed: {error}"),
        )
    })?
}

pub(crate) async fn configured(path: &Path, config: &Config) -> bool {
    Adapter::resolve(path, config).await.is_ok()
}

/// Discovers authoritative open Issues through the provider seam. Unsupported
/// providers return an explicit capability error, never an empty collection.
fn unsupported_issue_operation(provider: ProviderKind, operation: RemoteOperation) -> RemoteError {
    RemoteError::new(
        provider,
        operation,
        RemoteErrorClass::Unsupported,
        Retryability::NotRetryable,
        "provider Issue operation is not implemented",
    )
}

pub(crate) async fn discover_issues(
    path: &Path,
    config: &Config,
) -> Result<Vec<ProviderItemObservation>, RemoteError> {
    let (adapter, _) = Adapter::resolve(path, config).await.map_err(|message| {
        RemoteError::new(
            ProviderKind::GitHub,
            RemoteOperation::DiscoverIssues,
            RemoteErrorClass::Configuration,
            Retryability::NotRetryable,
            message,
        )
    })?;
    adapter.discover_issues().await
}

pub(crate) async fn observe_issue(
    path: &Path,
    config: &Config,
    repository: &RemoteRepositoryId,
    native_id: &str,
) -> Result<ProviderItemObservation, String> {
    Adapter::for_repository(path, config, repository)?
        .observe_issue(native_id)
        .await
        .map_err(|error| error.to_string())
}

pub(crate) async fn set_issue_labels(
    path: &Path,
    config: &Config,
    repository: &RemoteRepositoryId,
    native_id: &str,
    labels: &[String],
) -> Result<ProviderItemObservation, String> {
    Adapter::for_repository(path, config, repository)?
        .set_issue_labels(native_id, labels)
        .await
        .map_err(|error| error.to_string())
}

pub(crate) async fn set_issue_assignees(
    path: &Path,
    config: &Config,
    repository: &RemoteRepositoryId,
    native_id: &str,
    assignees: &[String],
) -> Result<ProviderItemObservation, String> {
    Adapter::for_repository(path, config, repository)?
        .set_issue_assignees(native_id, assignees)
        .await
        .map_err(|error| error.to_string())
}

pub(crate) async fn set_issue_lifecycle(
    path: &Path,
    config: &Config,
    repository: &RemoteRepositoryId,
    native_id: &str,
    lifecycle: &str,
) -> Result<ProviderItemObservation, String> {
    Adapter::for_repository(path, config, repository)?
        .set_issue_lifecycle(native_id, lifecycle)
        .await
        .map_err(|error| error.to_string())
}

pub(crate) async fn issue_has_comment_marker(
    path: &Path,
    config: &Config,
    repository: &RemoteRepositoryId,
    native_id: &str,
    marker: &str,
) -> Result<bool, String> {
    Adapter::for_repository(path, config, repository)?
        .issue_has_comment_marker(native_id, marker)
        .await
        .map_err(|error| error.to_string())
}

pub(crate) async fn create_issue_comment(
    path: &Path,
    config: &Config,
    repository: &RemoteRepositoryId,
    native_id: &str,
    body: &str,
    marker: &str,
) -> Result<(), String> {
    Adapter::for_repository(path, config, repository)?
        .create_issue_comment(native_id, body, marker)
        .await
        .map_err(|error| error.to_string())
}

pub(crate) async fn provider(path: &Path, config: &Config) -> Result<ProviderKind, String> {
    Adapter::resolve(path, config)
        .await
        .map(|(_, remote)| remote.repository.id.provider())
}

pub(crate) async fn repository_project(
    path: &Path,
    config: &Config,
    remote_name: &str,
) -> Result<String, String> {
    discover_git_remote(path, config, remote_name, RemoteUrlKind::Fetch)
        .await
        .map(|remote| remote.repository.id.project_path().to_string())
        .map_err(|error| error.to_string())
}

pub(crate) async fn fetch_change_request_branch(
    path: &Path,
    config: &Config,
    summary: &PrSummary,
    branch: &str,
) -> Result<(), String> {
    if branch.trim().is_empty() || branch == "(detached)" {
        return Err(
            "cannot fetch change request into an empty or detached branch name".to_string(),
        );
    }
    let identity = summary.change_request_identity.as_ref().ok_or_else(|| {
        "change request has no canonical identity; refresh before fetching".to_string()
    })?;
    let source = identity
        .source_repository()
        .map_err(|error| error.to_string())?;
    let target = identity
        .target_repository()
        .map_err(|error| error.to_string())?;
    let request = identity
        .change_request_id(Some(summary.number))
        .map_err(|error| error.to_string())?;
    if request.repository() != &target
        || source.provider() != identity.provider()
        || target.provider() != identity.provider()
    {
        return Err("change request identity has inconsistent repositories".to_string());
    }
    if summary.head_sha.trim().is_empty() {
        return Err("change request has no observed head SHA".to_string());
    }

    let destination_ref = format!("refs/heads/{branch}");
    validate_git_ref(path, config, &destination_ref).await?;
    let destination_old_oid = read_git_ref_or_zero(path, config, &destination_ref).await?;
    if destination_old_oid != *"0000000000000000000000000000000000000000" {
        return Ok(());
    }
    let mut configured = Vec::new();
    for remote_name in ["origin", "upstream"] {
        if let Ok(remote) =
            discover_git_remote(path, config, remote_name, RemoteUrlKind::Fetch).await
        {
            configured.push((remote_name, remote.repository.id));
        }
    }
    let fetch = select_fetch_source(
        identity.provider(),
        summary.number,
        &summary.head_ref,
        &source,
        &target,
        &configured,
    )?;
    validate_git_ref(path, config, &fetch.remote_ref).await?;

    let temporary_ref = format!(
        "refs/prism/change-requests/{:016x}",
        identity.stable_hash()
            ^ crate::util::stable_hash(Path::new(&summary.head_sha))
            ^ crate::util::stable_hash(Path::new(branch))
    );
    let refspec = format!("+{}:{temporary_ref}", fetch.remote_ref);
    let fetch_result = crate::process::run_status_named(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(path)
            .args(["fetch", fetch.remote_name])
            .arg(refspec),
        crate::process::ProcessPolicy::NetworkQuery,
        crate::process::ProcessDescriptor::new("git.fetch"),
    )
    .await;

    let publish = async {
        fetch_result?;
        let fetched_sha = crate::process::run_capture_named(
            Command::new(config.tool("git"))
                .arg("-C")
                .arg(path)
                .args(["rev-parse", "--verify"])
                .arg(format!("{temporary_ref}^{{commit}}")),
            crate::process::ProcessPolicy::Metadata,
            crate::process::ProcessDescriptor::new("git.rev_parse"),
        )
        .await?;
        if fetched_sha.trim() != summary.head_sha {
            return Err("change request head changed while it was being fetched".to_string());
        }
        crate::process::run_status_named(
            Command::new(config.tool("git")).arg("-C").arg(path).args([
                "update-ref",
                &destination_ref,
                &summary.head_sha,
                &destination_old_oid,
            ]),
            crate::process::ProcessPolicy::LocalMutation,
            crate::process::ProcessDescriptor::new("git.update_ref"),
        )
        .await
    }
    .await;
    let cleanup = crate::process::with_cancellation(
        crate::process::CancellationToken::new(),
        crate::process::run_status_named(
            Command::new(config.tool("git")).arg("-C").arg(path).args([
                "update-ref",
                "-d",
                &temporary_ref,
            ]),
            crate::process::ProcessPolicy::LocalMutation,
            crate::process::ProcessDescriptor::new("git.update_ref"),
        ),
    )
    .await;
    publish.and(cleanup)
}

async fn read_git_ref_or_zero(
    path: &Path,
    config: &Config,
    reference: &str,
) -> Result<String, String> {
    let output = crate::process::run_output_allow_failure_named(
        Command::new(config.tool("git")).arg("-C").arg(path).args([
            "rev-parse",
            "--verify",
            reference,
        ]),
        crate::process::ProcessPolicy::Metadata,
        crate::process::ProcessDescriptor::new("git.rev_parse"),
    )
    .await?;
    if !output.status.success() {
        return Ok("0000000000000000000000000000000000000000".to_string());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let oid = stdout.trim();
    if oid.is_empty() {
        return Err(format!("git returned an empty object ID for {reference}"));
    }
    Ok(oid.to_string())
}

struct FetchSource<'a> {
    remote_name: &'a str,
    remote_ref: String,
}

fn select_fetch_source<'a>(
    provider: ProviderKind,
    display_number: u64,
    source_branch: &str,
    source: &RemoteRepositoryId,
    target: &RemoteRepositoryId,
    configured: &'a [(&'a str, RemoteRepositoryId)],
) -> Result<FetchSource<'a>, String> {
    if provider != ProviderKind::Forgejo
        && let Some((remote_name, _)) = configured
            .iter()
            .find(|(_, repository)| repository == target)
    {
        let remote_ref = match provider {
            ProviderKind::GitHub => format!("refs/pull/{display_number}/head"),
            ProviderKind::GitLab => format!("refs/merge-requests/{display_number}/head"),
            ProviderKind::Forgejo => unreachable!(),
        };
        return Ok(FetchSource {
            remote_name,
            remote_ref,
        });
    }
    if let Some((remote_name, _)) = configured
        .iter()
        .find(|(_, repository)| repository == source)
    {
        return Ok(FetchSource {
            remote_name,
            remote_ref: format!("refs/heads/{source_branch}"),
        });
    }
    if provider == ProviderKind::Forgejo
        && let Some((remote_name, _)) = configured
            .iter()
            .find(|(_, repository)| repository == target)
    {
        return Ok(FetchSource {
            remote_name,
            remote_ref: format!("refs/pull/{display_number}/head"),
        });
    }
    Err(
        "no configured fetch remote matches the change request source or target repository"
            .to_string(),
    )
}

async fn validate_git_ref(path: &Path, config: &Config, reference: &str) -> Result<(), String> {
    crate::process::run_status_named(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(path)
            .args(["check-ref-format", reference]),
        crate::process::ProcessPolicy::Metadata,
        crate::process::ProcessDescriptor::new("git.check_ref_format"),
    )
    .await
}

pub(crate) async fn submit_review(
    path: &Path,
    config: &Config,
    summary: &PrSummary,
    kind: ReviewSubmissionKind,
    body: String,
) -> Result<(), String> {
    let change_request = change_request_from_legacy(summary)?;
    let target = &change_request.target_repository;
    if change_request.id.repository() != target {
        return Err("change request identity has an inconsistent target repository".to_string());
    }
    configured_remote_repositories(path, config)
        .await?
        .validate_target_repository(target)
        .map_err(|_| "change request target changed before review submission".to_string())?;
    let adapter = Adapter::for_repository(path, config, target)?;
    adapter
        .submit_review(&SubmitReview {
            id: change_request.id,
            expected_head_sha: change_request.head_sha,
            kind,
            body,
        })
        .await
        .map_err(|error| error.to_string())
}

pub(crate) async fn capabilities(path: &Path, config: &Config) -> Result<Capabilities, String> {
    let (adapter, _) = Adapter::resolve(path, config).await?;
    match adapter {
        Adapter::Forgejo(adapter) => {
            let adapter = (*adapter).clone();
            forgejo_call(RemoteOperation::DiscoverRepository, move || {
                adapter.discover_instance()?;
                Ok(adapter.capabilities())
            })
            .await
            .map_err(|error| error.to_string())
        }
        adapter => Ok(adapter.capabilities()),
    }
}

pub(crate) async fn authentication_status(path: &Path, config: &Config) -> Result<String, String> {
    let (adapter, remote) = Adapter::resolve(path, config).await?;
    match adapter {
        Adapter::GitHub(_) => crate::process::run_capture_named(
            Command::new(config.tool("gh"))
                .arg("auth")
                .arg("status")
                .arg("--hostname")
                .arg(remote.repository.id.host().to_string()),
            crate::process::ProcessPolicy::NetworkQuery,
            crate::process::ProcessDescriptor::new("gh.auth.status"),
        )
        .await
        .map(|_| "ok".to_string()),
        Adapter::GitLab(_) => crate::process::run_capture_named(
            Command::new(config.tool("glab"))
                .arg("auth")
                .arg("status")
                .arg("--hostname")
                .arg(remote.repository.id.host().to_string()),
            crate::process::ProcessPolicy::NetworkQuery,
            crate::process::ProcessDescriptor::new("glab.auth.status"),
        )
        .await
        .map(|_| "ok".to_string()),
        Adapter::Forgejo(_) => {
            let profile = config
                .remote_discovery()?
                .profile(remote.repository.id.host())
                .cloned()
                .ok_or_else(|| "Forgejo host profile is unavailable".to_string())?;
            Ok(profile
                .credential_environment
                .map(|name| {
                    if std::env::var_os(&name).is_some() {
                        format!("available from {name}")
                    } else {
                        format!("missing environment variable {name}")
                    }
                })
                .unwrap_or_else(|| "not configured".to_string()))
        }
    }
}

pub(crate) async fn server_version(path: &Path, config: &Config) -> Result<Option<String>, String> {
    let (adapter, _) = Adapter::resolve(path, config).await?;
    match adapter {
        Adapter::Forgejo(adapter) => {
            let adapter = (*adapter).clone();
            forgejo_call(RemoteOperation::DiscoverRepository, move || {
                adapter
                    .discover_instance()
                    .map(|instance| Some(instance.version))
            })
            .await
            .map_err(|error| error.to_string())
        }
        Adapter::GitHub(_) | Adapter::GitLab(_) => Ok(None),
    }
}

pub(crate) struct RemoteRuntimeDiagnostics {
    pub(crate) capabilities: Capabilities,
    pub(crate) server_version: Option<String>,
}

pub(crate) async fn runtime_diagnostics(
    path: &Path,
    config: &Config,
) -> Result<RemoteRuntimeDiagnostics, String> {
    let (adapter, remote) = Adapter::resolve(path, config).await?;
    match adapter {
        Adapter::Forgejo(adapter) => {
            let adapter = (*adapter).clone();
            let repository = remote.repository.id.clone();
            let diagnostics = forgejo_call(RemoteOperation::DiscoverRepository, move || {
                adapter.runtime_diagnostics(&repository)
            })
            .await
            .map_err(|error| error.to_string())?;
            Ok(RemoteRuntimeDiagnostics {
                capabilities: diagnostics.capabilities,
                server_version: Some(diagnostics.instance.version),
            })
        }
        adapter => Ok(RemoteRuntimeDiagnostics {
            capabilities: adapter.capabilities(),
            server_version: None,
        }),
    }
}

pub(crate) fn capabilities_for_summary(summary: &PrSummary) -> Capabilities {
    summary
        .change_request_identity
        .as_ref()
        .map(|identity| Capabilities::for_provider(identity.provider()))
        .unwrap_or_default()
}

pub(crate) async fn list_change_requests(
    path: &Path,
    config: &Config,
) -> Result<Vec<PrSummary>, String> {
    list_change_requests_for_head(path, config, None).await
}

async fn list_change_requests_for_head(
    path: &Path,
    config: &Config,
    head_ref: Option<&str>,
) -> Result<Vec<PrSummary>, String> {
    let repositories = configured_change_request_repositories(path, config).await?;
    let mut summaries = Vec::new();
    for repository in repositories {
        let adapter = Adapter::for_repository(path, config, &repository)?;
        let observed = adapter
            .list_change_requests(&repository, head_ref)
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(to_legacy_summary)
            .collect::<Result<Vec<_>, _>>()?;
        for summary in observed {
            if !summaries.iter().any(|existing: &PrSummary| {
                existing.change_request_identity == summary.change_request_identity
                    && existing.number == summary.number
            }) {
                summaries.push(summary);
            }
        }
    }
    Ok(summaries)
}

async fn configured_change_request_repositories(
    path: &Path,
    config: &Config,
) -> Result<Vec<RemoteRepositoryId>, String> {
    Ok(configured_remote_repositories(path, config)
        .await?
        .fetch_repositories)
}

struct ConfiguredRemoteRepositories {
    origin_fetch: RemoteRepositoryId,
    origin_push: RemoteRepositoryId,
    upstream_fetch: Option<RemoteRepositoryId>,
    upstream_push: Option<RemoteRepositoryId>,
    fetch_repositories: Vec<RemoteRepositoryId>,
}

impl ConfiguredRemoteRepositories {
    fn create_target(&self, project: Option<&str>) -> Result<RemoteRepositoryId, String> {
        let Some(project) = project.map(str::trim).filter(|project| !project.is_empty()) else {
            return Ok(self.origin_fetch.clone());
        };
        let mut matches = self
            .fetch_repositories
            .iter()
            .filter(|repository| repository.project_path_eq(project));
        let target = matches.next().ok_or_else(|| {
            "change request target is not a configured fetch repository".to_string()
        })?;
        if matches.next().is_some() {
            return Err("change request target matches multiple configured hosts".to_string());
        }
        Ok(target.clone())
    }

    fn validate_target_repository(&self, target: &RemoteRepositoryId) -> Result<(), String> {
        if !self.fetch_repositories.contains(target) {
            return Err(
                "change request target repository is no longer configured for fetch".to_string(),
            );
        }
        Ok(())
    }

    fn validate_source_mutation(
        &self,
        source: &RemoteRepositoryId,
        target: &RemoteRepositoryId,
    ) -> Result<(), String> {
        self.validate_target_repository(target)?;
        if source != &self.origin_push {
            return Err(
                "change request source repository no longer matches origin push URL".to_string(),
            );
        }
        Ok(())
    }

    fn fetch_remote_name(&self, repository: &RemoteRepositoryId) -> Result<&'static str, String> {
        if &self.origin_fetch == repository {
            return Ok("origin");
        }
        if self.upstream_fetch.as_ref() == Some(repository) {
            return Ok("upstream");
        }
        Err("target repository is no longer configured for fetch".to_string())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CreateChangeRequestGuard {
    pub(crate) source_push: PushGuard,
    pub(crate) source_repository: RemoteRepositoryId,
    pub(crate) target_repository: RemoteRepositoryId,
    pub(crate) local_branch: String,
    pub(crate) source_branch: String,
    pub(crate) target_branch: String,
    pub(crate) expected_head_sha: String,
    pub(crate) expected_base_sha: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub(crate) struct PushGuard {
    pub(crate) repository: RemoteRepositoryId,
    pub(crate) remote: String,
    pub(crate) remote_branch: String,
    pub(crate) local_branch: String,
    pub(crate) expected_head_sha: String,
    pub(crate) set_upstream: bool,
}

pub(crate) fn same_push_target(left: &PushGuard, right: &PushGuard) -> bool {
    left.repository == right.repository
        && left.remote == right.remote
        && left.remote_branch == right.remote_branch
        && left.local_branch == right.local_branch
        && left.expected_head_sha == right.expected_head_sha
}

fn validate_create_change_request_guard(
    expected: &CreateChangeRequestGuard,
    fresh: &CreateChangeRequestGuard,
) -> Result<(), String> {
    if fresh != expected {
        return Err("change request source, target, or base changed before creation".to_string());
    }
    Ok(())
}

async fn configured_remote_repositories(
    path: &Path,
    config: &Config,
) -> Result<ConfiguredRemoteRepositories, String> {
    let origin_fetch = discover_git_remote(path, config, "origin", RemoteUrlKind::Fetch)
        .await
        .map_err(|error| error.to_string())?
        .repository
        .id;
    let origin_push = discover_git_remote(path, config, "origin", RemoteUrlKind::Push)
        .await
        .map_err(|error| error.to_string())?
        .repository
        .id;
    if origin_push.provider() != origin_fetch.provider() {
        return Err("origin fetch and push repositories use different providers".to_string());
    }

    let upstream_fetch = discover_git_remote(path, config, "upstream", RemoteUrlKind::Fetch)
        .await
        .ok()
        .map(|remote| remote.repository.id)
        .filter(|repository| repository.provider() == origin_fetch.provider());
    let upstream_push = discover_git_remote(path, config, "upstream", RemoteUrlKind::Push)
        .await
        .ok()
        .map(|remote| remote.repository.id)
        .filter(|repository| repository.provider() == origin_fetch.provider());
    let mut fetch_repositories = vec![origin_fetch.clone()];
    if let Some(repository) = upstream_fetch.clone() {
        push_unique_repository(&mut fetch_repositories, repository);
    }
    Ok(ConfiguredRemoteRepositories {
        origin_fetch,
        origin_push,
        upstream_fetch,
        upstream_push,
        fetch_repositories,
    })
}

pub(crate) async fn create_change_request_targets(
    path: &Path,
    config: &Config,
) -> Result<(RemoteRepositoryId, Option<RemoteRepositoryId>), String> {
    let remotes = configured_remote_repositories(path, config).await?;
    let upstream = remotes
        .upstream_fetch
        .filter(|repository| repository != &remotes.origin_fetch);
    Ok((remotes.origin_fetch, upstream))
}

pub(crate) async fn fetch_remote_name_for_repository(
    path: &Path,
    config: &Config,
    repository: &RemoteRepositoryId,
) -> Result<String, String> {
    configured_remote_repositories(path, config)
        .await?
        .fetch_remote_name(repository)
        .map(str::to_string)
}

pub(crate) async fn fetch_repository_branch_head_sha(
    path: &Path,
    config: &Config,
    repository: &RemoteRepositoryId,
    branch: &str,
) -> Result<Option<String>, String> {
    let remote = fetch_remote_name_for_repository(path, config, repository).await?;
    crate::git::fetch_remote_branch(path, &remote, branch, config).await?;
    crate::git::remote_branch_head_sha_on(path, &remote, branch, config).await
}

pub(crate) async fn fetch_change_request_base_head_sha(
    path: &Path,
    config: &Config,
    summary: &PrSummary,
) -> Result<Option<String>, String> {
    let identity = summary
        .change_request_identity
        .as_ref()
        .ok_or_else(|| "change request has no canonical identity".to_string())?;
    let target = identity
        .target_repository()
        .map_err(|error| error.to_string())?;
    fetch_repository_branch_head_sha(path, config, &target, &summary.base_ref).await
}

pub(crate) async fn prepare_create_change_request(
    path: &Path,
    config: &Config,
    branch: &str,
    target_repository: &RemoteRepositoryId,
    source_push: &PushGuard,
) -> Result<CreateChangeRequestGuard, String> {
    let current_branch = crate::git::current_branch_name(path, config)
        .await?
        .ok_or_else(|| "cannot create a change request from detached HEAD".to_string())?;
    if current_branch != branch {
        return Err(format!(
            "selected branch changed from {branch} to {current_branch} before change request creation"
        ));
    }

    let fresh_push = prepare_push(path, config, branch).await?;
    if &fresh_push != source_push {
        return Err("change request push source changed before creation".to_string());
    }
    let remotes = configured_remote_repositories(path, config).await?;
    remotes.validate_target_repository(target_repository)?;
    if source_push.repository.provider() != target_repository.provider() {
        return Err("change request source and target use different providers".to_string());
    }
    let target_remote = remotes.fetch_remote_name(target_repository)?;
    let target_branch = config
        .default_base
        .as_deref()
        .map(str::trim)
        .filter(|branch| !branch.is_empty())
        .unwrap_or("main")
        .to_string();
    crate::git::fetch_remote_branch(path, target_remote, &target_branch, config).await?;
    let expected_base_sha =
        crate::git::remote_branch_head_sha_on(path, target_remote, &target_branch, config)
            .await?
            .ok_or_else(|| {
                format!(
                    "change request target branch {target_remote}/{target_branch} does not exist"
                )
            })?;

    let expected_head_sha = crate::git::current_head_sha(path, config).await?;
    let source_head_sha = crate::git::push_remote_branch_head_sha(
        path,
        &source_push.remote,
        &source_push.remote_branch,
        config,
    )
    .await?
    .ok_or_else(|| "change request source branch does not exist on the push remote".to_string())?;
    if source_head_sha != expected_head_sha {
        return Err("change request source branch does not match the expected HEAD".to_string());
    }

    Ok(CreateChangeRequestGuard {
        source_push: source_push.clone(),
        source_repository: source_push.repository.clone(),
        target_repository: target_repository.clone(),
        local_branch: branch.to_string(),
        source_branch: source_push.remote_branch.clone(),
        target_branch,
        expected_head_sha,
        expected_base_sha,
    })
}

pub(crate) async fn prepare_push(
    path: &Path,
    config: &Config,
    selected_branch: &str,
) -> Result<PushGuard, String> {
    let local_branch = crate::git::current_branch_name(path, config)
        .await?
        .ok_or_else(|| "cannot push detached HEAD".to_string())?;
    if local_branch != selected_branch {
        return Err(format!(
            "selected branch changed from {selected_branch} to {local_branch} before push"
        ));
    }
    let push_destination = crate::process::run_capture(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(path)
            .arg("for-each-ref")
            .arg("--format=%(push:remotename)%00%(push)")
            .arg(format!("refs/heads/{selected_branch}")),
        crate::process::ProcessPolicy::Metadata,
    )
    .await?;
    let (remote, remote_branch, set_upstream) = match push_destination.trim().split_once('\0') {
        Some((remote, push_ref)) if !remote.is_empty() && !push_ref.is_empty() => {
            let prefix = format!("refs/remotes/{remote}/");
            (
                remote.to_string(),
                push_ref
                    .strip_prefix(&prefix)
                    .ok_or_else(|| "push destination is not a remote-tracking branch".to_string())?
                    .to_string(),
                false,
            )
        }
        _ => ("origin".to_string(), selected_branch.to_string(), true),
    };
    crate::git::single_push_remote_url(path, &remote, config).await?;
    let repository = discover_git_remote(path, config, &remote, RemoteUrlKind::Push)
        .await
        .map_err(|error| error.to_string())?
        .repository
        .id;
    Ok(PushGuard {
        repository,
        remote: remote.to_string(),
        remote_branch,
        local_branch,
        expected_head_sha: crate::git::current_head_sha(path, config).await?,
        set_upstream,
    })
}

fn push_unique_repository(
    repositories: &mut Vec<RemoteRepositoryId>,
    repository: RemoteRepositoryId,
) {
    if !repositories.contains(&repository) {
        repositories.push(repository);
    }
}

pub(crate) async fn refresh_change_request_cache(
    repo: &Repository,
    branch: &str,
    cache: &mut PrCache,
    path: &Path,
    config: &Config,
    force_details: bool,
) -> Result<(), String> {
    let remotes = configured_remote_repositories(path, config).await?;
    let observation = if cache.summary_observed_in_process
        && let Some(summary) = cache.summary()
        && let Ok(change_request) = change_request_from_legacy(summary)
    {
        if let Err(error) = remotes.validate_target_repository(&change_request.target_repository) {
            Err(error)
        } else {
            let target_adapter =
                Adapter::for_repository(path, config, change_request.id.repository())?;
            target_adapter
                .lookup_change_request(&change_request.id)
                .await
                .map_err(|error| error.to_string())
                .and_then(|summary| summary.map(to_legacy_summary).transpose())
                .and_then(|summary| match summary {
                    Some(summary) => authoritative_active_summary(summary),
                    None => Ok(None),
                })
        }
    } else {
        let source_push = prepare_push(path, config, branch).await?;
        list_change_requests_for_head(path, config, Some(&source_push.remote_branch))
            .await
            .map(|summaries| {
                let matching = summaries.into_iter().filter(|summary| {
                    summary.head_ref == source_push.remote_branch
                        && summary.head_sha == source_push.expected_head_sha
                        && summary
                            .change_request_identity
                            .as_ref()
                            .is_some_and(|identity| {
                                identity.source_repository().ok().as_ref()
                                    == Some(&source_push.repository)
                                    && identity.target_repository().ok().is_some_and(|target| {
                                        remotes.validate_target_repository(&target).is_ok()
                                    })
                            })
                });
                let mut unknown_lifecycle = None;
                for summary in matching {
                    if summary.state.eq_ignore_ascii_case("OPEN") && !summary.merged {
                        return Some(summary);
                    }
                    if !known_legacy_lifecycle(&summary) {
                        unknown_lifecycle = Some(summary);
                    }
                }
                unknown_lifecycle
            })
    };
    super::store::record_provider_summary_refresh(repo, branch, cache, observation)?;
    if force_details && cache.summary().is_some() {
        refresh_change_request_details_state(branch, cache, path, config).await;
        super::store::persist_pr_cache_snapshot(repo, branch, cache)?;
    }
    Ok(())
}

pub(crate) async fn refresh_change_request_details_state(
    _branch: &str,
    cache: &mut PrCache,
    path: &Path,
    config: &Config,
) {
    let result = async {
        let summary = cache
            .summary()
            .cloned()
            .ok_or_else(|| "change request summary is not loaded".to_string())?;
        let change_request = change_request_from_legacy(&summary)?;
        configured_remote_repositories(path, config)
            .await?
            .validate_target_repository(&change_request.target_repository)?;
        let adapter = Adapter::for_repository(path, config, change_request.id.repository())?;
        let details = adapter
            .change_request_details(&change_request)
            .await
            .map_err(|error| error.to_string())?;
        if !details.association.as_ref().is_some_and(|association| {
            association.matches(&change_request.id, &change_request.head_sha)
        }) {
            return Err("change request head changed while details were loaded".to_string());
        }
        let details = to_legacy_details(details);
        Ok(details)
    }
    .await;
    match result {
        Ok(details) => cache::record_provider_details_refresh(cache, Ok(details)),
        Err(error) => cache::record_provider_details_refresh(cache, Err(error)),
    }
}

pub(crate) async fn refresh_repository_policy(
    repo: &Repository,
    path: &Path,
    config: &Config,
) -> Result<RepoPolicyCache, String> {
    refresh_repository_policy_for(repo, path, config, None).await
}

pub(crate) async fn refresh_repository_policy_for(
    repo: &Repository,
    path: &Path,
    config: &Config,
    target_repository: Option<&RemoteRepositoryId>,
) -> Result<RepoPolicyCache, String> {
    let (origin_adapter, remote) = Adapter::resolve(path, config).await?;
    let repository = target_repository
        .cloned()
        .unwrap_or_else(|| remote.repository.id.clone());
    let adapter = if target_repository.is_some() {
        Adapter::for_repository(path, config, &repository)?
    } else {
        origin_adapter
    };
    let observed_target = observed_policy_target_branch(repo, path, config, &repository).await;
    let target = observed_target
        .as_deref()
        .or(config.default_base.as_deref())
        .unwrap_or("main");
    let policy = adapter.repository_policy(&repository, target);
    let mut cache = RepoPolicyCache {
        repo_remote: repository.project_path().to_string(),
        provider: Some(repository.provider()),
        canonical_host: Some(repository.host().to_string()),
        project_path: Some(repository.project_path().to_string()),
        target_branch: Some(target.to_string()),
        identity_complete: true,
        default_branch: Some(target.to_string()),
        refreshed_unix_ms: unix_seconds(),
        ..RepoPolicyCache::default()
    };
    match policy.await {
        Ok(policy) => {
            if policy
                .repository
                .as_ref()
                .is_some_and(|observed| observed != &repository)
                || policy.target_branch != target
            {
                return Err(
                    "provider returned policy for a different repository or branch".to_string(),
                );
            }
            let mut errors = Vec::new();
            cache.required_checks =
                policy_fact(policy.facts.required_checks, "required checks", &mut errors);
            cache.required_approvals = u64::from(policy_fact(
                policy.facts.required_approvals,
                "required approvals",
                &mut errors,
            ));
            cache.require_conversation_resolution = policy_fact(
                policy.facts.conversations_must_be_resolved,
                "conversation policy",
                &mut errors,
            );
            cache.require_branch_up_to_date = policy_fact(
                policy.facts.source_must_be_up_to_date,
                "up-to-date policy",
                &mut errors,
            );
            cache.merge_queue_required =
                policy_fact(policy.facts.queue_required, "queue policy", &mut errors);
            cache.error = (!errors.is_empty()).then(|| errors.join("; "));
        }
        Err(error) => {
            if let Some(mut stale) =
                super::store::load_repo_policy_cache_for_identity(repo, &repository, target)
            {
                stale.error = Some(error.to_string());
                cache = stale;
            } else {
                cache.error = Some(error.to_string());
            }
        }
    }
    super::store::save_repo_policy_cache(repo, &cache)?;
    Ok(cache)
}

async fn observed_policy_target_branch(
    repo: &Repository,
    path: &Path,
    config: &Config,
    repository: &RemoteRepositoryId,
) -> Option<String> {
    let branch = crate::git::current_branch_name(path, config)
        .await
        .ok()
        .flatten()?;
    let cache = super::store::load_pr_cache(repo, &branch);
    let summary = cache.summary()?;
    let identity = summary.change_request_identity.as_ref()?;
    (identity.target_repository().ok().as_ref() == Some(repository)
        && !summary.base_ref.trim().is_empty())
    .then(|| summary.base_ref.clone())
}

fn policy_fact<T: Default>(
    observation: Observation<T>,
    label: &str,
    errors: &mut Vec<String>,
) -> T {
    match known(observation, label) {
        Ok(value) => value,
        Err(error) => {
            errors.push(error);
            T::default()
        }
    }
}

pub(crate) async fn create_change_request(
    repo: &Repository,
    config: &Config,
    path: &Path,
    body: &str,
    guard: &CreateChangeRequestGuard,
    cache: &mut PrCache,
) -> Result<(), String> {
    let fresh = prepare_create_change_request(
        path,
        config,
        &guard.local_branch,
        &guard.target_repository,
        &guard.source_push,
    )
    .await?;
    validate_create_change_request_guard(guard, &fresh)?;
    let source = guard.source_repository.clone();
    let target = guard.target_repository.clone();
    let adapter = Adapter::for_repository(path, config, &target)?;
    let request = CreateChangeRequest {
        source_repository: source,
        target_repository: target.clone(),
        source_branch: guard.source_branch.clone(),
        target_branch: guard.target_branch.clone(),
        expected_head_sha: guard.expected_head_sha.clone(),
        title: guard.local_branch.replace(['-', '_'], " "),
        body: body.to_string(),
        draft: false,
    };
    let summary = adapter
        .create_change_request(&request)
        .await
        .map_err(|error| error.to_string())?;
    super::store::record_pr_summary(
        repo,
        &guard.local_branch,
        cache,
        to_legacy_summary(summary)?,
    );
    refresh_change_request_cache(repo, &guard.local_branch, cache, path, config, true).await
}

pub(crate) async fn merge_change_request(
    config: &Config,
    path: &Path,
    authorized_identity: &CanonicalChangeRequestIdentity,
    display_number: u64,
    expected_head_sha: &str,
) -> Result<MergeMutationResult, String> {
    let remotes = configured_remote_repositories(path, config).await?;
    let authorized_id = authorized_identity
        .change_request_id(Some(display_number))
        .map_err(|error| error.to_string())?;
    let authorized_target = authorized_identity
        .target_repository()
        .map_err(|error| error.to_string())?;
    if authorized_id.repository() != &authorized_target {
        return Err("change request repository changed since authorization".to_string());
    }
    remotes
        .validate_target_repository(&authorized_target)
        .map_err(|_| "change request repository changed since authorization".to_string())?;
    let adapter = Adapter::for_repository(path, config, &authorized_target)?;
    let capabilities = adapter.capabilities();
    if capabilities.guarded_merge == super::SupportLevel::Unsupported {
        return Err(capabilities.guarded_merge_reason.unwrap_or_else(|| {
            "guarded merge is unsupported by the provider adapter".to_string()
        }));
    }
    let summary = adapter
        .observe_change_request(&authorized_id)
        .await
        .map_err(|error| error.to_string())?;
    if summary.change_request.id != authorized_id {
        return Err("provider returned a different change request identity".to_string());
    }
    if summary.change_request.head_sha != expected_head_sha {
        return Err("change request head changed since authorization".to_string());
    }
    let request = GuardedMerge {
        id: summary.change_request.id.clone(),
        target_repository: summary.change_request.target_repository.clone(),
        target_branch: summary.change_request.target_branch.clone(),
        expected_source_sha: expected_head_sha.to_string(),
        method: match config.merge_method {
            crate::config::MergeMethod::Merge => MergeMethod::Merge,
            crate::config::MergeMethod::Squash => MergeMethod::Squash,
            crate::config::MergeMethod::Rebase => MergeMethod::Rebase,
        },
        native_guard: None,
    };
    request
        .validate_observation(&summary)
        .map_err(|error| error.to_string())?;
    adapter
        .merge_change_request(&request)
        .await
        .map_err(|error| error.to_string())
}

pub(crate) async fn resolve_review_thread(
    path: &Path,
    config: &Config,
    summary: &PrSummary,
    thread_id: &str,
) -> Result<(), String> {
    let change_request = change_request_from_legacy(summary)?;
    configured_remote_repositories(path, config)
        .await?
        .validate_target_repository(&change_request.target_repository)
        .map_err(|_| "change request repository changed before thread resolution".to_string())?;
    let adapter = Adapter::for_repository(path, config, change_request.id.repository())?;
    let request = ResolveReviewThread {
        id: change_request.id,
        thread_id: NativeReviewThreadId::new(thread_id.to_string())
            .map_err(|error| error.to_string())?,
        expected_head_sha: summary.head_sha.clone(),
    };
    adapter
        .resolve_review_thread(&request)
        .await
        .map_err(|error| error.to_string())
}

pub(crate) async fn wait_for_change_request_merged(
    path: &Path,
    expected: &ChangeRequest,
    config: &Config,
) -> Result<ChangeRequestSummary, String> {
    let mut last_summary = None;
    let mut last_error = None;
    for attempt in 0..MERGE_VERIFY_ATTEMPTS {
        match observe_exact_change_request(path, expected, config).await {
            Ok(summary) if summary.lifecycle == LifecycleState::Merged => return Ok(summary),
            Ok(summary) => {
                last_summary = Some(summary);
                last_error = None;
            }
            Err(error) => last_error = Some(error),
        }
        if attempt + 1 < MERGE_VERIFY_ATTEMPTS {
            tokio::time::sleep(MERGE_VERIFY_INTERVAL).await;
        }
    }
    last_summary.ok_or_else(|| {
        last_error
            .unwrap_or_else(|| "change request could not be reobserved after merge".to_string())
    })
}

async fn observe_exact_change_request(
    path: &Path,
    expected: &ChangeRequest,
    config: &Config,
) -> Result<ChangeRequestSummary, String> {
    let adapter = Adapter::for_repository(path, config, expected.id.repository())?;
    let observed = adapter
        .observe_change_request(&expected.id)
        .await
        .map_err(|error| error.to_string())?;
    let request = &observed.change_request;
    if request.id != expected.id
        || request.source_repository != expected.source_repository
        || request.target_repository != expected.target_repository
        || request.source_branch != expected.source_branch
        || request.target_branch != expected.target_branch
    {
        return Err(
            "change request identity or target changed during merge verification".to_string(),
        );
    }
    if request.head_sha != expected.head_sha {
        return Err("change request head changed during merge verification".to_string());
    }
    Ok(observed)
}

/// Resolve an opaque workflow Change Request reference through the repository's configured
/// provider adapter and return one current, exact-head Gate observation. Extensions receive only
/// the normalized value; provider credentials and adapter identities remain inside Prism.
pub(crate) async fn observe_workflow_change_request(
    path: &Path,
    config: &Config,
    subject_id: &str,
    expected_head: &str,
    operation: &str,
) -> Result<serde_json::Value, String> {
    let (adapter, discovered) = Adapter::resolve(path, config).await?;
    let marker = ":change_request:";
    let (repository_key, native_id) = subject_id
        .rsplit_once(marker)
        .ok_or_else(|| "opaque subject is not a Change Request identity".to_string())?;
    let expected_repository_key = format!(
        "{}:{}:{}",
        discovered.repository.id.provider().config_label(),
        discovered.repository.id.host(),
        discovered.repository.id.project_path()
    );
    if repository_key != expected_repository_key {
        return Err("opaque Change Request belongs to a different repository".into());
    }
    let native_id = super::NativeChangeRequestId::new(native_id.to_string())
        .map_err(|error| error.to_string())?;
    let summary = adapter
        .list_change_requests(&discovered.repository.id, None)
        .await
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|summary| summary.change_request.id.native_id() == &native_id)
        .ok_or_else(|| "opaque Change Request is not open in the target repository".to_string())?;
    if summary.change_request.head_sha != expected_head {
        return Err("Change Request head changed before workflow observation".into());
    }

    let details = if matches!(operation, "review" | "policy") {
        Some(
            adapter
                .change_request_details(&summary.change_request)
                .await
                .map_err(|error| error.to_string())?,
        )
    } else {
        None
    };
    let policy = if operation == "policy" {
        Some(
            adapter
                .repository_policy(
                    &summary.change_request.target_repository,
                    &summary.change_request.target_branch,
                )
                .await
                .map_err(|error| error.to_string())?,
        )
    } else {
        None
    };

    let satisfied = match operation {
        "ci" => matches!(
            summary.check_state,
            CheckState::Passed | CheckState::Skipped
        ),
        "review" => {
            let no_rejection = !matches!(summary.review_decision, ReviewDecision::ChangesRequested);
            let all_threads_resolved = details
                .as_ref()
                .map(|details| match &details.review_threads {
                    Observation::Known(threads) => threads.iter().all(|thread| thread.resolved),
                    Observation::EmptyKnown | Observation::AuthoritativelyAbsent => true,
                    _ => false,
                })
                .unwrap_or(false);
            no_rejection && all_threads_resolved
        }
        "policy" => {
            let policy = policy
                .as_ref()
                .ok_or_else(|| "repository policy was not observed".to_string())?;
            let details = details
                .as_ref()
                .ok_or_else(|| "Change Request details were not observed".to_string())?;
            policy_satisfied(policy, details, &summary)
        }
        "mergeability" => matches!(summary.mergeability, MergeabilityState::Mergeable),
        "merge_relation" => !matches!(
            summary.mergeability,
            MergeabilityState::Behind | MergeabilityState::Conflicting
        ),
        other => return Err(format!("unsupported provider observation '{other}'")),
    };
    let revision_source =
        format!("{subject_id}\n{expected_head}\n{operation}\n{summary:?}\n{details:?}\n{policy:?}");
    let revision = format!("sha256:{:x}", Sha256::digest(revision_source.as_bytes()));
    let threads = details.as_ref().and_then(|details| match &details.review_threads {
        Observation::Known(threads) => Some(threads.iter().filter(|thread| thread.resolvable).map(|thread| {
            let comment = thread.comments.last();
            let thread_revision = format!("sha256:{:x}", Sha256::digest(format!("{expected_head}\n{thread:?}").as_bytes()));
            serde_json::json!({
                "id": thread.native_id.to_string(),
                "revision": thread_revision,
                "resolved": thread.resolved,
                "body": comment.map(|comment| comment.body.as_str()).unwrap_or_default(),
                "author": comment.map(|comment| comment.author.as_str()),
                "created_at": comment.and_then(|comment| comment.created_at.as_deref()),
                "path": comment.and_then(|comment| comment.path.as_deref()),
                "line": comment.and_then(|comment| comment.line),
            })
        }).collect::<Vec<_>>()),
        Observation::EmptyKnown | Observation::AuthoritativelyAbsent => Some(Vec::new()),
        _ => None,
    });
    Ok(serde_json::json!({
        "quality": "current",
        "satisfied": satisfied,
        "head": expected_head,
        "subject": {"id": subject_id, "revision": expected_head},
        "revision": revision,
        "policy_revision": (operation == "policy").then_some(revision.clone()),
        "threads": (operation == "review").then_some(threads).flatten(),
    }))
}

/// Resolve one exact review thread for an opaque workflow Change Request identity. The current
/// head is reobserved immediately before mutation.
pub(crate) async fn resolve_workflow_review_thread(
    path: &Path,
    config: &Config,
    subject_id: &str,
    expected_head: &str,
    thread_id: &str,
    expected_thread_revision: &str,
) -> Result<(), String> {
    let (adapter, discovered) = Adapter::resolve(path, config).await?;
    let (repository_key, native_id) = subject_id
        .rsplit_once(":change_request:")
        .ok_or_else(|| "opaque subject is not a Change Request identity".to_string())?;
    let expected_repository_key = format!(
        "{}:{}:{}",
        discovered.repository.id.provider().config_label(),
        discovered.repository.id.host(),
        discovered.repository.id.project_path()
    );
    if repository_key != expected_repository_key {
        return Err("opaque Change Request belongs to a different repository".into());
    }
    let native_id = super::NativeChangeRequestId::new(native_id.to_string())
        .map_err(|error| error.to_string())?;
    let summary = adapter
        .list_change_requests(&discovered.repository.id, None)
        .await
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|summary| summary.change_request.id.native_id() == &native_id)
        .ok_or_else(|| "opaque Change Request is not open in the target repository".to_string())?;
    if summary.change_request.head_sha != expected_head {
        return Err("Change Request head changed before review-thread resolution".into());
    }
    let details = adapter
        .change_request_details(&summary.change_request)
        .await
        .map_err(|error| error.to_string())?;
    let native_thread =
        NativeReviewThreadId::new(thread_id.to_string()).map_err(|error| error.to_string())?;
    let current = match details.review_threads {
        Observation::Known(threads) => threads
            .into_iter()
            .find(|thread| thread.native_id == native_thread),
        Observation::EmptyKnown | Observation::AuthoritativelyAbsent => None,
        other => return known(other, "review threads").map(|_: Vec<super::ReviewThread>| ()),
    }
    .ok_or_else(|| "review thread is no longer present".to_string())?;
    let current_revision = format!(
        "sha256:{:x}",
        Sha256::digest(format!("{expected_head}\n{current:?}").as_bytes())
    );
    if current_revision != expected_thread_revision {
        return Err("review thread changed after the resolution intent was prepared".into());
    }
    if current.resolved {
        return Ok(());
    }
    if !current.resolvable {
        return Err("review thread is not provider-resolvable".into());
    }
    adapter
        .resolve_review_thread(&ResolveReviewThread {
            id: summary.change_request.id,
            thread_id: native_thread,
            expected_head_sha: expected_head.to_string(),
        })
        .await
        .map_err(|error| error.to_string())
}

pub(crate) async fn merge_workflow_change_request(
    path: &Path,
    config: &Config,
    subject_id: &str,
    expected_head: &str,
) -> Result<serde_json::Value, String> {
    let (adapter, discovered) = Adapter::resolve(path, config).await?;
    let marker = ":change_request:";
    let (repository_key, native_id) = subject_id
        .rsplit_once(marker)
        .ok_or_else(|| "opaque subject is not a Change Request identity".to_string())?;
    let expected_repository_key = format!(
        "{}:{}:{}",
        discovered.repository.id.provider().config_label(),
        discovered.repository.id.host(),
        discovered.repository.id.project_path()
    );
    if repository_key != expected_repository_key {
        return Err("opaque Change Request belongs to a different repository".into());
    }
    let native_id = super::NativeChangeRequestId::new(native_id.to_string())
        .map_err(|error| error.to_string())?;
    let summary = adapter
        .list_change_requests(&discovered.repository.id, None)
        .await
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|summary| summary.change_request.id.native_id() == &native_id)
        .ok_or_else(|| "opaque Change Request is not open in the target repository".to_string())?;
    if summary.change_request.head_sha != expected_head {
        return Err("Change Request identity or head changed before squash merge".into());
    }
    let request = GuardedMerge {
        id: summary.change_request.id.clone(),
        target_repository: summary.change_request.target_repository.clone(),
        target_branch: summary.change_request.target_branch.clone(),
        expected_source_sha: expected_head.to_string(),
        method: MergeMethod::Squash,
        native_guard: None,
    };
    request
        .validate_observation(&summary)
        .map_err(|error| error.to_string())?;
    let result = adapter
        .merge_change_request(&request)
        .await
        .map_err(|error| error.to_string())?;
    Ok(serde_json::json!({
        "status": match result.outcome {
            MergeMutationOutcome::Merged => "merged",
            MergeMutationOutcome::Pending => "pending",
            MergeMutationOutcome::Uncertain => "uncertain",
        },
        "head": expected_head,
        "native_state": result.native_state,
    }))
}

fn policy_satisfied(
    policy: &RepositoryPolicy,
    details: &ChangeRequestDetails,
    summary: &ChangeRequestSummary,
) -> bool {
    let required_checks = match &policy.facts.required_checks {
        Observation::Known(checks) => checks.as_slice(),
        Observation::EmptyKnown | Observation::AuthoritativelyAbsent => &[],
        _ => return false,
    };
    let observed_checks = match &details.checks {
        Observation::Known(checks) => checks.as_slice(),
        Observation::EmptyKnown | Observation::AuthoritativelyAbsent => &[],
        _ => return false,
    };
    let checks_pass = required_checks.iter().all(|required| {
        observed_checks.iter().any(|check| {
            check.name == *required
                && matches!(check.state, CheckState::Passed | CheckState::Skipped)
        })
    });
    let required_approvals = match policy.facts.required_approvals {
        Observation::Known(value) => value,
        Observation::EmptyKnown | Observation::AuthoritativelyAbsent => 0,
        _ => return false,
    };
    let approvals = match &details.reviews {
        Observation::Known(reviews) => reviews
            .iter()
            .filter(|review| matches!(review.decision, ReviewDecision::Approved))
            .map(|review| review.author.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len() as u32,
        Observation::EmptyKnown | Observation::AuthoritativelyAbsent => 0,
        _ => return false,
    };
    let conversations_resolved = match policy.facts.conversations_must_be_resolved {
        Observation::Known(true) => match &details.review_threads {
            Observation::Known(threads) => threads.iter().all(|thread| thread.resolved),
            Observation::EmptyKnown | Observation::AuthoritativelyAbsent => true,
            _ => false,
        },
        Observation::Known(false)
        | Observation::EmptyKnown
        | Observation::AuthoritativelyAbsent => true,
        _ => false,
    };
    let up_to_date = match policy.facts.source_must_be_up_to_date {
        Observation::Known(true) => !matches!(summary.mergeability, MergeabilityState::Behind),
        Observation::Known(false)
        | Observation::EmptyKnown
        | Observation::AuthoritativelyAbsent => true,
        _ => false,
    };
    let queue_ready = match policy.facts.queue_required {
        Observation::Known(true) => !matches!(summary.queue_state, QueueState::Blocked),
        Observation::Known(false)
        | Observation::EmptyKnown
        | Observation::AuthoritativelyAbsent => true,
        _ => false,
    };
    checks_pass
        && approvals >= required_approvals
        && conversations_resolved
        && up_to_date
        && queue_ready
}

pub(crate) async fn observe_change_request_identity(
    path: &Path,
    config: &Config,
    identity: &CanonicalChangeRequestIdentity,
    display_number: u64,
) -> Result<ChangeRequestSummary, String> {
    let id = identity
        .change_request_id(Some(display_number))
        .map_err(|error| error.to_string())?;
    let target = identity
        .target_repository()
        .map_err(|error| error.to_string())?;
    configured_remote_repositories(path, config)
        .await?
        .validate_target_repository(&target)
        .map_err(|_| "change request repository changed since authorization".to_string())?;
    let observed = Adapter::for_repository(path, config, &target)?
        .observe_change_request(&id)
        .await
        .map_err(|error| error.to_string())?;
    if observed.change_request.id != id {
        return Err("provider returned a different change request identity".to_string());
    }
    Ok(observed)
}

pub(crate) async fn observe_change_request_for_source(
    path: &Path,
    config: &Config,
    target: &RemoteRepositoryId,
    source_branch: &str,
    expected_head: &str,
) -> Result<Option<ChangeRequestSummary>, String> {
    configured_remote_repositories(path, config)
        .await?
        .validate_target_repository(target)
        .map_err(|_| "change request target repository is not configured".to_string())?;
    let matches = Adapter::for_repository(path, config, target)?
        .list_change_requests(target, Some(source_branch))
        .await
        .map_err(|error| error.to_string())?
        .into_iter()
        .filter(|summary| {
            summary.change_request.source_branch == source_branch
                && summary.change_request.head_sha == expected_head
        })
        .collect::<Vec<_>>();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.into_iter().next()),
        _ => Err("multiple change requests match the exact source branch and head".to_string()),
    }
}

pub(crate) async fn review_thread_resolution_state(
    path: &Path,
    config: &Config,
    identity: &CanonicalChangeRequestIdentity,
    display_number: u64,
    expected_head: &str,
    thread_id: &str,
) -> Result<Option<bool>, String> {
    let summary = observe_change_request_identity(path, config, identity, display_number).await?;
    if summary.change_request.head_sha != expected_head {
        return Err("change request head changed before review-thread observation".to_string());
    }
    let native_id =
        NativeReviewThreadId::new(thread_id.to_string()).map_err(|error| error.to_string())?;
    let details = Adapter::for_repository(path, config, summary.change_request.id.repository())?
        .change_request_details(&summary.change_request)
        .await
        .map_err(|error| error.to_string())?;
    match details.review_threads {
        Observation::Known(threads) => Ok(threads
            .into_iter()
            .find(|thread| thread.native_id == native_id)
            .map(|thread| thread.resolved)),
        Observation::EmptyKnown | Observation::AuthoritativelyAbsent => Ok(None),
        other => known(other, "review threads").map(|_: Vec<super::ReviewThread>| None),
    }
}

pub(crate) async fn resolve_review_thread_identity(
    path: &Path,
    config: &Config,
    identity: &CanonicalChangeRequestIdentity,
    display_number: u64,
    expected_head: &str,
    thread_id: &str,
) -> Result<(), String> {
    let summary = observe_change_request_identity(path, config, identity, display_number).await?;
    if summary.change_request.head_sha != expected_head {
        return Err("change request head changed before review-thread resolution".to_string());
    }
    let request = ResolveReviewThread {
        id: summary.change_request.id.clone(),
        thread_id: NativeReviewThreadId::new(thread_id.to_string())
            .map_err(|error| error.to_string())?,
        expected_head_sha: expected_head.to_string(),
    };
    Adapter::for_repository(path, config, request.id.repository())?
        .resolve_review_thread(&request)
        .await
        .map_err(|error| error.to_string())
}

fn known<T>(observation: Observation<T>, label: &str) -> Result<T, String> {
    match observation {
        Observation::Known(value) => Ok(value),
        Observation::EmptyKnown => Err(format!("{label} returned an invalid empty fact")),
        Observation::Unsupported => Err(format!("{label} is unsupported")),
        Observation::Unconfigured => Err(format!("{label} is not configured")),
        Observation::Unauthorized => Err(format!("{label} is unauthorized")),
        Observation::NotLoaded => Err(format!("{label} is unknown")),
        Observation::AuthoritativelyAbsent => Err(format!("{label} is authoritatively absent")),
        Observation::Stale { error, .. } => Err(error
            .map(|error| error.to_string())
            .unwrap_or_else(|| format!("{label} is stale"))),
        Observation::Failed(error) => Err(error.to_string()),
    }
}

fn known_vec<T>(observation: Observation<Vec<T>>, label: &str) -> Result<Vec<T>, String> {
    match observation {
        Observation::EmptyKnown | Observation::AuthoritativelyAbsent => Ok(Vec::new()),
        other => known(other, label),
    }
}

fn displayable_vec<T>(
    observation: Observation<Vec<T>>,
    label: &str,
    partial_errors: &mut Vec<String>,
) -> Result<Vec<T>, String> {
    match observation {
        Observation::Stale { value, error } => {
            let error = error
                .map(|error| error.to_string())
                .unwrap_or_else(|| format!("{label} is stale"));
            partial_errors.push(format!("{label}: {error}"));
            Ok(value)
        }
        other => known_vec(other, label),
    }
}

fn to_legacy_summary(summary: ChangeRequestSummary) -> Result<PrSummary, String> {
    let request = summary.change_request;
    let number = request
        .id
        .display_number()
        .ok_or_else(|| "change request has no display number".to_string())?;
    let identity = CanonicalChangeRequestIdentity::new(
        request.id.repository(),
        request.id.native_id(),
        &request.source_repository,
        &request.target_repository,
    );
    Ok(PrSummary {
        number,
        change_request_identity: Some(identity),
        native_state_evidence: summary.native_state_evidence,
        title: summary.title,
        author: summary.author,
        body: summary.body,
        url: summary.web_url.unwrap_or_default(),
        state: lifecycle_label(&summary.lifecycle).to_string(),
        review_decision: review_label(&summary.review_decision).to_string(),
        requested_reviewers: summary.requested_reviewers,
        head_ref: request.source_branch,
        base_ref: request.target_branch,
        head_sha: request.head_sha,
        updated_at: summary.updated_at.unwrap_or_default(),
        check_status: check_label(&summary.check_state).to_string(),
        merge_state_status: mergeability_label(&summary.mergeability).to_string(),
        queue_state: queue_label(&summary.queue_state).to_string(),
        comment_count: summary.comment_count,
        merged: matches!(summary.lifecycle, LifecycleState::Merged),
        draft: summary.draft,
    })
}

fn authoritative_active_summary(summary: PrSummary) -> Result<Option<PrSummary>, String> {
    Ok((summary.merged
        || summary.state.eq_ignore_ascii_case("OPEN")
        || !known_legacy_lifecycle(&summary))
    .then_some(summary))
}

fn known_legacy_lifecycle(summary: &PrSummary) -> bool {
    summary.merged
        || matches!(
            summary.state.trim().to_ascii_uppercase().as_str(),
            "OPEN" | "CLOSED" | "MERGED"
        )
}

fn change_request_from_legacy(summary: &PrSummary) -> Result<ChangeRequest, String> {
    let identity = summary
        .change_request_identity
        .as_ref()
        .ok_or_else(|| "change request identity is incomplete; refresh required".to_string())?;
    Ok(ChangeRequest {
        id: identity
            .change_request_id(Some(summary.number))
            .map_err(|error| error.to_string())?,
        source_repository: identity
            .source_repository()
            .map_err(|error| error.to_string())?,
        target_repository: identity
            .target_repository()
            .map_err(|error| error.to_string())?,
        source_branch: summary.head_ref.clone(),
        target_branch: summary.base_ref.clone(),
        head_sha: summary.head_sha.clone(),
    })
}

fn change_request_summary_from_legacy(summary: PrSummary) -> Result<ChangeRequestSummary, String> {
    let lifecycle = if summary.merged {
        LifecycleState::Merged
    } else {
        LifecycleState::from_native(summary.state.clone())
    };
    Ok(ChangeRequestSummary {
        change_request: change_request_from_legacy(&summary)?,
        title: summary.title,
        author: summary.author,
        body: summary.body,
        web_url: (!summary.url.trim().is_empty()).then_some(summary.url),
        lifecycle,
        review_decision: ReviewDecision::from_native(summary.review_decision),
        requested_reviewers: summary.requested_reviewers,
        mergeability: MergeabilityState::from_native(summary.merge_state_status),
        check_state: CheckState::from_native(summary.check_status),
        queue_state: QueueState::from_native(summary.queue_state),
        native_state_evidence: summary.native_state_evidence,
        comment_count: summary.comment_count,
        draft: summary.draft,
        updated_at: (!summary.updated_at.trim().is_empty()).then_some(summary.updated_at),
    })
}

pub(crate) fn record_change_request_summary(
    repo: &Repository,
    branch: &str,
    cache: &mut PrCache,
    summary: ChangeRequestSummary,
) -> Result<(), String> {
    super::store::record_pr_summary(repo, branch, cache, to_legacy_summary(summary)?);
    Ok(())
}

fn to_legacy_details(details: ChangeRequestDetails) -> ProviderDetailsObservation {
    let mut partial_errors = Vec::new();
    let comments =
        displayable_vec(details.comments, "comments", &mut partial_errors).map(|comments| {
            comments
                .into_iter()
                .map(|comment| PrComment {
                    id: comment.native_id,
                    author: comment.author,
                    body: comment.body,
                    created_at: comment.created_at.unwrap_or_default(),
                })
                .collect()
        });
    let reviews = displayable_vec(details.reviews, "reviews", &mut partial_errors).map(|reviews| {
        reviews
            .into_iter()
            .map(|review| PrReview {
                id: review.native_id,
                author: review.author,
                state: review_label(&review.decision).to_string(),
                body: review.body,
                submitted_at: review.submitted_at.unwrap_or_default(),
            })
            .collect()
    });
    let review_comments = displayable_vec(
        details.review_threads,
        "review threads",
        &mut partial_errors,
    )
    .map(|threads| {
        threads
            .into_iter()
            .flat_map(|thread| {
                let thread_id = thread.native_id.to_string();
                let resolvable = thread.resolvable;
                thread
                    .comments
                    .into_iter()
                    .map(move |comment| PrReviewComment {
                        thread_id: if resolvable {
                            thread_id.clone()
                        } else {
                            String::new()
                        },
                        id: comment.native_id,
                        author: comment.author,
                        path: comment.path.unwrap_or_default(),
                        line: comment
                            .line
                            .map(|line| line.to_string())
                            .unwrap_or_default(),
                        body: comment.body,
                        created_at: comment.created_at.unwrap_or_default(),
                        resolved: thread.resolved,
                    })
            })
            .collect()
    });
    let check_contexts =
        displayable_vec(details.checks, "checks", &mut partial_errors).map(|checks| {
            checks
                .into_iter()
                .map(|check| PrCheckContext {
                    name: check.name,
                    state: legacy_check_state(&check.state),
                })
                .collect::<Vec<_>>()
        });
    let failing_checks = match &check_contexts {
        Ok(checks) => Ok(checks
            .iter()
            .filter(|check| matches!(check.state, PrCheckState::Failed | PrCheckState::Mixed))
            .map(|check| check.name.clone())
            .collect()),
        Err(error) => Err(error.clone()),
    };
    let ci_failures =
        displayable_vec(details.ci_failures, "CI logs", &mut partial_errors).map(|failures| {
            failures
                .into_iter()
                .map(|failure| LegacyCiFailure {
                    workflow: failure.pipeline,
                    name: failure.job,
                    conclusion: failure.native_conclusion,
                    url: failure.web_url.unwrap_or_default(),
                    run_id: failure.native_run_id,
                    log_tail: failure.log_tail,
                })
                .collect()
        });
    ProviderDetailsObservation {
        comments,
        reviews,
        review_comments,
        files: displayable_vec(details.changed_files, "changed files", &mut partial_errors),
        failing_checks,
        check_contexts,
        ci_failures,
        partial_errors,
    }
}

fn lifecycle_label(state: &LifecycleState) -> &str {
    match state {
        LifecycleState::Open => "OPEN",
        LifecycleState::Closed => "CLOSED",
        LifecycleState::Merged => "MERGED",
        LifecycleState::Unknown(native) => native,
    }
}

fn review_label(state: &ReviewDecision) -> &str {
    match state {
        ReviewDecision::Approved => "APPROVED",
        ReviewDecision::ChangesRequested => "CHANGES_REQUESTED",
        ReviewDecision::ReviewRequired => "REVIEW_REQUIRED",
        ReviewDecision::Pending => "PENDING",
        ReviewDecision::Dismissed => "DISMISSED",
        ReviewDecision::Unknown(native) => native,
    }
}

fn check_label(state: &CheckState) -> &str {
    match state {
        CheckState::Pending => "running",
        CheckState::Passed | CheckState::Skipped => "passed",
        CheckState::Failed | CheckState::Cancelled => "failed",
        CheckState::Mixed => "mixed",
        CheckState::Unknown(_) => "unknown",
    }
}

fn mergeability_label(state: &MergeabilityState) -> &str {
    match state {
        MergeabilityState::Mergeable => "CLEAN",
        MergeabilityState::Conflicting => "DIRTY",
        MergeabilityState::Blocked => "BLOCKED",
        MergeabilityState::Behind => "BEHIND",
        MergeabilityState::Unknown(native) => native,
    }
}

fn queue_label(state: &QueueState) -> &str {
    match state {
        QueueState::NotQueued => "not_queued",
        QueueState::Queued => "queued",
        QueueState::Running => "running",
        QueueState::Blocked => "blocked",
        QueueState::Complete => "complete",
        QueueState::Unknown(native) => native,
    }
}

fn legacy_check_state(state: &CheckState) -> PrCheckState {
    match state {
        CheckState::Pending => PrCheckState::Pending,
        CheckState::Passed | CheckState::Skipped => PrCheckState::Success,
        CheckState::Failed | CheckState::Cancelled => PrCheckState::Failed,
        CheckState::Mixed => PrCheckState::Mixed,
        CheckState::Unknown(_) => PrCheckState::Unknown,
    }
}

fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repository(provider: ProviderKind, project: &str) -> RemoteRepositoryId {
        let host = match provider {
            ProviderKind::GitHub => "github.com",
            ProviderKind::GitLab => "gitlab.com",
            ProviderKind::Forgejo => "codeberg.org",
        };
        RemoteRepositoryId::new(provider, HostIdentity::parse(host).unwrap(), project).unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_guard_rejects_target_or_base_drift() {
        let source_repository = repository(ProviderKind::GitHub, "contributor/widget");
        let expected = CreateChangeRequestGuard {
            source_push: PushGuard {
                repository: source_repository.clone(),
                remote: "origin".to_string(),
                remote_branch: "feature".to_string(),
                local_branch: "feature".to_string(),
                expected_head_sha: "head-a".to_string(),
                set_upstream: false,
            },
            source_repository,
            target_repository: repository(ProviderKind::GitHub, "acme/widget"),
            local_branch: "feature".to_string(),
            source_branch: "feature".to_string(),
            target_branch: "main".to_string(),
            expected_head_sha: "head-a".to_string(),
            expected_base_sha: "base-a".to_string(),
        };
        assert!(validate_create_change_request_guard(&expected, &expected).is_ok());

        let changed_target = CreateChangeRequestGuard {
            target_repository: repository(ProviderKind::GitHub, "other/widget"),
            ..expected.clone()
        };
        assert!(
            validate_create_change_request_guard(&expected, &changed_target)
                .unwrap_err()
                .contains("target")
        );
        let changed_base = CreateChangeRequestGuard {
            expected_base_sha: "base-b".to_string(),
            ..expected.clone()
        };
        assert!(validate_create_change_request_guard(&expected, &changed_base).is_err());
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn push_guard_uses_git_push_destination_and_canonical_push_url() {
        let directory = std::env::temp_dir().join(format!(
            "prism-push-guard-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let mut config = crate::test_support::test_config();
        crate::test_support::install_tool(
            &mut config,
            &directory,
            "git",
            r#"#!/bin/sh
case "$*" in
  *"branch --show-current"*) printf '%s\n' 'feature' ;;
  *"for-each-ref --format=%(push:remotename)%00%(push) refs/heads/feature"*) printf 'publish\000refs/remotes/publish/review/feature\n' ;;
  *"remote get-url --push --all publish"*) printf '%s\n' 'https://github.com/contributor/widget.git' ;;
  *"remote get-url publish --push"*) printf '%s\n' 'https://github.com/contributor/widget.git' ;;
  *"rev-parse HEAD"*) printf '%s\n' 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' ;;
  *) exit 1 ;;
esac
"#,
        );

        let guard = prepare_push(&directory, &config, "feature").await.unwrap();

        assert_eq!(guard.remote, "publish");
        assert_eq!(guard.remote_branch, "review/feature");
        assert_eq!(
            guard.repository,
            repository(ProviderKind::GitHub, "contributor/widget")
        );
        assert!(!guard.set_upstream);
        std::fs::remove_dir_all(directory).unwrap();
    }

    fn legacy_summary() -> PrSummary {
        PrSummary {
            number: 42,
            change_request_identity: Some(crate::remote::test_change_request_identity()),
            native_state_evidence: super::super::NativeStateEvidence::default(),
            title: "Change".to_string(),
            author: "example".to_string(),
            body: String::new(),
            url: "https://github.com/example/repo/pull/42".to_string(),
            state: "OPEN".to_string(),
            review_decision: "APPROVED".to_string(),
            requested_reviewers: Vec::new(),
            head_ref: "topic".to_string(),
            base_ref: "main".to_string(),
            head_sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            updated_at: String::new(),
            check_status: "success".to_string(),
            merge_state_status: "CLEAN".to_string(),
            queue_state: "not_queued".to_string(),
            comment_count: 0,
            merged: false,
            draft: false,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fork_fetch_uses_the_configured_target_request_ref() {
        let source = repository(ProviderKind::GitHub, "contributor/widget");
        let target = repository(ProviderKind::GitHub, "acme/widget");
        let configured = [("origin", source.clone()), ("upstream", target.clone())];

        let fetch = select_fetch_source(
            ProviderKind::GitHub,
            42,
            "topic",
            &source,
            &target,
            &configured,
        )
        .unwrap();

        assert_eq!(fetch.remote_name, "upstream");
        assert_eq!(fetch.remote_ref, "refs/pull/42/head");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn upstream_github_review_uses_the_canonical_target_repository() {
        let directory = std::env::temp_dir().join(format!(
            "prism-upstream-review-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let log = directory.join("gh.log");
        let mut config = crate::test_support::test_config();
        crate::test_support::install_tool(
            &mut config,
            &directory,
            "git",
            r#"#!/bin/sh
case "$*" in
  *"remote get-url --push --all origin"*) printf '%s\n' 'https://github.com/contributor/widget.git' ;;
  *"remote get-url origin --push"*) printf '%s\n' 'https://github.com/contributor/widget.git' ;;
  *"remote get-url origin"*) printf '%s\n' 'https://github.com/contributor/widget.git' ;;
  *"remote get-url upstream"*) printf '%s\n' 'https://github.com/acme/widget.git' ;;
  *) exit 1 ;;
esac
"#,
        );
        crate::test_support::install_tool(
            &mut config,
            &directory,
            "gh",
            &format!(
                r#"#!/bin/sh
case "$*" in
  "api graphql"*)
    printf '%s\n' '{{"data":{{"repository":{{"pullRequest":{{"id":"PR_42","number":42,"title":"Change","state":"OPEN","headRefName":"topic","baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRepository":{{"nameWithOwner":"contributor/widget"}},"baseRepository":{{"nameWithOwner":"acme/widget"}}}}}}}}}}'
    ;;
  *"/repos/acme/widget/pulls/42/reviews"*)
    printf '%s\n' "$*" > '{}'
    printf '%s\n' '{{"commit_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}'
    ;;
  *) exit 1 ;;
esac
"#,
                log.display()
            ),
        );
        let source = repository(ProviderKind::GitHub, "contributor/widget");
        let target = repository(ProviderKind::GitHub, "acme/widget");
        let mut summary = legacy_summary();
        summary.change_request_identity = Some(CanonicalChangeRequestIdentity::new(
            &target,
            &super::super::NativeChangeRequestId::new("PR_42").unwrap(),
            &source,
            &target,
        ));

        submit_review(
            &directory,
            &config,
            &summary,
            ReviewSubmissionKind::Approve,
            "looks good".to_string(),
        )
        .await
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(&log).unwrap().trim(),
            "api /repos/acme/widget/pulls/42/reviews --hostname github.com --method POST -H Accept: application/vnd.github+json -f commit_id=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa -f event=APPROVE -f body=looks good"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn cached_github_details_use_canonical_target_number_not_origin_branch() {
        let directory = std::env::temp_dir().join(format!(
            "prism-upstream-details-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let log = directory.join("gh.log");
        let mut config = crate::test_support::test_config();
        crate::test_support::install_tool(
            &mut config,
            &directory,
            "git",
            r#"#!/bin/sh
case "$*" in
  *"remote get-url --push --all origin"*) printf '%s\n' 'https://github.com/contributor/widget.git' ;;
  *"remote get-url origin --push"*) printf '%s\n' 'https://github.com/contributor/widget.git' ;;
  *"remote get-url origin"*) printf '%s\n' 'https://github.com/contributor/widget.git' ;;
  *"remote get-url upstream"*) printf '%s\n' 'https://github.com/acme/widget.git' ;;
  *) exit 1 ;;
esac
"#,
        );
        crate::test_support::install_tool(
            &mut config,
            &directory,
            "gh",
            &format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$*" in
  *"api graphql"*"reviewThreads(first: 100"*)
    printf '%s\n' '[{{"data":{{"repository":{{"pullRequest":{{"reviewThreads":{{"totalCount":0,"pageInfo":{{"hasNextPage":false}},"nodes":[]}}}}}}}}}}]'
    ;;
  *"api graphql"*"owner=acme"*"number=42"*)
    printf '%s\n' '{{"data":{{"repository":{{"pullRequest":{{"id":"PR_42","number":42,"title":"Fork change","state":"OPEN","headRefName":"topic","baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRepository":{{"nameWithOwner":"contributor/widget"}},"baseRepository":{{"nameWithOwner":"acme/widget"}}}}}}}}}}'
    ;;
  *"/issues/42/comments"*|*"/pulls/42/reviews"*|*"/pulls/42/files"*|*"/commits/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/statuses"*)
    printf '%s\n' '[[]]'
    ;;
  *"/commits/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/check-runs"*)
    printf '%s\n' '[{{"total_count":0,"check_runs":[]}}]'
    ;;
  *) exit 1 ;;
esac
"#,
                log.display()
            ),
        );
        let source = repository(ProviderKind::GitHub, "contributor/widget");
        let target = repository(ProviderKind::GitHub, "acme/widget");
        let mut summary = legacy_summary();
        summary.change_request_identity = Some(CanonicalChangeRequestIdentity::new(
            &target,
            &super::super::NativeChangeRequestId::new("PR_42").unwrap(),
            &source,
            &target,
        ));
        let mut cache = PrCache::observed(summary, None);

        refresh_change_request_details_state(
            "synthetic-local-branch",
            &mut cache,
            &directory,
            &config,
        )
        .await;

        let commands = std::fs::read_to_string(&log).unwrap();
        assert!(commands.contains("owner=acme"));
        assert!(commands.contains("/repos/acme/widget/issues/42/comments?per_page=100"));
        assert!(!commands.contains("pr view synthetic-local-branch"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn forgejo_fetch_uses_the_canonical_source_branch() {
        let source = repository(ProviderKind::Forgejo, "contributor/widget");
        let target = repository(ProviderKind::Forgejo, "acme/widget");
        let configured = [("origin", source.clone()), ("upstream", target.clone())];

        let fetch = select_fetch_source(
            ProviderKind::Forgejo,
            42,
            "topic",
            &source,
            &target,
            &configured,
        )
        .unwrap();

        assert_eq!(fetch.remote_name, "origin");
        assert_eq!(fetch.remote_ref, "refs/heads/topic");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn forgejo_fork_fetch_uses_the_configured_target_request_ref_without_source_remote() {
        let source = repository(ProviderKind::Forgejo, "contributor/widget");
        let target = repository(ProviderKind::Forgejo, "acme/widget");
        let configured = [("origin", target.clone())];

        let fetch = select_fetch_source(
            ProviderKind::Forgejo,
            42,
            "topic",
            &source,
            &target,
            &configured,
        )
        .unwrap();

        assert_eq!(fetch.remote_name, "origin");
        assert_eq!(fetch.remote_ref, "refs/pull/42/head");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fetch_rejects_unconfigured_source_and_target_repositories() {
        let source = repository(ProviderKind::GitLab, "contributor/widget");
        let target = repository(ProviderKind::GitLab, "acme/widget");
        let configured = [(
            "origin",
            repository(ProviderKind::GitLab, "unrelated/widget"),
        )];

        let error = select_fetch_source(
            ProviderKind::GitLab,
            42,
            "topic",
            &source,
            &target,
            &configured,
        )
        .err()
        .unwrap();

        assert!(error.contains("no configured fetch remote matches"));
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn change_request_discovery_includes_distinct_origin_and_upstream_identities() {
        let directory = std::env::temp_dir().join(format!(
            "prism-remote-identities-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let mut config = crate::test_support::test_config();
        crate::test_support::install_tool(
            &mut config,
            &directory,
            "git",
            r#"#!/bin/sh
case "$*" in
  *"remote get-url origin --push"*) printf '%s\n' 'https://github.com/contributor/widget.git' ;;
  *"remote get-url upstream --push"*) printf '%s\n' 'https://github.com/acme/widget.git' ;;
  *"remote get-url origin"*) printf '%s\n' 'https://github.com/contributor/widget.git' ;;
  *"remote get-url upstream"*) printf '%s\n' 'https://github.com/acme/widget.git' ;;
  *) exit 1 ;;
esac
"#,
        );

        let repositories = configured_change_request_repositories(&directory, &config)
            .await
            .unwrap();

        assert_eq!(
            repositories,
            [
                repository(ProviderKind::GitHub, "contributor/widget"),
                repository(ProviderKind::GitHub, "acme/widget"),
            ]
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn triangular_remote_identities_are_independent_deduplicated_and_guard_mutations() {
        let directory = std::env::temp_dir().join(format!(
            "prism-triangular-identities-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let mut config = crate::test_support::test_config();
        crate::test_support::install_tool(
            &mut config,
            &directory,
            "git",
            r#"#!/bin/sh
case "$*" in
  *"remote get-url origin --push"*) printf '%s\n' 'https://github.com/contributor/widget.git' ;;
  *"remote get-url upstream --push"*) printf '%s\n' 'https://github.com/release/widget.git' ;;
  *"remote get-url origin"*) printf '%s\n' 'https://github.com/acme/widget.git' ;;
  *"remote get-url upstream"*) printf '%s\n' 'https://github.com/acme/widget.git' ;;
  *) exit 1 ;;
esac
"#,
        );

        let remotes = configured_remote_repositories(&directory, &config)
            .await
            .unwrap();
        let target = repository(ProviderKind::GitHub, "acme/widget");
        let source = repository(ProviderKind::GitHub, "contributor/widget");

        assert_eq!(remotes.origin_fetch, target);
        assert_eq!(remotes.origin_push, source);
        assert_eq!(remotes.upstream_fetch.as_ref(), Some(&target));
        assert_eq!(
            remotes.upstream_push,
            Some(repository(ProviderKind::GitHub, "release/widget"))
        );
        assert_eq!(
            remotes.fetch_repositories.as_slice(),
            std::slice::from_ref(&target)
        );
        assert_eq!(remotes.create_target(None).unwrap(), target);
        assert_eq!(
            configured_change_request_repositories(&directory, &config)
                .await
                .unwrap(),
            std::slice::from_ref(&target)
        );
        assert!(remotes.validate_source_mutation(&source, &target).is_ok());
        assert!(
            remotes
                .validate_source_mutation(
                    &repository(ProviderKind::GitHub, "former-contributor/widget"),
                    &target,
                )
                .unwrap_err()
                .contains("push URL")
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn triangular_create_uses_origin_push_source_and_explicit_fetch_target() {
        let directory = std::env::temp_dir().join(format!(
            "prism-triangular-create-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let git_log = directory.join("git.log");
        let gh_log = directory.join("gh.log");
        let mut config = crate::test_support::test_config();
        crate::test_support::install_tool(
            &mut config,
            &directory,
            "git",
            &format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$*" in
  *"remote get-url --push --all origin"*) printf '%s\n' 'https://github.com/contributor/widget.git' ;;
  *"remote get-url origin --push"*) printf '%s\n' 'https://github.com/contributor/widget.git' ;;
  *"remote get-url origin"*) printf '%s\n' 'https://github.com/acme/widget.git' ;;
  *"remote get-url upstream"*) exit 2 ;;
  *"branch --show-current"*) printf '%s\n' 'topic' ;;
  *"for-each-ref --format=%(push:remotename)%00%(push) refs/heads/topic"*) printf 'origin\000refs/remotes/origin/topic\n' ;;
  *"fetch --no-tags origin +refs/heads/main:refs/remotes/origin/main"*) exit 0 ;;
  *"rev-parse --verify --quiet refs/remotes/origin/main"*) printf '%s\n' 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb' ;;
  *"rev-parse HEAD"*) printf '%s\n' 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' ;;
  *"ls-remote --exit-code --heads https://github.com/contributor/widget.git refs/heads/topic"*) printf '%s\t%s\n' 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' 'refs/heads/topic' ;;
  *) exit 1 ;;
esac
"#,
                git_log.display()
            ),
        );
        crate::test_support::install_tool(
            &mut config,
            &directory,
            "gh",
            &format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$*" in
  "pr create"*) printf '%s\n' 'https://github.com/acme/widget/pull/42' ;;
  *"number=42"*) printf '%s\n' '{{"data":{{"repository":{{"pullRequest":{{"id":"PR_fork","number":42,"title":"Fork change","state":"OPEN","merged":false,"headRefName":"topic","baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRepository":{{"nameWithOwner":"contributor/widget"}},"baseRepository":{{"nameWithOwner":"acme/widget"}}}}}}}}}}' ;;
  *) printf '%s\n' '{{"data":{{"repository":{{"pullRequests":{{"nodes":[{{"id":"PR_wrong_base","number":40,"state":"OPEN","headRefName":"topic","baseRefName":"release","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRepository":{{"nameWithOwner":"contributor/widget"}},"baseRepository":{{"nameWithOwner":"acme/widget"}}}},{{"id":"PR_wrong_head","number":41,"state":"OPEN","headRefName":"topic","baseRefName":"main","headRefOid":"cccccccccccccccccccccccccccccccccccccccc","headRepository":{{"nameWithOwner":"contributor/widget"}},"baseRepository":{{"nameWithOwner":"acme/widget"}}}},{{"id":"PR_fork","number":42,"title":"Fork change","state":"OPEN","merged":false,"headRefName":"topic","baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRepository":{{"nameWithOwner":"contributor/widget"}},"baseRepository":{{"nameWithOwner":"acme/widget"}}}}],"pageInfo":{{"hasNextPage":false}}}}}}}}}}' ;;
esac
"#,
                gh_log.display()
            ),
        );
        let repo =
            Repository::with_config_dir_for_test(directory.clone(), directory.join("config"));
        let mut cache = PrCache::default();
        let target = repository(ProviderKind::GitHub, "acme/widget");
        let source_push = prepare_push(&directory, &config, "topic").await.unwrap();
        let guard =
            prepare_create_change_request(&directory, &config, "topic", &target, &source_push)
                .await
                .unwrap();

        create_change_request(&repo, &config, &directory, "body", &guard, &mut cache)
            .await
            .unwrap();

        let identity = cache
            .summary()
            .unwrap()
            .change_request_identity
            .as_ref()
            .unwrap();
        assert_eq!(
            identity.source_repository().unwrap(),
            repository(ProviderKind::GitHub, "contributor/widget")
        );
        assert_eq!(
            identity.target_repository().unwrap(),
            repository(ProviderKind::GitHub, "acme/widget")
        );
        let commands = std::fs::read_to_string(&gh_log).unwrap();
        assert!(commands.contains(
            "pr create --fill --body body --repo acme/widget --base main --head contributor:topic"
        ));
        let commands = std::fs::read_to_string(&git_log).unwrap();
        assert!(commands.contains("remote get-url origin"));
        assert!(commands.contains("remote get-url origin --push"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn polling_associates_the_configured_branch_push_repository_as_source() {
        let directory = std::env::temp_dir().join(format!(
            "prism-triangular-poll-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let gh_log = directory.join("gh.log");
        let mut config = crate::test_support::test_config();
        crate::test_support::install_tool(
            &mut config,
            &directory,
            "git",
            r#"#!/bin/sh
case "$*" in
  *"remote get-url --push --all publish"*) printf '%s\n' 'https://github.com/contributor/widget.git' ;;
  *"remote get-url publish --push"*) printf '%s\n' 'https://github.com/contributor/widget.git' ;;
  *"remote get-url origin --push"*) printf '%s\n' 'https://github.com/acme/widget.git' ;;
  *"remote get-url upstream --push"*) printf '%s\n' 'https://github.com/acme/widget.git' ;;
  *"remote get-url origin"*) printf '%s\n' 'https://github.com/acme/widget.git' ;;
  *"remote get-url upstream"*) printf '%s\n' 'https://github.com/acme/widget.git' ;;
  *"branch --show-current"*) printf '%s\n' 'topic' ;;
  *"for-each-ref --format=%(push:remotename)%00%(push) refs/heads/topic"*) printf 'publish\000refs/remotes/publish/review/topic\n' ;;
  *"rev-parse HEAD"*) printf '%s\n' 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' ;;
  *) exit 1 ;;
esac
"#,
        );
        crate::test_support::install_tool(
            &mut config,
            &directory,
            "gh",
            &format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
printf '%s\n' '{{"data":{{"repository":{{"pullRequests":{{"nodes":[{{"id":"PR_fork","number":42,"title":"Fork change","state":"OPEN","merged":false,"headRefName":"review/topic","baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRepository":{{"nameWithOwner":"contributor/widget"}},"baseRepository":{{"nameWithOwner":"acme/widget"}},"comments":{{"totalCount":4}},"reviewThreads":{{"totalCount":2}}}}],"pageInfo":{{"hasNextPage":false}}}}}}}}}}'
"#,
                gh_log.display()
            ),
        );
        let repo =
            Repository::with_config_dir_for_test(directory.clone(), directory.join("config"));
        let mut cache = PrCache::default();

        refresh_change_request_cache(&repo, "topic", &mut cache, &directory, &config, false)
            .await
            .unwrap();

        let identity = cache
            .summary()
            .unwrap()
            .change_request_identity
            .as_ref()
            .unwrap();
        assert_eq!(
            identity.source_repository().unwrap(),
            repository(ProviderKind::GitHub, "contributor/widget")
        );
        assert_eq!(cache.summary().unwrap().comment_count, 6);
        let commands = std::fs::read_to_string(&gh_log).unwrap();
        assert_eq!(commands.matches("api graphql").count(), 1);
        assert!(commands.contains("headRefName=review/topic"));
        assert!(commands.contains("states: OPEN"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn maintainer_target_checkout_can_merge_a_fork_change_request() {
        let directory = std::env::temp_dir().join(format!(
            "prism-changed-push-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let gh_log = directory.join("gh.log");
        let mut config = crate::test_support::test_config();
        crate::test_support::install_tool(
            &mut config,
            &directory,
            "git",
            r#"#!/bin/sh
case "$*" in
  *"remote get-url origin --push"*) printf '%s\n' 'https://github.com/new-contributor/widget.git' ;;
  *"remote get-url origin"*) printf '%s\n' 'https://github.com/acme/widget.git' ;;
  *"remote get-url upstream"*) exit 2 ;;
  *) exit 1 ;;
esac
"#,
        );
        crate::test_support::install_tool(
            &mut config,
            &directory,
            "gh",
            &format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$*" in
  *"api graphql"*)
    printf '%s\n' '{{"data":{{"repository":{{"pullRequest":{{"id":"PR_stale","number":42,"title":"Fork change","state":"OPEN","headRefName":"topic","baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRepository":{{"nameWithOwner":"former-contributor/widget"}},"baseRepository":{{"nameWithOwner":"acme/widget"}}}}}}}}}}'
    ;;
  *"pr merge 42"*) exit 0 ;;
  *) exit 1 ;;
esac
"#,
                gh_log.display()
            ),
        );
        let source = repository(ProviderKind::GitHub, "former-contributor/widget");
        let target = repository(ProviderKind::GitHub, "acme/widget");
        let identity = CanonicalChangeRequestIdentity::new(
            &target,
            &super::super::NativeChangeRequestId::new("PR_stale").unwrap(),
            &source,
            &target,
        );

        let result = merge_change_request(
            &config,
            &directory,
            &identity,
            42,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
        .await
        .unwrap();

        assert_eq!(
            result.outcome,
            super::super::MergeMutationOutcome::Uncertain
        );
        let commands = std::fs::read_to_string(&gh_log).unwrap();
        assert!(commands.contains(
            "pr merge 42 --squash --match-head-commit aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa --repo acme/widget"
        ));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn maintainer_target_checkout_can_resolve_a_fork_review_thread() {
        let directory = std::env::temp_dir().join(format!(
            "prism-maintainer-resolve-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let mut config = crate::test_support::test_config();
        crate::test_support::install_tool(
            &mut config,
            &directory,
            "git",
            r#"#!/bin/sh
case "$*" in
  *"remote get-url origin --push"*) printf '%s\n' 'https://github.com/maintainer/widget.git' ;;
  *"remote get-url origin"*) printf '%s\n' 'https://github.com/acme/widget.git' ;;
  *"remote get-url upstream"*) exit 2 ;;
  *) exit 1 ;;
esac
"#,
        );
        crate::test_support::install_tool(
            &mut config,
            &directory,
            "gh",
            r#"#!/bin/sh
case "$*" in
  *"resolveReviewThread(input:"*)
    printf '%s\n' '{"data":{"resolveReviewThread":{"thread":{"id":"PRRT_1","isResolved":true}}}}'
    ;;
  *"reviewThreads(first: 100"*)
    printf '%s\n' '[{"data":{"repository":{"pullRequest":{"reviewThreads":{"totalCount":1,"pageInfo":{"hasNextPage":false},"nodes":[{"id":"PRRT_1","isResolved":false,"comments":{"totalCount":1,"pageInfo":{"hasNextPage":false},"nodes":[{"id":"PRRC_1","author":{"login":"reviewer"},"path":"src/lib.rs","line":7,"body":"review","createdAt":"2026-01-01T00:00:00Z"}]}}]}}}}}]'
    ;;
  *"/issues/42/comments"*|*"/pulls/42/reviews"*|*"/pulls/42/files"*|*"/statuses"*)
    printf '%s\n' '[[]]'
    ;;
  *"/check-runs"*)
    printf '%s\n' '[{"total_count":0,"check_runs":[]}]'
    ;;
  *"api graphql"*)
    printf '%s\n' '{"data":{"repository":{"pullRequest":{"id":"PR_42","number":42,"title":"Fork change","state":"OPEN","headRefName":"topic","baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRepository":{"nameWithOwner":"contributor/widget"},"baseRepository":{"nameWithOwner":"acme/widget"}}}}}'
    ;;
  *) exit 1 ;;
esac
"#,
        );
        let source = repository(ProviderKind::GitHub, "contributor/widget");
        let target = repository(ProviderKind::GitHub, "acme/widget");
        let mut summary = legacy_summary();
        summary.change_request_identity = Some(CanonicalChangeRequestIdentity::new(
            &target,
            &super::super::NativeChangeRequestId::new("PR_42").unwrap(),
            &source,
            &target,
        ));

        resolve_review_thread(&directory, &config, &summary, "PRRT_1")
            .await
            .unwrap();

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_lifecycle_is_not_converted_to_authoritative_absence() {
        let mut summary = legacy_summary();
        summary.state = "SUPERSEDED_BY_TRAIN".to_string();

        let observed = authoritative_active_summary(summary.clone())
            .unwrap()
            .expect("unknown lifecycle remains displayable");
        assert_eq!(observed.state, "SUPERSEDED_BY_TRAIN");
        let normalized = change_request_summary_from_legacy(summary).unwrap();
        assert_eq!(
            normalized.lifecycle,
            LifecycleState::Unknown("SUPERSEDED_BY_TRAIN".to_string())
        );
        assert_eq!(
            to_legacy_summary(normalized).unwrap().state,
            "SUPERSEDED_BY_TRAIN"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn compatibility_conversion_preserves_unknown_native_queue_state() {
        let mut summary = legacy_summary();
        summary.queue_state = "preparing_merged_result".to_string();
        summary.native_state_evidence = super::super::NativeStateEvidence {
            lifecycle: vec!["OPEN".to_string()],
            review: vec!["REVIEW_REQUIRED".to_string()],
            mergeability: vec!["CLEAN".to_string()],
            check: vec!["COMPLETED".to_string(), "NEUTRAL".to_string()],
            queue: vec!["PREPARING".to_string()],
        };

        let normalized = change_request_summary_from_legacy(summary).unwrap();
        assert_eq!(
            normalized.queue_state,
            QueueState::Unknown("preparing_merged_result".to_string())
        );
        assert_eq!(normalized.native_state_evidence.mergeability, ["CLEAN"]);
        assert_eq!(
            normalized.native_state_evidence.check,
            ["COMPLETED", "NEUTRAL"]
        );
        let round_trip = to_legacy_summary(normalized).unwrap();
        assert_eq!(round_trip.queue_state, "preparing_merged_result");
        assert_eq!(round_trip.native_state_evidence.queue, ["PREPARING"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn compatibility_conversion_preserves_comment_count_and_ui_check_labels() {
        let labels = [
            ("pending", "running"),
            ("running", "running"),
            ("passed", "passed"),
            ("failed", "failed"),
            ("mixed", "mixed"),
            ("unknown", "unknown"),
        ];

        for (legacy, expected) in labels {
            let mut summary = legacy_summary();
            summary.comment_count = 17;
            summary.check_status = legacy.to_string();

            let normalized = change_request_summary_from_legacy(summary).unwrap();
            assert_eq!(normalized.comment_count, 17);
            let round_trip = to_legacy_summary(normalized).unwrap();
            assert_eq!(round_trip.comment_count, 17);
            assert_eq!(round_trip.check_status, expected);
            assert_eq!(
                round_trip.check_state(),
                PrCheckState::from_label(expected),
                "{legacy}"
            );
        }

        let mut unknown = legacy_summary();
        unknown.check_status = "unknown".to_string();
        let round_trip =
            to_legacy_summary(change_request_summary_from_legacy(unknown).unwrap()).unwrap();
        assert_eq!(round_trip.check_state(), PrCheckState::Unknown);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn cached_github_exact_lookup_absence_clears_stale_summary_authoritatively() {
        let directory = std::env::temp_dir().join(format!(
            "prism-github-exact-absence-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let mut config = crate::test_support::test_config();
        crate::test_support::install_tool(
            &mut config,
            &directory,
            "git",
            r#"#!/bin/sh
case "$*" in
  *"remote get-url --push --all origin"*|*"remote get-url origin --push"*|*"remote get-url origin"*)
    printf '%s\n' 'https://github.com/example/repo.git'
    ;;
  *"remote get-url"*"upstream"*) exit 2 ;;
  *) exit 1 ;;
esac
"#,
        );
        crate::test_support::install_tool(
            &mut config,
            &directory,
            "gh",
            r#"#!/bin/sh
printf '%s\n' '{"data":{"repository":{"pullRequest":null}}}'
"#,
        );
        let repo =
            Repository::with_config_dir_for_test(directory.clone(), directory.join("config"));
        let mut cache = PrCache::observed(legacy_summary(), None);

        refresh_change_request_cache(&repo, "topic", &mut cache, &directory, &config, false)
            .await
            .unwrap();

        assert!(cache.summary().is_none());
        assert_eq!(
            cache.summary_observation_quality(),
            PrObservationQuality::AuthoritativeAbsence
        );
        assert_eq!(cache.trusted_summary().unwrap(), None);
        assert_eq!(cache.display_error(), None);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn merge_verification_observes_the_canonical_fork_target() {
        let directory = std::env::temp_dir().join(format!(
            "prism-fork-merge-verification-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let log = directory.join("gh.log");
        let mut config = crate::test_support::test_config();
        crate::test_support::install_tool(
            &mut config,
            &directory,
            "gh",
            &format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
printf '%s\n' '{{"data":{{"repository":{{"pullRequest":{{"id":"PR_fork","number":42,"title":"Fork change","state":"MERGED","merged":true,"headRefName":"topic","baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRepository":{{"nameWithOwner":"contributor/widget"}},"baseRepository":{{"nameWithOwner":"acme/widget"}}}}}}}}}}'
"#,
                log.display()
            ),
        );
        let source = repository(ProviderKind::GitHub, "contributor/widget");
        let target = repository(ProviderKind::GitHub, "acme/widget");
        let expected = ChangeRequest {
            id: super::super::ChangeRequestId::new(
                target.clone(),
                super::super::NativeChangeRequestId::new("PR_fork").unwrap(),
                Some(42),
            ),
            source_repository: source,
            target_repository: target,
            source_branch: "topic".to_string(),
            target_branch: "main".to_string(),
            head_sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
        };

        let observed = wait_for_change_request_merged(&directory, &expected, &config)
            .await
            .unwrap();

        assert_eq!(observed.lifecycle, LifecycleState::Merged);
        let commands = std::fs::read_to_string(&log).unwrap();
        assert!(commands.contains("owner=acme"), "{commands}");
        assert!(commands.contains("name=widget"), "{commands}");
        assert!(commands.contains("number=42"), "{commands}");
        assert!(
            commands.contains("pullRequest(number: $number)"),
            "{commands}"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unavailable_ci_logs_do_not_invalidate_other_legacy_details() {
        let mut cache = PrCache::observed(legacy_summary(), None);
        let details = ChangeRequestDetails {
            association: None,
            comments: Observation::EmptyKnown,
            reviews: Observation::EmptyKnown,
            review_threads: Observation::Unsupported,
            changed_files: Observation::Known(vec!["src/lib.rs".to_string()]),
            checks: Observation::Known(Vec::new()),
            ci_failures: Observation::Unsupported,
        };

        cache::record_provider_details_refresh(&mut cache, Ok(to_legacy_details(details)));

        assert_eq!(
            cache.details_observation_quality(),
            PrObservationQuality::PreservedStale
        );
        assert!(cache.trusted_details().is_err());

        let details = ChangeRequestDetails {
            association: None,
            comments: Observation::EmptyKnown,
            reviews: Observation::EmptyKnown,
            review_threads: Observation::EmptyKnown,
            changed_files: Observation::Known(vec!["src/lib.rs".to_string()]),
            checks: Observation::Known(Vec::new()),
            ci_failures: Observation::Unsupported,
        };
        cache::record_provider_details_refresh(&mut cache, Ok(to_legacy_details(details)));

        assert_eq!(
            cache.details_observation_quality(),
            PrObservationQuality::Fresh
        );
        assert_eq!(
            cache.trusted_details().unwrap().unwrap().files,
            ["src/lib.rs"]
        );
        assert!(
            cache
                .display_error()
                .is_some_and(|error| error.contains("CI logs unavailable"))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stale_current_details_update_display_but_remain_untrusted() {
        let mut cache = PrCache::observed(
            legacy_summary(),
            Some(PrDetails {
                comments: vec![PrComment {
                    body: "previous comment".to_string(),
                    ..PrComment::default()
                }],
                files: vec!["src/previous.rs".to_string()],
                ..PrDetails::default()
            }),
        );
        let error = crate::remote::RemoteError::new(
            ProviderKind::GitLab,
            crate::remote::RemoteOperation::ObserveChangedFiles,
            crate::remote::RemoteErrorClass::Transport,
            crate::remote::Retryability::Retryable,
            "changed files refresh failed",
        );
        let details = ChangeRequestDetails {
            association: None,
            comments: Observation::Failed(error.clone()),
            reviews: Observation::EmptyKnown,
            review_threads: Observation::EmptyKnown,
            changed_files: Observation::Stale {
                value: vec!["src/current.rs".to_string()],
                error: Some(error),
            },
            checks: Observation::EmptyKnown,
            ci_failures: Observation::Unsupported,
        };

        cache::record_provider_details_refresh(&mut cache, Ok(to_legacy_details(details)));

        let displayed = cache.details().unwrap();
        assert_eq!(displayed.files, ["src/current.rs"]);
        assert_eq!(displayed.comments[0].body, "previous comment");
        assert_eq!(
            cache.details_observation_quality(),
            PrObservationQuality::PreservedStale
        );
        assert!(
            cache
                .display_error()
                .is_some_and(|error| error.contains("changed files refresh failed"))
        );
        assert!(cache.trusted_details().is_err());
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn changed_head_is_not_published_to_the_destination_branch() {
        let directory = std::env::temp_dir().join(format!(
            "prism-fetch-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let log = directory.join("git.log");
        let mut config = crate::test_support::test_config();
        crate::test_support::install_tool(
            &mut config,
            &directory,
            "git",
            &format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$*" in
  *"remote get-url origin"*) printf '%s\n' 'https://github.com/example/repo.git'; exit 0 ;;
  *"remote get-url upstream"*) exit 2 ;;
  *"check-ref-format"*) exit 0 ;;
  *"fetch origin"*) exit 0 ;;
  *"rev-parse --verify refs/prism/change-requests/"*) printf '%s\n' 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb'; exit 0 ;;
  *"update-ref -d refs/prism/change-requests/"*) exit 0 ;;
esac
exit 1
"#,
                log.display()
            ),
        );
        let summary = legacy_summary();

        let error = fetch_change_request_branch(&directory, &config, &summary, "pr/42")
            .await
            .unwrap_err();

        assert!(error.contains("head changed"));
        let commands = std::fs::read_to_string(&log).unwrap();
        assert!(commands.contains("fetch origin +refs/pull/42/head:refs/prism/change-requests/"));
        assert!(!commands.contains("update-ref refs/heads/pr/42"));
        assert!(commands.contains("update-ref -d refs/prism/change-requests/"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn cancellation_during_fetch_still_deletes_the_temporary_ref() {
        let directory = std::env::temp_dir().join(format!(
            "prism-fetch-cancel-{}-{}",
            std::process::id(),
            crate::util::timestamp_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let log = directory.join("git.log");
        let fetch_started = directory.join("fetch-started");
        let mut config = crate::test_support::test_config();
        crate::test_support::install_tool(
            &mut config,
            &directory,
            "git",
            &format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$*" in
  *"remote get-url origin"*) printf '%s\n' 'https://github.com/example/repo.git'; exit 0 ;;
  *"remote get-url upstream"*) exit 2 ;;
  *"check-ref-format"*) exit 0 ;;
  *"fetch origin"*) touch '{}'; sleep 30; exit 0 ;;
  *"update-ref -d refs/prism/change-requests/"*) exit 0 ;;
esac
exit 1
"#,
                log.display(),
                fetch_started.display()
            ),
        );
        let token = crate::process::CancellationToken::new();
        let cancel = token.clone();
        let marker = fetch_started.clone();
        let cancellation = tokio::spawn(async move {
            while !marker.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            cancel.cancel();
        });

        let error = crate::process::with_cancellation(
            token,
            fetch_change_request_branch(&directory, &config, &legacy_summary(), "pr/42"),
        )
        .await
        .unwrap_err();
        cancellation.await.unwrap();

        assert!(crate::process::is_cancellation_error(&error), "{error}");
        let commands = std::fs::read_to_string(&log).unwrap();
        assert!(commands.contains("fetch origin"));
        assert!(commands.contains("update-ref -d refs/prism/change-requests/"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn existing_destination_branch_is_preserved() {
        let directory = std::env::temp_dir().join(format!(
            "prism-fetch-existing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let log = directory.join("git.log");
        let mut config = crate::test_support::test_config();
        crate::test_support::install_tool(
            &mut config,
            &directory,
            "git",
            &format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$*" in
  *"remote get-url origin"*) printf '%s\n' 'https://github.com/example/repo.git'; exit 0 ;;
  *"remote get-url upstream"*) exit 2 ;;
  *"check-ref-format"*) exit 0 ;;
  *"rev-parse --verify refs/heads/pr/42"*) printf '%s\n' '1111111111111111111111111111111111111111'; exit 0 ;;
esac
exit 1
"#,
                log.display()
            ),
        );

        fetch_change_request_branch(&directory, &config, &legacy_summary(), "pr/42")
            .await
            .unwrap();

        let commands = std::fs::read_to_string(&log).unwrap();
        assert!(!commands.contains("fetch origin"));
        assert!(!commands.contains("update-ref refs/heads/pr/42"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn destination_branch_race_fails_the_compare_and_swap_publication() {
        let directory = std::env::temp_dir().join(format!(
            "prism-fetch-race-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let log = directory.join("git.log");
        let mut config = crate::test_support::test_config();
        crate::test_support::install_tool(
            &mut config,
            &directory,
            "git",
            &format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$*" in
  *"remote get-url origin"*) printf '%s\n' 'https://github.com/example/repo.git'; exit 0 ;;
  *"remote get-url upstream"*) exit 2 ;;
  *"check-ref-format"*) exit 0 ;;
  *"rev-parse --verify refs/heads/pr/42"*) exit 1 ;;
  *"fetch origin"*) exit 0 ;;
  *"rev-parse --verify refs/prism/change-requests/"*) printf '%s\n' 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'; exit 0 ;;
  *"update-ref refs/heads/pr/42 "*) printf '%s\n' 'cannot lock ref: is at raced oid' >&2; exit 1 ;;
  *"update-ref -d refs/prism/change-requests/"*) exit 0 ;;
esac
exit 1
"#,
                log.display()
            ),
        );

        let error = fetch_change_request_branch(&directory, &config, &legacy_summary(), "pr/42")
            .await
            .unwrap_err();

        assert!(error.contains("update-ref"));
        let commands = std::fs::read_to_string(&log).unwrap();
        assert!(commands.contains(
            "update-ref refs/heads/pr/42 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 0000000000000000000000000000000000000000"
        ));
        assert!(commands.contains("update-ref -d refs/prism/change-requests/"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn forgejo_target_request_ref_still_requires_the_exact_observed_sha() {
        let directory = std::env::temp_dir().join(format!(
            "prism-forgejo-fork-fetch-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let log = directory.join("git.log");
        let mut config = crate::test_support::test_config();
        crate::test_support::install_tool(
            &mut config,
            &directory,
            "git",
            &format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$*" in
  *"remote get-url origin"*) printf '%s\n' 'https://codeberg.org/acme/widget.git'; exit 0 ;;
  *"remote get-url upstream"*) exit 2 ;;
  *"check-ref-format"*) exit 0 ;;
  *"fetch origin"*) exit 0 ;;
  *"rev-parse --verify refs/prism/change-requests/"*) printf '%s\n' 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb'; exit 0 ;;
  *"update-ref -d refs/prism/change-requests/"*) exit 0 ;;
esac
exit 1
"#,
                log.display()
            ),
        );
        let source = repository(ProviderKind::Forgejo, "contributor/widget");
        let target = repository(ProviderKind::Forgejo, "acme/widget");
        let mut summary = legacy_summary();
        summary.change_request_identity = Some(CanonicalChangeRequestIdentity::new(
            &target,
            &super::super::NativeChangeRequestId::new("42").unwrap(),
            &source,
            &target,
        ));

        let error = fetch_change_request_branch(&directory, &config, &summary, "pr/42")
            .await
            .unwrap_err();

        assert!(error.contains("head changed"));
        let commands = std::fs::read_to_string(&log).unwrap();
        assert!(commands.contains("fetch origin +refs/pull/42/head:refs/prism/change-requests/"));
        assert!(!commands.contains("update-ref refs/heads/pr/42"));
        assert!(commands.contains("update-ref -d refs/prism/change-requests/"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn gitlab_policy_cache_persists_only_classified_static_errors() {
        let directory = std::env::temp_dir().join(format!(
            "prism-gitlab-safe-policy-cache-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let mut config = crate::test_support::test_config();
        crate::test_support::install_tool(
            &mut config,
            &directory,
            "git",
            r#"#!/bin/sh
case "$*" in
  *"remote get-url origin"*) printf '%s\n' 'https://gitlab.com/acme/widget.git' ;;
  *"branch --show-current"*) exit 0 ;;
  *) exit 1 ;;
esac
"#,
        );
        crate::test_support::install_tool(
            &mut config,
            &directory,
            "glab",
            r#"#!/bin/sh
printf '%s\n' 'https://attacker.example/collect?access_token=query-secret'
printf '%s\n' '{"message":"malicious multiline response body"}'
printf '%s\n' 'HTTP 503 Service Unavailable' >&2
printf '%s%s\n' 'glpat-' 'direct-secret' >&2
printf '%s\n' 'Authorization: Bearer bearer-header-secret' >&2
printf '%s%s\n' 'PRIVATE-TOKEN: glpat-' 'private-header-secret' >&2
printf '%s\n' 'injected cache line' 'another injected line' >&2
exit 17
"#,
        );
        let repo =
            Repository::with_config_dir_for_test(directory.clone(), directory.join("config"));

        let cache = refresh_repository_policy(&repo, &directory, &config)
            .await
            .unwrap();
        let expected = "GitLab observe_repository_policy failed: provider; retry=retryable; status=503; exit=17; hint=backoff";
        assert!(
            cache
                .error
                .as_deref()
                .is_some_and(|error| error.contains(expected))
        );
        let persisted = crate::remote::store::load_repo_policy_cache_for_identity(
            &repo,
            &repository(ProviderKind::GitLab, "acme/widget"),
            "main",
        )
        .unwrap();
        assert_eq!(persisted.error, cache.error);
        let persisted_error = persisted.error.unwrap();
        for untrusted in [
            concat!("glpat-", "direct-secret"),
            "bearer-header-secret",
            concat!("glpat-", "private-header-secret"),
            "query-secret",
            "Authorization",
            "PRIVATE-TOKEN",
            "https://attacker.example",
            "malicious multiline response body",
            "injected cache line",
        ] {
            assert!(
                !persisted_error.contains(untrusted),
                "untrusted output was persisted: {untrusted}"
            );
        }
        std::fs::remove_dir_all(directory).unwrap();
    }
}
