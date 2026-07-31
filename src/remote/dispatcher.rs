use std::path::Path;
use std::process::Command;

use crate::config::Config;
use crate::repo::Repository;

use super::forgejo::ForgejoAdapter;
use super::github::{
    self, CiFailure as LegacyCiFailure, PrCache, PrCheckContext, PrCheckState, PrComment,
    PrDetails, PrReview, PrReviewComment, PrSummary, ProviderDetailsObservation, RepoPolicyCache,
};
use super::gitlab::GitLabAdapter;
use super::{
    CanonicalChangeRequestIdentity, Capabilities, ChangeRequest, ChangeRequestDetails,
    ChangeRequestSummary, CheckState, CreateChangeRequest, DiscoveredRemote, GuardedMerge,
    HostIdentity, LifecycleState, MergeMethod, MergeabilityState, NativeReviewThreadId,
    Observation, ProviderKind, QueueState, RemoteRepositoryId, RemoteUrlKind, ResolveReviewThread,
    ReviewDecision, SupportLevel, discover_git_remote,
};

enum Adapter {
    GitHub,
    GitLab(GitLabAdapter),
    Forgejo(Box<ForgejoAdapter>),
}

impl Adapter {
    fn resolve(path: &Path, config: &Config) -> Result<(Self, DiscoveredRemote), String> {
        let discovered = discover_git_remote(path, config, "origin", RemoteUrlKind::Fetch)
            .map_err(|error| error.to_string())?;
        let adapter = match discovered.repository.id.provider() {
            ProviderKind::GitHub => Self::GitHub,
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
            Self::GitHub => Capabilities::for_provider(ProviderKind::GitHub),
            Self::GitLab(_) => GitLabAdapter::capabilities(),
            Self::Forgejo(adapter) => adapter.capabilities(),
        }
    }

