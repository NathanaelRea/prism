use std::path::Path;
use std::process::Command;
use std::time::Duration;

use crate::config::Config;
use crate::repo::Repository;

#[cfg(test)]
use super::HostIdentity;
use super::forgejo::ForgejoAdapter;
use super::github::{
    self, CiFailure as LegacyCiFailure, PrCache, PrCheckContext, PrCheckState, PrComment, PrReview,
    PrReviewComment, PrSummary, ProviderDetailsObservation, RepoPolicyCache,
};
use super::gitlab::GitLabAdapter;
use super::{
    CanonicalChangeRequestIdentity, Capabilities, ChangeRequest, ChangeRequestDetails,
    ChangeRequestSummary, CheckState, CreateChangeRequest, DiscoveredRemote, GuardedMerge,
    LifecycleState, MergeMethod, MergeMutationResult, MergeabilityState, NativeReviewThreadId,
    Observation, ProviderKind, QueueState, RemoteRepositoryId, RemoteUrlKind, ResolveReviewThread,
    ReviewDecision, discover_git_remote,
};

const MERGE_VERIFY_ATTEMPTS: usize = 6;
const MERGE_VERIFY_INTERVAL: Duration = Duration::from_millis(500);

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
            Self::GitLab(adapter) => adapter.capabilities(),
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
    let destination_old_oid = read_git_ref_or_zero(path, config, &destination_ref)?;
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
                &destination_old_oid,
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

fn read_git_ref_or_zero(path: &Path, config: &Config, reference: &str) -> Result<String, String> {
    let output = crate::process::run_output_allow_failure_named(
        Command::new(config.tool("git")).arg("-C").arg(path).args([
            "rev-parse",
            "--verify",
            reference,
        ]),
        crate::process::ProcessPolicy::Metadata,
        crate::process::ProcessDescriptor::new("git.rev_parse"),
    )?;
    if !output.status.success() {
        return Ok("0000000000000000000000000000000000000000".to_string());
    }
    let oid = output.stdout.trim();
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
    summary: &PrSummary,
    flag: &str,
    body: &str,
) -> Result<(), String> {
    let change_request = change_request_from_legacy(summary)?;
    let target = &change_request.target_repository;
    if target.provider() != ProviderKind::GitHub {
        return Err(
            "review submission is not supported by the selected provider adapter".to_string(),
        );
    }
    configured_remote_repositories(path, config)?
        .validate_target_repository(target)
        .map_err(|_| "change request target changed before review submission".to_string())?;
    if !matches!(flag, "--approve" | "--comment" | "--request-changes") {
        return Err("review submission type is invalid".to_string());
    }
    let canonical_repository = format!("{}/{}", target.host(), target.project_path());
    let mut command = Command::new(config.tool("gh"));
    command
        .arg("pr")
        .arg("review")
        .arg(summary.number.to_string())
        .arg("--repo")
        .arg(canonical_repository)
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
    let (adapter, _) = Adapter::resolve(path, config)?;
    if let Adapter::Forgejo(adapter) = &adapter {
        adapter
            .discover_instance()
            .map_err(|error| error.to_string())?;
    }
    Ok(adapter.capabilities())
}

