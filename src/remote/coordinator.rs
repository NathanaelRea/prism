#[cfg(test)]
use std::collections::BTreeSet;
#[cfg(test)]
use std::time::Instant;

use crate::config::Config;
use crate::repo::Repository;
use crate::session::Session;
use crate::util::timestamp_label;

use super::cache::{PrCache, PrSummary};

#[cfg(test)]
pub(crate) struct PrCacheRepository<'a> {
    pub repo: &'a Repository,
    pub config: &'a Config,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PrCacheEligibility {
    pub(super) is_default_branch: bool,
    pub(super) is_detached: bool,
    pub(super) has_github_remote: bool,
}

impl PrCacheEligibility {
    async fn for_worktree(branch: &str, path: &std::path::Path, config: &Config) -> Self {
        Self {
            is_default_branch: config.is_default_branch(branch),
            is_detached: branch == "(detached)",
            has_github_remote: super::discover_git_remote(
                path,
                config,
                "origin",
                super::RemoteUrlKind::Fetch,
            )
            .await
            .is_ok(),
        }
    }

    fn for_successful_index(session: &Session, config: &Config) -> Self {
        Self {
            is_default_branch: session.is_default_branch(config),
            is_detached: session.is_detached(),
            has_github_remote: true,
        }
    }

    pub(super) fn can_observe(self) -> bool {
        !self.is_default_branch && !self.is_detached && self.has_github_remote
    }
}

pub(super) async fn cache_eligible_for_worktree(
    branch: &str,
    path: &std::path::Path,
    config: &Config,
) -> bool {
    PrCacheEligibility::for_worktree(branch, path, config)
        .await
        .can_observe()
}

pub(crate) async fn load_pr_cache_for_branch(
    repo: &Repository,
    config: &Config,
    branch: &str,
    path: &std::path::Path,
) -> PrCache {
    if config.is_default_branch(branch) || branch == "(detached)" {
        return remove_invalid_pr_cache(repo, branch);
    }
    let mut cache = super::store::load_pr_cache(repo, branch);
    if let Err(error) =
        super::discover_git_remote(path, config, "origin", super::RemoteUrlKind::Fetch).await
    {
        cache.record_remote_unavailable(error.to_string());
        super::store::persist_observation_errors(repo, branch, &mut cache);
        return cache;
    }
    if cache.summary.as_ref().is_some_and(|summary| {
        summary.head_ref != branch
            && (summary.change_request_identity.is_none() || summary.head_sha.trim().is_empty())
    }) {
        return remove_invalid_pr_cache(repo, branch);
    }
    cache
}

fn remove_invalid_pr_cache(repo: &Repository, branch: &str) -> PrCache {
    let mut cache = PrCache::default();
    cache.record_summary_observation(None, timestamp_label());
    cache.record_persistence_result(super::store::remove_pr_cache(repo, branch));
    cache
}

pub(super) fn pr_summary_matches_worktree(
    summary: &PrSummary,
    source_branch: &str,
    known_summary: Option<&PrSummary>,
    origin_push: Option<&crate::remote::RemoteRepositoryId>,
    local_head: Option<&str>,
) -> bool {
    let cached_canonical_association = known_summary.is_some_and(|known| {
        known.change_request_identity.is_some()
            && known.change_request_identity == summary.change_request_identity
    });
    let initial_canonical_association = summary.head_ref == source_branch
        && local_head == Some(summary.head_sha.as_str())
        && summary
            .change_request_identity
            .as_ref()
            .and_then(|identity| identity.source_repository().ok())
            .as_ref()
            == origin_push;
    if !summary.merged && summary.state.eq_ignore_ascii_case("open") {
        return cached_canonical_association || initial_canonical_association;
    }
    if !summary.merged
        && !matches!(
            summary.state.trim().to_ascii_uppercase().as_str(),
            "OPEN" | "CLOSED" | "MERGED"
        )
    {
        return cached_canonical_association || initial_canonical_association;
    }
    summary.merged && (cached_canonical_association || initial_canonical_association)
}

pub(crate) async fn resolve_pr_summary_for_session(
    session: &Session,
    config: &Config,
    summaries: &[PrSummary],
) -> Option<PrSummary> {
    if !PrCacheEligibility::for_successful_index(session, config).can_observe() {
        return None;
    }
    let source_push = super::dispatcher::prepare_push(&session.path, config, &session.branch)
        .await
        .ok();
    let known_summary = session
        .pr
        .summary_observed_in_process
        .then_some(session.pr.summary.as_ref())
        .flatten();
    summaries
        .iter()
        .find(|summary| {
            pr_summary_matches_worktree(
                summary,
                source_push
                    .as_ref()
                    .map(|guard| guard.remote_branch.as_str())
                    .unwrap_or(session.branch.as_str()),
                known_summary,
                source_push.as_ref().map(|guard| &guard.repository),
                source_push
                    .as_ref()
                    .map(|guard| guard.expected_head_sha.as_str()),
            )
        })
        .cloned()
}

pub(crate) fn pr_cache_pollable_for_session(session: &Session, config: &Config) -> bool {
    PrCache::structurally_eligible(&session.branch, config, session.hidden)
        && !session
            .pr
            .summary
            .as_ref()
            .is_some_and(|summary| summary.merged)
}

pub(crate) fn pr_details_pollable(session: &Session, config: &Config) -> bool {
    pr_cache_pollable_for_session(session, config) && super::cache::pr_details_due(&session.pr)
}

#[cfg(test)]
pub(crate) async fn refresh_pr_summary_index_for_sessions(
    repos: &[PrCacheRepository<'_>],
    sessions: &mut [Session],
    repo_index: usize,
    summaries: Vec<PrSummary>,
    poll_started_at: Instant,
) {
    let targets = (0..sessions.len()).collect::<BTreeSet<_>>();
    refresh_pr_summary_index_for_target_sessions(
        repos,
        sessions,
        repo_index,
        &targets,
        summaries,
        poll_started_at,
    )
    .await;
}

#[cfg(test)]
pub(crate) async fn refresh_pr_summary_index_for_target_sessions(
    repos: &[PrCacheRepository<'_>],
    sessions: &mut [Session],
    repo_index: usize,
    targets: &BTreeSet<usize>,
    summaries: Vec<PrSummary>,
    poll_started_at: Instant,
) {
    let Some(managed) = repos.get(repo_index) else {
        return;
    };
    let refreshed = timestamp_label();
    for (_, session) in sessions.iter_mut().enumerate().filter(|(index, session)| {
        targets.contains(index) && session.repo_index == repo_index && !session.hidden
    }) {
        if !session.pr.finish_summary_poll(poll_started_at) {
            continue;
        }
        let summary = resolve_pr_summary_for_session(session, managed.config, &summaries).await;
        let mutation = session
            .pr
            .record_summary_observation(summary, refreshed.clone());
        super::store::persist_pr_summary_mutation(
            managed.repo,
            &session.branch,
            &mut session.pr,
            mutation,
        );
    }
}