    fn for_repository(config: &Config, repository: &RemoteRepositoryId) -> Result<Self, String> {
        match repository.provider() {
            ProviderKind::GitHub => Ok(Self::GitHub),
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
}

pub(crate) fn configured(path: &Path, config: &Config) -> bool {
    Adapter::resolve(path, config).is_ok()
}

pub(crate) fn provider(path: &Path, config: &Config) -> Result<ProviderKind, String> {
    Adapter::resolve(path, config).map(|(_, remote)| remote.repository.id.provider())
}

pub(crate) fn repository_project(
    path: &Path,
    config: &Config,
    remote_name: &str,
) -> Result<String, String> {
    discover_git_remote(path, config, remote_name, RemoteUrlKind::Fetch)
        .map(|remote| remote.repository.id.project_path().to_string())
        .map_err(|error| error.to_string())
}

pub(crate) fn fetch_change_request_branch(
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
    validate_git_ref(path, config, &destination_ref)?;
    let mut configured = Vec::new();
    for remote_name in ["origin", "upstream"] {
        if let Ok(remote) = discover_git_remote(path, config, remote_name, RemoteUrlKind::Fetch) {
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
    validate_git_ref(path, config, &fetch.remote_ref)?;

    let temporary_ref = format!(
        "refs/prism/change-requests/{:016x}",
        identity.stable_hash()
            ^ crate::util::stable_hash(Path::new(&summary.head_sha))
            ^ crate::util::stable_hash(Path::new(branch))
    );
    let refspec = format!("+{}:{temporary_ref}", fetch.remote_ref);
    crate::process::run_status_named(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(path)
            .args(["fetch", fetch.remote_name])
            .arg(refspec),
        crate::process::ProcessPolicy::NetworkQuery,
        crate::process::ProcessDescriptor::new("git.fetch"),
    )?;

    let publish = (|| {
        let fetched_sha = crate::process::run_capture_named(
            Command::new(config.tool("git"))
                .arg("-C")
                .arg(path)
                .args(["rev-parse", "--verify"])
                .arg(format!("{temporary_ref}^{{commit}}")),
            crate::process::ProcessPolicy::Metadata,
            crate::process::ProcessDescriptor::new("git.rev_parse"),
        )?;
        if fetched_sha.trim() != summary.head_sha {
            return Err("change request head changed while it was being fetched".to_string());
        }
        crate::process::run_status_named(
            Command::new(config.tool("git")).arg("-C").arg(path).args([
                "update-ref",
                &destination_ref,
                &summary.head_sha,
            ]),
            crate::process::ProcessPolicy::LocalMutation,
            crate::process::ProcessDescriptor::new("git.update_ref"),
        )
    })();
    let cleanup = crate::process::run_status_named(
        Command::new(config.tool("git")).arg("-C").arg(path).args([
            "update-ref",
            "-d",
            &temporary_ref,
        ]),
        crate::process::ProcessPolicy::LocalMutation,
        crate::process::ProcessDescriptor::new("git.update_ref"),
    );
    publish.and(cleanup)
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
    Err(
        "no configured fetch remote matches the change request source or target repository"
            .to_string(),
    )
}

fn validate_git_ref(path: &Path, config: &Config, reference: &str) -> Result<(), String> {
    crate::process::run_status_named(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(path)
            .args(["check-ref-format", reference]),
        crate::process::ProcessPolicy::Metadata,
        crate::process::ProcessDescriptor::new("git.check_ref_format"),
    )
}

pub(crate) fn submit_review(
    path: &Path,
    config: &Config,
    display_number: u64,
    flag: &str,
    body: &str,
) -> Result<(), String> {
    if provider(path, config)? != ProviderKind::GitHub {
        return Err(
            "review submission is not supported by the selected provider adapter".to_string(),
        );
    }
    if !matches!(flag, "--approve" | "--comment" | "--request-changes") {
        return Err("review submission type is invalid".to_string());
    }
    let mut command = Command::new(config.tool("gh"));
    command
        .arg("pr")
        .arg("review")
        .arg(display_number.to_string())
        .arg(flag)
        .current_dir(path);
    if !body.trim().is_empty() {
        command.arg("--body").arg(body.trim());
    }
    let output = crate::process::run_output_allow_failure_named(
        &mut command,
        crate::process::ProcessPolicy::NetworkQuery,
        crate::process::ProcessDescriptor::new("gh.pr.review"),
    )?;
    if output.status.success() {
        Ok(())
    } else if output.stderr.trim().is_empty() {
        Err(format!("gh pr review exited with {}", output.status))
    } else {
        Err(output.stderr.trim().to_string())
    }
}

pub(crate) fn capabilities(path: &Path, config: &Config) -> Result<Capabilities, String> {
    Adapter::resolve(path, config).map(|(adapter, _)| adapter.capabilities())
}

pub(crate) fn authentication_status(path: &Path, config: &Config) -> Result<String, String> {
    let (adapter, remote) = Adapter::resolve(path, config)?;
    match adapter {
        Adapter::GitHub => crate::process::run_capture_named(
            Command::new(config.tool("gh"))
                .arg("auth")
                .arg("status")
                .arg("--hostname")
                .arg(remote.repository.id.host().hostname()),
            crate::process::ProcessPolicy::NetworkQuery,
            crate::process::ProcessDescriptor::new("gh.auth.status"),
        )
        .map(|_| "ok".to_string()),
        Adapter::GitLab(_) => crate::process::run_capture_named(
            Command::new(config.tool("glab"))
                .arg("auth")
                .arg("status")
                .arg("--hostname")
                .arg(remote.repository.id.host().hostname()),
            crate::process::ProcessPolicy::NetworkQuery,
            crate::process::ProcessDescriptor::new("glab.auth.status"),
        )
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

pub(crate) fn server_version(path: &Path, config: &Config) -> Result<Option<String>, String> {
    let (adapter, _) = Adapter::resolve(path, config)?;
    match adapter {
        Adapter::Forgejo(adapter) => adapter
            .discover_instance()
            .map(|instance| Some(instance.version))
            .map_err(|error| error.to_string()),
        Adapter::GitHub | Adapter::GitLab(_) => Ok(None),
    }
}

pub(crate) fn capabilities_for_summary(summary: &PrSummary) -> Capabilities {
    summary
        .change_request_identity
        .as_ref()
        .map(|identity| Capabilities::for_provider(identity.provider()))
        // Legacy GitHub cache rows intentionally remain usable after migration.
        .unwrap_or_else(|| Capabilities::for_provider(ProviderKind::GitHub))
}

pub(crate) fn list_change_requests(path: &Path, config: &Config) -> Result<Vec<PrSummary>, String> {
    let (adapter, remote) = Adapter::resolve(path, config)?;
    match adapter {
        Adapter::GitHub => github::fetch_pr_summary_index(path, config),
        Adapter::GitLab(adapter) => adapter
            .list_change_requests()
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(to_legacy_summary)
            .collect(),
        Adapter::Forgejo(adapter) => adapter
            .list_change_requests(&remote.repository.id)
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(to_legacy_summary)
            .collect(),
    }
}

pub(crate) fn refresh_change_request_cache(
    repo: &Repository,
    branch: &str,
    cache: &mut PrCache,
    path: &Path,
    config: &Config,
    force_details: bool,
) -> Result<(), String> {
    let (adapter, source_remote) = Adapter::resolve(path, config)?;
    if matches!(adapter, Adapter::GitHub) {
        return github::refresh_pr_cache(repo, branch, cache, path, config, force_details);
    }

    let observation = if let Some(summary) = cache.summary()
        && let Ok(change_request) = change_request_from_legacy(summary)
    {
        if change_request.source_repository != source_remote.repository.id {
            Err("change request source repository no longer matches origin".to_string())
        } else {
            let target_adapter = Adapter::for_repository(config, change_request.id.repository())?;
            let observed = match target_adapter {
                Adapter::GitLab(adapter) => adapter.observe_change_request(&change_request.id),
                Adapter::Forgejo(adapter) => adapter.change_request_summary(&change_request.id),
                Adapter::GitHub => unreachable!(),
            };
            observed
                .map(to_legacy_summary)
                .map_err(|error| error.to_string())
                .and_then(|summary| summary)
                .map(|summary| {
                    (summary.merged || summary.state.eq_ignore_ascii_case("OPEN"))
                        .then_some(summary)
                })
        }
    } else {
        list_change_requests(path, config).map(|summaries| {
            summaries.into_iter().find(|summary| {
                summary.head_ref == branch
                    && !summary.merged
                    && summary.state.eq_ignore_ascii_case("OPEN")
                    && summary
                        .change_request_identity
                        .as_ref()
                        .and_then(|identity| identity.source_repository().ok())
                        .as_ref()
                        == Some(&source_remote.repository.id)
            })
        })
    };
    github::record_provider_summary_refresh(repo, branch, cache, observation)?;
    if force_details && cache.summary().is_some() {
        refresh_change_request_details_state(branch, cache, path, config);
        github::persist_pr_cache_snapshot(repo, branch, cache)?;
    }
    Ok(())
}

pub(crate) fn refresh_change_request_details_state(
    branch: &str,
    cache: &mut PrCache,
    path: &Path,
    config: &Config,
) {
    let result = (|| {
        let (origin_adapter, _) = Adapter::resolve(path, config)?;
        if matches!(origin_adapter, Adapter::GitHub) {
            github::refresh_pr_details_cache_state(branch, cache, path, config);
            return Ok(None);
        }
        let summary = cache
            .summary()
            .ok_or_else(|| "change request summary is not loaded".to_string())?;
        let change_request = change_request_from_legacy(summary)?;
        let adapter = Adapter::for_repository(config, change_request.id.repository())?;
        let details = match adapter {
            Adapter::GitLab(adapter) => adapter
                .change_request_details(&change_request)
                .map_err(|error| error.to_string())?,
            Adapter::Forgejo(adapter) => adapter
                .change_request_details(&change_request.id)
                .map_err(|error| error.to_string())?,
            Adapter::GitHub => unreachable!(),
        };
        if !details.association.as_ref().is_some_and(|association| {
            association.matches(&change_request.id, &change_request.head_sha)
        }) {
            return Err("change request head changed while details were loaded".to_string());
        }
        Ok(Some(details))
    })();
    match result {
        Ok(None) => {}
        Ok(Some(details)) => {
            github::record_provider_details_refresh(cache, Ok(to_legacy_details(details)))
        }
        Err(error) => github::record_provider_details_refresh(cache, Err(error)),
    }
}

pub(crate) fn refresh_repository_policy(
    repo: &Repository,
    path: &Path,
    config: &Config,
) -> Result<RepoPolicyCache, String> {
    refresh_repository_policy_for(repo, path, config, None)
}

pub(crate) fn refresh_repository_policy_for(
    repo: &Repository,
    path: &Path,
    config: &Config,
    target_repository: Option<&RemoteRepositoryId>,
) -> Result<RepoPolicyCache, String> {
    let (origin_adapter, remote) = Adapter::resolve(path, config)?;
    let repository = target_repository
        .cloned()
        .unwrap_or_else(|| remote.repository.id.clone());
    let adapter = if target_repository.is_some() {
        Adapter::for_repository(config, &repository)?
    } else {
        origin_adapter
    };
    if matches!(adapter, Adapter::GitHub) {
        return github::refresh_repo_policy_cache(repo, path, config);
    }
    let target = config.default_base.as_deref().unwrap_or("main");
    let policy = match adapter {
        Adapter::GitLab(adapter) => adapter.repository_policy(target),
        Adapter::Forgejo(adapter) => adapter.repository_policy(&repository, target),
        Adapter::GitHub => unreachable!(),
    };
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
    match policy {
        Ok(policy) => {
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
        Err(error) => cache.error = Some(error.to_string()),
    }
    github::save_repo_policy_cache(repo, &cache)?;
    Ok(cache)
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

pub(crate) fn create_change_request(
    repo: &Repository,
    config: &Config,
    branch: &str,
    path: &Path,
    body: &str,
    target_project: Option<&str>,
    cache: &mut PrCache,
) -> Result<(), String> {
    let (adapter, remote) = Adapter::resolve(path, config)?;
    if matches!(adapter, Adapter::GitHub) {
        github::run_create_pull_request(config, path, body, target_project)?;
        let source = remote.repository.id;
        let target = match target_project {
            Some(project) => {
                RemoteRepositoryId::new(ProviderKind::GitHub, source.host().clone(), project)
                    .map_err(|error| error.to_string())?
            }
            None => source.clone(),
        };
        let summary = github::fetch_pr_summary_index_for_repository(path, config, &target)?
            .into_iter()
            .find(|summary| {
                summary.head_ref == branch
                    && summary
                        .change_request_identity
                        .as_ref()
                        .and_then(|identity| identity.source_repository().ok())
                        .as_ref()
                        == Some(&source)
            })
            .ok_or_else(|| "created pull request was not returned by GitHub".to_string())?;
        github::record_pr_summary(repo, branch, cache, summary);
        if target == source {
            github::refresh_pr_cache(repo, branch, cache, path, config, true)?;
        }
        return Ok(());
    }
    let head_sha = crate::git::current_head_sha(path, config)?;
    let source = remote.repository.id;
    let target = match target_project
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(project) => RemoteRepositoryId::new(source.provider(), source.host().clone(), project)
            .map_err(|error| error.to_string())?,
        None => source.clone(),
    };
    let request = CreateChangeRequest {
        source_repository: source,
        target_repository: target.clone(),
        source_branch: branch.to_string(),
        target_branch: config
            .default_base
            .clone()
            .unwrap_or_else(|| "main".to_string()),
        expected_head_sha: head_sha,
        title: branch.replace(['-', '_'], " "),
        body: body.to_string(),
        draft: false,
    };
    let summary = match adapter {
        Adapter::GitLab(_) => GitLabAdapter::new(config, target)
            .map_err(|error| error.to_string())?
            .create_change_request(&request)
            .map_err(|error| error.to_string())?,
        Adapter::Forgejo(adapter) => adapter
            .create_change_request(request)
            .map_err(|error| error.to_string())?,
        Adapter::GitHub => unreachable!(),
    };
    github::record_pr_summary(repo, branch, cache, to_legacy_summary(summary)?);
    refresh_change_request_cache(repo, branch, cache, path, config, true)
}

pub(crate) fn merge_change_request(
    config: &Config,
    path: &Path,
    authorized_identity: &CanonicalChangeRequestIdentity,
    display_number: u64,
    expected_head_sha: &str,
) -> Result<(), String> {
    let (_, source_remote) = Adapter::resolve(path, config)?;
    let authorized_id = authorized_identity
        .change_request_id(Some(display_number))
        .map_err(|error| error.to_string())?;
    let authorized_source = authorized_identity
        .source_repository()
        .map_err(|error| error.to_string())?;
    let authorized_target = authorized_identity
        .target_repository()
        .map_err(|error| error.to_string())?;
    if source_remote.repository.id != authorized_source
        || authorized_id.repository() != &authorized_target
    {
        return Err("change request repository changed since authorization".to_string());
    }
    if authorized_target != authorized_source {
        let upstream = discover_git_remote(path, config, "upstream", RemoteUrlKind::Fetch)
            .map_err(|_| "authorized target repository is no longer configured as upstream")?;
        if upstream.repository.id != authorized_target {
            return Err("change request target repository changed since authorization".to_string());
        }
    }
    let adapter = Adapter::for_repository(config, &authorized_target)?;
    if matches!(adapter, Adapter::GitHub) {
        let observed =
            github::fetch_pr_summary_index_for_repository(path, config, &authorized_target)?
                .into_iter()
                .find(|summary| {
                    summary.change_request_identity.as_ref() == Some(authorized_identity)
                        && summary.number == display_number
                })
                .ok_or_else(|| "authorized change request is no longer present".to_string())?;
        if observed.head_sha != expected_head_sha {
            return Err("change request head changed since authorization".to_string());
        }
        let target_project =
            (authorized_target != authorized_source).then_some(authorized_target.project_path());
        return github::merge_pull_request(
            config,
            path,
            display_number,
            expected_head_sha,
            target_project,
        );
    }
    let summary = match &adapter {
        Adapter::GitLab(adapter) => adapter
            .observe_change_request(&authorized_id)
            .map_err(|error| error.to_string())?,
        Adapter::Forgejo(adapter) => adapter
            .change_request_summary(&authorized_id)
            .map_err(|error| error.to_string())?,
        Adapter::GitHub => unreachable!(),
    };
    if summary.change_request.id != authorized_id {
        return Err("provider returned a different change request identity".to_string());
    }
    if summary.change_request.head_sha != expected_head_sha {
        return Err("change request head changed since authorization".to_string());
    }
    let request = GuardedMerge {
        id: summary.change_request.id,
        target_repository: summary.change_request.target_repository,
        target_branch: summary.change_request.target_branch,
        expected_source_sha: expected_head_sha.to_string(),
        method: match config.merge_method {
            crate::config::MergeMethod::Merge => MergeMethod::Merge,
            crate::config::MergeMethod::Squash => MergeMethod::Squash,
            crate::config::MergeMethod::Rebase => MergeMethod::Rebase,
        },
        native_guard: None,
    };
    match adapter {
        Adapter::GitLab(adapter) => adapter
            .merge_change_request(&request)
            .map(|_| ())
            .map_err(|error| error.to_string()),
        Adapter::Forgejo(adapter) => adapter
            .merge_change_request(request)
            .map(|_| ())
            .map_err(|error| error.to_string()),
        Adapter::GitHub => unreachable!(),
    }
}

pub(crate) fn resolve_review_thread(
    path: &Path,
    config: &Config,
    summary: &PrSummary,
    thread_id: &str,
) -> Result<(), String> {
    let change_request = change_request_from_legacy(summary)?;
    let (_, source_remote) = Adapter::resolve(path, config)?;
    if source_remote.repository.id != change_request.source_repository {
        return Err(
            "change request source repository changed before thread resolution".to_string(),
        );
    }
    if change_request.target_repository != change_request.source_repository {
        let upstream = discover_git_remote(path, config, "upstream", RemoteUrlKind::Fetch)
            .map_err(|_| "change request target repository is no longer configured as upstream")?;
        if upstream.repository.id != change_request.target_repository {
            return Err(
                "change request target repository changed before thread resolution".to_string(),
            );
        }
    }
    let adapter = Adapter::for_repository(config, change_request.id.repository())?;
    if matches!(adapter, Adapter::GitHub) {
        return github::resolve_review_thread(path, config, thread_id);
    }
    let request = ResolveReviewThread {
        id: change_request.id,
        thread_id: NativeReviewThreadId::new(thread_id.to_string())
            .map_err(|error| error.to_string())?,
        expected_head_sha: summary.head_sha.clone(),
    };
    match adapter {
        Adapter::GitLab(adapter) => adapter
            .resolve_review_thread(&request)
            .map(|_| ())
            .map_err(|error| error.to_string()),
        Adapter::Forgejo(adapter) => adapter
            .resolve_review_thread(request)
            .map_err(|error| error.to_string()),
        Adapter::GitHub => unreachable!(),
    }
}

pub(crate) fn wait_for_change_request_merged(
    path: &Path,
    display_number: u64,
    config: &Config,
) -> Result<bool, String> {
    let (adapter, remote) = Adapter::resolve(path, config)?;
    if matches!(adapter, Adapter::GitHub) {
        return github::wait_for_pr_merged(path, display_number, config);
    }
    let summary = provider_summaries(&adapter, &remote)?
        .into_iter()
        .find(|summary| summary.change_request.id.display_number() == Some(display_number));
    Ok(summary.is_some_and(|summary| matches!(summary.lifecycle, LifecycleState::Merged)))
}

fn provider_summaries(
    adapter: &Adapter,
    remote: &DiscoveredRemote,
) -> Result<Vec<ChangeRequestSummary>, String> {
    match adapter {
        Adapter::GitLab(adapter) => adapter
            .list_change_requests()
            .map_err(|error| error.to_string()),
        Adapter::Forgejo(adapter) => adapter
            .list_change_requests(&remote.repository.id)
            .map_err(|error| error.to_string()),
        Adapter::GitHub => Err("GitHub summaries use the compatibility adapter".to_string()),
    }
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
        title: summary.title,
        author: summary.author,
        body: summary.body,
        url: summary.web_url.unwrap_or_default(),
        state: lifecycle_label(&summary.lifecycle).to_string(),
        review_decision: review_label(&summary.review_decision).to_string(),
        requested_reviewers: Vec::new(),
        head_ref: request.source_branch,
        base_ref: request.target_branch,
        head_sha: request.head_sha,
        updated_at: summary.updated_at.unwrap_or_default(),
        check_status: check_label(&summary.check_state).to_string(),
        merge_state_status: mergeability_label(&summary.mergeability).to_string(),
        comment_count: 0,
        merged: matches!(summary.lifecycle, LifecycleState::Merged),
        draft: summary.draft,
    })
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

fn to_legacy_details(details: ChangeRequestDetails) -> ProviderDetailsObservation {
    let comments = known_vec(details.comments, "comments").map(|comments| {
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
    let reviews = known_vec(details.reviews, "reviews").map(|reviews| {
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
    let review_comments = known_vec(details.review_threads, "review threads").map(|threads| {
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
    let check_contexts = known_vec(details.checks, "checks").map(|checks| {
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
    let ci_failures = known_vec(details.ci_failures, "CI logs").map(|failures| {
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
        files: known_vec(details.changed_files, "changed files"),
        failing_checks,
        check_contexts,
        ci_failures,
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
        CheckState::Pending => "pending",
        CheckState::Passed => "success",
        CheckState::Failed => "failure",
        CheckState::Cancelled => "cancelled",
        CheckState::Skipped => "skipped",
        CheckState::Mixed => "mixed",
        CheckState::Unknown(native) => native,
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

    #[test]
    fn fork_fetch_uses_the_configured_target_request_ref() {
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

    #[test]
    fn forgejo_fetch_uses_the_canonical_source_branch() {
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

    #[test]
    fn fetch_rejects_unconfigured_source_and_target_repositories() {
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
    #[test]
    fn changed_head_is_not_published_to_the_destination_branch() {
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
        let summary = PrSummary {
            number: 42,
            change_request_identity: Some(crate::remote::test_change_request_identity()),
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
            comment_count: 0,
            merged: false,
            draft: false,
        };

        let error =
            fetch_change_request_branch(&directory, &config, &summary, "pr/42").unwrap_err();

        assert!(error.contains("head changed"));
        let commands = std::fs::read_to_string(&log).unwrap();
        assert!(commands.contains("fetch origin +refs/pull/42/head:refs/prism/change-requests/"));
        assert!(!commands.contains("update-ref refs/heads/pr/42"));
        assert!(commands.contains("update-ref -d refs/prism/change-requests/"));
        std::fs::remove_dir_all(directory).unwrap();
    }
}