pub(crate) fn authentication_status(path: &Path, config: &Config) -> Result<String, String> {
    let (adapter, remote) = Adapter::resolve(path, config)?;
    match adapter {
        Adapter::GitHub => crate::process::run_capture_named(
            Command::new(config.tool("gh"))
                .arg("auth")
                .arg("status")
                .arg("--hostname")
                .arg(remote.repository.id.host().to_string()),
            crate::process::ProcessPolicy::NetworkQuery,
            crate::process::ProcessDescriptor::new("gh.auth.status"),
        )
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

pub(crate) struct RemoteRuntimeDiagnostics {
    pub(crate) capabilities: Capabilities,
    pub(crate) server_version: Option<String>,
}

pub(crate) fn runtime_diagnostics(
    path: &Path,
    config: &Config,
) -> Result<RemoteRuntimeDiagnostics, String> {
    let (adapter, remote) = Adapter::resolve(path, config)?;
    match adapter {
        Adapter::Forgejo(adapter) => {
            let diagnostics = adapter
                .runtime_diagnostics(&remote.repository.id)
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
        // Legacy GitHub cache rows intentionally remain usable after migration.
        .unwrap_or_else(|| Capabilities::for_provider(ProviderKind::GitHub))
}

pub(crate) fn list_change_requests(path: &Path, config: &Config) -> Result<Vec<PrSummary>, String> {
    let repositories = configured_change_request_repositories(path, config)?;
    let mut summaries = Vec::new();
    for repository in repositories {
        let adapter = Adapter::for_repository(config, &repository)?;
        let observed = match adapter {
            Adapter::GitHub => {
                github::fetch_pr_summary_index_for_repository(path, config, &repository)?
            }
            Adapter::GitLab(adapter) => adapter
                .list_change_requests()
                .map_err(|error| error.to_string())?
                .into_iter()
                .map(to_legacy_summary)
                .collect::<Result<Vec<_>, _>>()?,
            Adapter::Forgejo(adapter) => adapter
                .list_change_requests(&repository)
                .map_err(|error| error.to_string())?
                .into_iter()
                .map(to_legacy_summary)
                .collect::<Result<Vec<_>, _>>()?,
        };
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

fn configured_change_request_repositories(
    path: &Path,
    config: &Config,
) -> Result<Vec<RemoteRepositoryId>, String> {
    Ok(configured_remote_repositories(path, config)?.fetch_repositories)
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
}

fn configured_remote_repositories(
    path: &Path,
    config: &Config,
) -> Result<ConfiguredRemoteRepositories, String> {
    let origin_fetch = discover_git_remote(path, config, "origin", RemoteUrlKind::Fetch)
        .map_err(|error| error.to_string())?
        .repository
        .id;
    let origin_push = discover_git_remote(path, config, "origin", RemoteUrlKind::Push)
        .map_err(|error| error.to_string())?
        .repository
        .id;
    if origin_push.provider() != origin_fetch.provider() {
        return Err("origin fetch and push repositories use different providers".to_string());
    }

    let upstream_fetch = discover_git_remote(path, config, "upstream", RemoteUrlKind::Fetch)
        .ok()
        .map(|remote| remote.repository.id)
        .filter(|repository| repository.provider() == origin_fetch.provider());
    let upstream_push = discover_git_remote(path, config, "upstream", RemoteUrlKind::Push)
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

fn push_unique_repository(
    repositories: &mut Vec<RemoteRepositoryId>,
    repository: RemoteRepositoryId,
) {
    if !repositories.contains(&repository) {
        repositories.push(repository);
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
    let remotes = configured_remote_repositories(path, config)?;
    let has_distinct_upstream = remotes
        .upstream_fetch
        .as_ref()
        .is_some_and(|repository| repository != &remotes.origin_fetch)
        || remotes
            .upstream_push
            .as_ref()
            .is_some_and(|repository| repository != &remotes.origin_push)
        || remotes.origin_push != remotes.origin_fetch;
    if remotes.origin_fetch.provider() == ProviderKind::GitHub && !has_distinct_upstream {
        return github::refresh_pr_cache(repo, branch, cache, path, config, force_details);
    }
    let observation = if let Some(summary) = cache.summary()
        && let Ok(change_request) = change_request_from_legacy(summary)
    {
        if let Err(error) = remotes.validate_target_repository(&change_request.target_repository) {
            Err(error)
        } else {
            let target_adapter = Adapter::for_repository(config, change_request.id.repository())?;
            match target_adapter {
                Adapter::GitHub => github::fetch_pr_summary_index_for_repository(
                    path,
                    config,
                    change_request.id.repository(),
                )
                .and_then(|summaries| {
                    let observed = summaries.into_iter().find(|summary| {
                        summary.change_request_identity.as_ref()
                            == cache
                                .summary()
                                .and_then(|summary| summary.change_request_identity.as_ref())
                    });
                    observed.map_or(Ok(None), authoritative_active_summary)
                }),
                Adapter::GitLab(adapter) => adapter
                    .observe_change_request(&change_request.id)
                    .map(to_legacy_summary)
                    .map_err(|error| error.to_string())
                    .and_then(|summary| summary)
                    .and_then(authoritative_active_summary),
                Adapter::Forgejo(adapter) => adapter
                    .change_request_summary(&change_request.id)
                    .map(to_legacy_summary)
                    .map_err(|error| error.to_string())
                    .and_then(|summary| summary)
                    .and_then(authoritative_active_summary),
            }
        }
    } else {
        let local_head = crate::git::current_head_sha(path, config)?;
        list_change_requests(path, config).map(|summaries| {
            let matching = summaries.into_iter().filter(|summary| {
                summary.head_ref == branch
                    && summary.head_sha == local_head
                    && summary
                        .change_request_identity
                        .as_ref()
                        .is_some_and(|identity| {
                            identity.source_repository().ok().as_ref() == Some(&remotes.origin_push)
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
    github::record_provider_summary_refresh(repo, branch, cache, observation)?;
    if force_details && cache.summary().is_some() {
        refresh_change_request_details_state(branch, cache, path, config);
        github::persist_pr_cache_snapshot(repo, branch, cache)?;
    }
    Ok(())
}

pub(crate) fn refresh_change_request_details_state(
    _branch: &str,
    cache: &mut PrCache,
    path: &Path,
    config: &Config,
) {
    let result = (|| {
        let summary = cache
            .summary()
            .cloned()
            .ok_or_else(|| "change request summary is not loaded".to_string())?;
        let change_request = change_request_from_legacy(&summary)?;
        configured_remote_repositories(path, config)?
            .validate_target_repository(&change_request.target_repository)?;
        let adapter = Adapter::for_repository(config, change_request.id.repository())?;
        let details = match adapter {
            Adapter::GitHub => {
                let observed = github::fetch_pr_summary_index_for_repository(
                    path,
                    config,
                    change_request.id.repository(),
                )?
                .into_iter()
                .find(|observed| {
                    observed.number == summary.number
                        && observed.change_request_identity == summary.change_request_identity
                })
                .ok_or_else(|| {
                    "canonical change request was not returned while details were loaded"
                        .to_string()
                })?;
                if observed.head_sha != summary.head_sha {
                    return Err("change request head changed while details were loaded".to_string());
                }
                github::fetch_pr_details_for_repository_number(
                    path,
                    config,
                    change_request.id.repository(),
                    summary.number,
                    &change_request.source_branch,
                    &summary.head_sha,
                )?
            }
            Adapter::GitLab(adapter) => adapter
                .change_request_details(&change_request)
                .map_err(|error| error.to_string())
                .and_then(|details| {
                    if !details.association.as_ref().is_some_and(|association| {
                        association.matches(&change_request.id, &change_request.head_sha)
                    }) {
                        return Err(
                            "change request head changed while details were loaded".to_string()
                        );
                    }
                    Ok(to_legacy_details(details))
                })?,
            Adapter::Forgejo(adapter) => adapter
                .change_request_details(&change_request)
                .map_err(|error| error.to_string())
                .and_then(|details| {
                    if !details.association.as_ref().is_some_and(|association| {
                        association.matches(&change_request.id, &change_request.head_sha)
                    }) {
                        return Err(
                            "change request head changed while details were loaded".to_string()
                        );
                    }
                    Ok(to_legacy_details(details))
                })?,
        };
        Ok(details)
    })();
    match result {
        Ok(details) => github::record_provider_details_refresh(cache, Ok(details)),
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
    let observed_target = observed_policy_target_branch(repo, path, config, &repository);
    let target = observed_target
        .as_deref()
        .or(config.default_base.as_deref())
        .unwrap_or("main");
    if matches!(adapter, Adapter::GitHub) {
        return github::refresh_repo_policy_cache_for_repository(
            repo,
            path,
            config,
            &repository,
            target,
        );
    }
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
                github::load_repo_policy_cache_for_identity(repo, &repository, target)
            {
                stale.error = Some(error.to_string());
                cache = stale;
            } else {
                cache.identity_complete = false;
                cache.error = Some(error.to_string());
            }
        }
    }
    github::save_repo_policy_cache(repo, &cache)?;
    Ok(cache)
}

fn observed_policy_target_branch(
    repo: &Repository,
    path: &Path,
    config: &Config,
    repository: &RemoteRepositoryId,
) -> Option<String> {
    let branch = crate::git::current_branch_name(path, config)
        .ok()
        .flatten()?;
    let cache = github::load_pr_cache(repo, &branch);
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

pub(crate) fn create_change_request(
    repo: &Repository,
    config: &Config,
    branch: &str,
    path: &Path,
    body: &str,
    target_project: Option<&str>,
    cache: &mut PrCache,
) -> Result<(), String> {
    let remotes = configured_remote_repositories(path, config)?;
    let source = remotes.origin_push.clone();
    let target = remotes.create_target(target_project)?;
    remotes.validate_source_mutation(&source, &target)?;
    let adapter = Adapter::for_repository(config, &target)?;
    if matches!(adapter, Adapter::GitHub) {
        github::run_create_pull_request(config, path, body, Some(target.project_path()))?;
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
        return refresh_change_request_cache(repo, branch, cache, path, config, true);
    }
    let head_sha = crate::git::current_head_sha(path, config)?;
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
) -> Result<MergeMutationResult, String> {
    let remotes = configured_remote_repositories(path, config)?;
    let authorized_id = authorized_identity
        .change_request_id(Some(display_number))
        .map_err(|error| error.to_string())?;
    let authorized_source = authorized_identity
        .source_repository()
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
    let adapter = Adapter::for_repository(config, &authorized_target)?;
    let capabilities = adapter.capabilities();
    if capabilities.guarded_merge == super::SupportLevel::Unsupported {
        return Err(capabilities.guarded_merge_reason.unwrap_or_else(|| {
            "guarded merge is unsupported by the provider adapter".to_string()
        }));
    }
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
        if !observed.state.eq_ignore_ascii_case("OPEN") || observed.merged {
            return Err("change request lifecycle is not authoritatively open".to_string());
        }
        let target_project =
            (authorized_target != authorized_source).then_some(authorized_target.project_path());
        github::merge_pull_request(
            config,
            path,
            display_number,
            expected_head_sha,
            target_project,
        )?;
        let observed =
            github::fetch_pr_summary_index_for_repository(path, config, &authorized_target)?
                .into_iter()
                .find(|summary| {
                    summary.change_request_identity.as_ref() == Some(authorized_identity)
                        && summary.number == display_number
                })
                .ok_or_else(|| "mutated change request was not returned by GitHub".to_string())?;
        if observed.head_sha != expected_head_sha {
            return Err("change request head changed during merge".to_string());
        }
        let native_state = observed.state.clone();
        return Ok(MergeMutationResult::from_summary(
            change_request_summary_from_legacy(observed)?,
            native_state,
        ));
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
    match adapter {
        Adapter::GitLab(adapter) => adapter
            .merge_change_request(&request)
            .map_err(|error| error.to_string()),
        Adapter::Forgejo(adapter) => adapter
            .merge_change_request(request)
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
    configured_remote_repositories(path, config)?
        .validate_target_repository(&change_request.target_repository)
        .map_err(|_| "change request repository changed before thread resolution".to_string())?;
    let adapter = Adapter::for_repository(config, change_request.id.repository())?;
    if matches!(adapter, Adapter::GitHub) {
        return github::resolve_review_thread(
            path,
            config,
            change_request.id.repository().host(),
            thread_id,
        );
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
    expected: &ChangeRequest,
    config: &Config,
) -> Result<ChangeRequestSummary, String> {
    let mut last_summary = None;
    let mut last_error = None;
    for attempt in 0..MERGE_VERIFY_ATTEMPTS {
        match observe_exact_change_request(path, expected, config) {
            Ok(summary) if summary.lifecycle == LifecycleState::Merged => return Ok(summary),
            Ok(summary) => {
                last_summary = Some(summary);
                last_error = None;
            }
            Err(error) => last_error = Some(error),
        }
        if attempt + 1 < MERGE_VERIFY_ATTEMPTS {
            std::thread::sleep(MERGE_VERIFY_INTERVAL);
        }
    }
    last_summary.ok_or_else(|| {
        last_error
            .unwrap_or_else(|| "change request could not be reobserved after merge".to_string())
    })
}

fn observe_exact_change_request(
    path: &Path,
    expected: &ChangeRequest,
    config: &Config,
) -> Result<ChangeRequestSummary, String> {
    let adapter = Adapter::for_repository(config, expected.id.repository())?;
    let observed = match adapter {
        Adapter::GitHub => {
            let identity = CanonicalChangeRequestIdentity::new(
                expected.id.repository(),
                expected.id.native_id(),
                &expected.source_repository,
                &expected.target_repository,
            );
            let summary = github::fetch_pr_summary_index_for_repository(
                path,
                config,
                expected.id.repository(),
            )?
            .into_iter()
            .find(|summary| {
                summary.change_request_identity.as_ref() == Some(&identity)
                    && expected.id.display_number() == Some(summary.number)
            })
            .ok_or_else(|| "canonical change request was not returned by GitHub".to_string())?;
            change_request_summary_from_legacy(summary)?
        }
        Adapter::GitLab(adapter) => adapter
            .observe_change_request(&expected.id)
            .map_err(|error| error.to_string())?,
        Adapter::Forgejo(adapter) => adapter
            .change_request_summary(&expected.id)
            .map_err(|error| error.to_string())?,
    };
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
        comment_count: 0,
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
    github::record_pr_summary(repo, branch, cache, to_legacy_summary(summary)?);
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

    #[cfg(unix)]
    #[test]
    fn upstream_github_review_uses_the_canonical_target_repository() {
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
            &format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n", log.display()),
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

        submit_review(&directory, &config, &summary, "--approve", "looks good").unwrap();

        assert_eq!(
            std::fs::read_to_string(&log).unwrap().trim(),
            "pr review 42 --repo github.com/acme/widget --approve --body looks good"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn cached_github_details_use_canonical_target_number_not_origin_branch() {
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
  *"api graphql"*"owner=acme"*)
    printf '%s\n' '{{"data":{{"repository":{{"pullRequests":{{"nodes":[{{"id":"PR_42","number":42,"title":"Fork change","state":"OPEN","headRefName":"topic","baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRepository":{{"nameWithOwner":"contributor/widget"}},"baseRepository":{{"nameWithOwner":"acme/widget"}}}}],"pageInfo":{{"hasNextPage":false}}}}}}}}}}'
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
        );

        let commands = std::fs::read_to_string(&log).unwrap();
        assert!(commands.contains("owner=acme"));
        assert!(commands.contains("/repos/acme/widget/issues/42/comments?per_page=100"));
        assert!(!commands.contains("pr view synthetic-local-branch"));
        std::fs::remove_dir_all(directory).unwrap();
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
    fn forgejo_fork_fetch_uses_the_configured_target_request_ref_without_source_remote() {
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
    fn change_request_discovery_includes_distinct_origin_and_upstream_identities() {
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

        let repositories = configured_change_request_repositories(&directory, &config).unwrap();

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
    #[test]
    fn triangular_remote_identities_are_independent_deduplicated_and_guard_mutations() {
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

        let remotes = configured_remote_repositories(&directory, &config).unwrap();
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
            configured_change_request_repositories(&directory, &config).unwrap(),
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
    #[test]
    fn triangular_create_uses_origin_push_source_and_explicit_fetch_target() {
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
  *"remote get-url origin --push"*) printf '%s\n' 'https://github.com/contributor/widget.git' ;;
  *"remote get-url origin"*) printf '%s\n' 'https://github.com/acme/widget.git' ;;
  *"remote get-url upstream"*) exit 2 ;;
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
  *) printf '%s\n' '{{"data":{{"repository":{{"pullRequests":{{"nodes":[{{"id":"PR_fork","number":42,"title":"Fork change","state":"OPEN","merged":false,"headRefName":"topic","baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRepository":{{"nameWithOwner":"contributor/widget"}},"baseRepository":{{"nameWithOwner":"acme/widget"}}}}],"pageInfo":{{"hasNextPage":false}}}}}}}}}}' ;;
esac
"#,
                gh_log.display()
            ),
        );
        let repo =
            Repository::with_config_dir_for_test(directory.clone(), directory.join("config"));
        let mut cache = PrCache::default();

        create_change_request(
            &repo, &config, "topic", &directory, "body", None, &mut cache,
        )
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
        assert!(commands.contains("pr create --fill --body body --repo acme/widget"));
        let commands = std::fs::read_to_string(&git_log).unwrap();
        assert!(commands.contains("remote get-url origin"));
        assert!(commands.contains("remote get-url origin --push"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn triangular_polling_associates_the_origin_push_repository_as_source() {
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
  *"remote get-url origin --push"*) printf '%s\n' 'https://github.com/contributor/widget.git' ;;
  *"remote get-url upstream --push"*) printf '%s\n' 'https://github.com/contributor/widget.git' ;;
  *"remote get-url origin"*) printf '%s\n' 'https://github.com/acme/widget.git' ;;
  *"remote get-url upstream"*) printf '%s\n' 'https://github.com/acme/widget.git' ;;
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
printf '%s\n' '{{"data":{{"repository":{{"pullRequests":{{"nodes":[{{"id":"PR_fork","number":42,"title":"Fork change","state":"OPEN","merged":false,"headRefName":"topic","baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRepository":{{"nameWithOwner":"contributor/widget"}},"baseRepository":{{"nameWithOwner":"acme/widget"}}}}],"pageInfo":{{"hasNextPage":false}}}}}}}}}}'
"#,
                gh_log.display()
            ),
        );
        let repo =
            Repository::with_config_dir_for_test(directory.clone(), directory.join("config"));
        let mut cache = PrCache::default();

        refresh_change_request_cache(&repo, "topic", &mut cache, &directory, &config, false)
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
            std::fs::read_to_string(&gh_log)
                .unwrap()
                .matches("api graphql")
                .count(),
            1
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn maintainer_target_checkout_can_merge_a_fork_change_request() {
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
    printf '%s\n' '{{"data":{{"repository":{{"pullRequests":{{"nodes":[{{"id":"PR_stale","number":42,"title":"Fork change","state":"OPEN","headRefName":"topic","baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRepository":{{"nameWithOwner":"former-contributor/widget"}},"baseRepository":{{"nameWithOwner":"acme/widget"}}}}],"pageInfo":{{"hasNextPage":false}}}}}}}}}}'
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
    #[test]
    fn maintainer_target_checkout_can_resolve_a_fork_review_thread() {
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
printf '%s\n' '{"data":{"resolveReviewThread":{"thread":{"id":"PRRT_1","isResolved":true}}}}'
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

        resolve_review_thread(&directory, &config, &summary, "PRRT_1").unwrap();

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn unknown_lifecycle_is_not_converted_to_authoritative_absence() {
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

    #[test]
    fn compatibility_conversion_preserves_unknown_native_queue_state() {
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

    #[cfg(unix)]
    #[test]
    fn merge_verification_observes_the_canonical_fork_target() {
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
printf '%s\n' '{{"data":{{"repository":{{"pullRequests":{{"nodes":[{{"id":"PR_fork","number":42,"title":"Fork change","state":"MERGED","merged":true,"headRefName":"topic","baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRepository":{{"nameWithOwner":"contributor/widget"}},"baseRepository":{{"nameWithOwner":"acme/widget"}}}}],"pageInfo":{{"hasNextPage":false}}}}}}}}}}'
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

        let observed = wait_for_change_request_merged(&directory, &expected, &config).unwrap();

        assert_eq!(observed.lifecycle, LifecycleState::Merged);
        let commands = std::fs::read_to_string(&log).unwrap();
        assert!(commands.contains("owner=acme"), "{commands}");
        assert!(commands.contains("name=widget"), "{commands}");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn unavailable_ci_logs_do_not_invalidate_other_legacy_details() {
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

        github::record_provider_details_refresh(&mut cache, Ok(to_legacy_details(details)));

        assert_eq!(
            cache.details_observation_quality(),
            github::PrObservationQuality::PreservedStale
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
        github::record_provider_details_refresh(&mut cache, Ok(to_legacy_details(details)));

        assert_eq!(
            cache.details_observation_quality(),
            github::PrObservationQuality::Fresh
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

    #[test]
    fn stale_current_details_update_display_but_remain_untrusted() {
        let mut cache = PrCache::observed(
            legacy_summary(),
            Some(github::PrDetails {
                comments: vec![PrComment {
                    body: "previous comment".to_string(),
                    ..PrComment::default()
                }],
                files: vec!["src/previous.rs".to_string()],
                ..github::PrDetails::default()
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

        github::record_provider_details_refresh(&mut cache, Ok(to_legacy_details(details)));

        let displayed = cache.details().unwrap();
        assert_eq!(displayed.files, ["src/current.rs"]);
        assert_eq!(displayed.comments[0].body, "previous comment");
        assert_eq!(
            cache.details_observation_quality(),
            github::PrObservationQuality::PreservedStale
        );
        assert!(
            cache
                .display_error()
                .is_some_and(|error| error.contains("changed files refresh failed"))
        );
        assert!(cache.trusted_details().is_err());
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
        let summary = legacy_summary();

        let error =
            fetch_change_request_branch(&directory, &config, &summary, "pr/42").unwrap_err();

        assert!(error.contains("head changed"));
        let commands = std::fs::read_to_string(&log).unwrap();
        assert!(commands.contains("fetch origin +refs/pull/42/head:refs/prism/change-requests/"));
        assert!(!commands.contains("update-ref refs/heads/pr/42"));
        assert!(commands.contains("update-ref -d refs/prism/change-requests/"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn existing_destination_branch_is_published_with_its_observed_old_oid() {
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
  *"fetch origin"*) exit 0 ;;
  *"rev-parse --verify refs/prism/change-requests/"*) printf '%s\n' 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'; exit 0 ;;
  *"update-ref refs/heads/pr/42 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1111111111111111111111111111111111111111"*) exit 0 ;;
  *"update-ref -d refs/prism/change-requests/"*) exit 0 ;;
esac
exit 1
"#,
                log.display()
            ),
        );

        fetch_change_request_branch(&directory, &config, &legacy_summary(), "pr/42").unwrap();

        let commands = std::fs::read_to_string(&log).unwrap();
        assert!(commands.contains(
            "update-ref refs/heads/pr/42 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1111111111111111111111111111111111111111"
        ));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn destination_branch_race_fails_the_compare_and_swap_publication() {
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
  *"rev-parse --verify refs/heads/pr/42"*) printf '%s\n' '1111111111111111111111111111111111111111'; exit 0 ;;
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
            .unwrap_err();

        assert!(error.contains("update-ref"));
        let commands = std::fs::read_to_string(&log).unwrap();
        assert!(commands.contains(
            "update-ref refs/heads/pr/42 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1111111111111111111111111111111111111111"
        ));
        assert!(commands.contains("update-ref -d refs/prism/change-requests/"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn forgejo_target_request_ref_still_requires_the_exact_observed_sha() {
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

        let error =
            fetch_change_request_branch(&directory, &config, &summary, "pr/42").unwrap_err();

        assert!(error.contains("head changed"));
        let commands = std::fs::read_to_string(&log).unwrap();
        assert!(commands.contains("fetch origin +refs/pull/42/head:refs/prism/change-requests/"));
        assert!(!commands.contains("update-ref refs/heads/pr/42"));
        assert!(commands.contains("update-ref -d refs/prism/change-requests/"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn gitlab_policy_cache_persists_only_classified_static_errors() {
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
printf '%s\n' 'glpat-direct-secret' >&2
printf '%s\n' 'Authorization: Bearer bearer-header-secret' >&2
printf '%s\n' 'PRIVATE-TOKEN: glpat-private-header-secret' >&2
printf '%s\n' 'injected cache line' 'another injected line' >&2
exit 17
"#,
        );
        let repo =
            Repository::with_config_dir_for_test(directory.clone(), directory.join("config"));

        let cache = refresh_repository_policy(&repo, &directory, &config).unwrap();
        let expected = "GitLab observe_repository_policy failed: provider; retry=retryable; status=503; exit=17; hint=backoff";
        assert!(
            cache
                .error
                .as_deref()
                .is_some_and(|error| error.contains(expected))
        );
        let persisted = github::load_repo_policy_cache_for_repository(
            &repo,
            &repository(ProviderKind::GitLab, "acme/widget"),
        )
        .unwrap();
        assert_eq!(persisted.error, cache.error);
        let persisted_error = persisted.error.unwrap();
        for untrusted in [
            "glpat-direct-secret",
            "bearer-header-secret",
            "glpat-private-header-secret",
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
