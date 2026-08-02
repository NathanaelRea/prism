mod adapter;

pub(super) use adapter::GitHubAdapter;

#[cfg(test)]
mod tests;

use std::process::Command;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::config::Config;
use crate::config::MergeMethod;
use crate::process::{
    ProcessDescriptor, ProcessPolicy, run_capture_named, run_output_allow_failure_named,
};
use crate::repo::Repository;
use crate::util::{strip_ansi, timestamp_label};

use super::cache::*;
use super::{
    Capabilities, ChangeRequest, ChangeRequestDetails, ChangeRequestId, ChangeRequestSummary,
    CheckContext, CheckState, Comment, CreateChangeRequest, GuardedMerge, LifecycleState,
    MergeMutationResult, MergeabilityState, NativeReviewThreadId, Observation, PolicyFacts,
    ProviderKind, QueueState, RemoteError, RemoteErrorClass, RemoteOperation, RemoteRepositoryId,
    RepositoryPolicy, ResolveReviewThread, RetryHint, Retryability, Review, ReviewDecision,
    ReviewSubmissionKind, ReviewThread, SubmitReview,
};

const PR_MERGE_VERIFY_ATTEMPTS: usize = 6;
const PR_MERGE_VERIFY_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Default, Deserialize)]
struct GithubPrSummaryIndexResponse {
    data: GithubPrSummaryIndexData,
}

#[derive(Debug, Default, Deserialize)]
struct GithubPrSummaryIndexData {
    repository: GithubRepository,
}

#[derive(Debug, Default, Deserialize)]
struct GithubRepository {
    #[serde(default, rename = "pullRequests")]
    pull_requests: GithubPullRequestConnection,
    #[serde(default, rename = "pullRequest")]
    pull_request: GithubPullRequest,
}

#[derive(Debug, Default, Deserialize)]
struct GithubPageInfo {
    #[serde(default, rename = "hasNextPage")]
    has_next_page: bool,
}

#[derive(Debug, Default, Deserialize)]
struct GithubPullRequestConnection {
    #[serde(default)]
    nodes: Vec<GithubPullRequest>,
    #[serde(default, rename = "pageInfo")]
    page_info: GithubPageInfo,
}

#[derive(Debug, Default, Deserialize)]
struct GithubPullRequest {
    #[serde(default)]
    id: String,
    number: Option<u64>,
    #[serde(default)]
    title: String,
    #[serde(default)]
    author: GithubLogin,
    #[serde(default)]
    body: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    state: String,
    #[serde(default, rename = "reviewDecision")]
    review_decision: Option<String>,
    #[serde(default, rename = "reviewRequests")]
    review_requests: GithubReviewRequests,
    #[serde(default, rename = "headRefName")]
    head_ref_name: String,
    #[serde(default, rename = "baseRefName")]
    base_ref_name: String,
    #[serde(default, rename = "headRefOid")]
    head_ref_oid: String,
    #[serde(default, rename = "headRepository")]
    head_repository: GithubRepositoryIdentity,
    #[serde(default, rename = "baseRepository")]
    base_repository: GithubRepositoryIdentity,
    #[serde(default, rename = "updatedAt")]
    updated_at: String,
    #[serde(default)]
    comments: GithubCount,
    #[serde(default, rename = "reviewThreads")]
    review_threads: GithubReviewThreadConnection,
    #[serde(default)]
    commits: GithubCommitConnection,
    #[serde(
        default,
        rename = "statusCheckRollup",
        deserialize_with = "deserialize_status_rollup"
    )]
    status_check_rollup: GithubStatusCheckRollup,
    #[serde(default, rename = "mergeStateStatus")]
    merge_state_status: String,
    #[serde(
        default,
        rename = "mergeQueueEntry",
        deserialize_with = "deserialize_merge_queue_entry"
    )]
    merge_queue_entry: GithubMergeQueueObservation,
    #[serde(default)]
    merged: Option<bool>,
    #[serde(default, rename = "mergedAt")]
    merged_at: Option<String>,
    #[serde(default, rename = "isDraft")]
    is_draft: bool,
}

#[derive(Debug, Default, Deserialize)]
struct GithubMergeQueueEntry {
    #[serde(default)]
    state: String,
}

#[derive(Debug, Default)]
enum GithubMergeQueueObservation {
    #[default]
    NotObserved,
    NotQueued,
    Entry(GithubMergeQueueEntry),
}

fn deserialize_merge_queue_entry<'de, D>(
    deserializer: D,
) -> Result<GithubMergeQueueObservation, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<GithubMergeQueueEntry>::deserialize(deserializer)?
        .map(GithubMergeQueueObservation::Entry)
        .unwrap_or(GithubMergeQueueObservation::NotQueued))
}

#[derive(Debug, Default, Deserialize)]
struct GithubRepositoryIdentity {
    #[serde(default, rename = "nameWithOwner")]
    name_with_owner: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(untagged)]
enum GithubReviewRequests {
    Connection {
        nodes: Vec<GithubReviewRequest>,
    },
    List(Vec<GithubReviewRequest>),
    #[default]
    Missing,
}

impl GithubReviewRequests {
    fn nodes(&self) -> &[GithubReviewRequest] {
        match self {
            Self::Connection { nodes } | Self::List(nodes) => nodes,
            Self::Missing => &[],
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct GithubReviewRequest {
    #[serde(default, rename = "requestedReviewer")]
    requested_reviewer: GithubReviewer,
}

#[derive(Debug, Default, Deserialize)]
struct GithubReviewer {
    login: Option<String>,
    slug: Option<String>,
    name: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct GithubCount {
    #[serde(default, rename = "totalCount")]
    total_count: u64,
}

#[derive(Debug, Default, Deserialize)]
struct GithubReviewThreadConnection {
    #[serde(default, rename = "totalCount")]
    total_count: u64,
    #[serde(default, rename = "pageInfo")]
    page_info: GithubPageInfo,
    #[serde(default)]
    nodes: Vec<GithubReviewThread>,
}

#[derive(Debug, Default, Deserialize)]
struct GithubReviewThread {
    #[serde(default)]
    id: String,
    #[serde(default, rename = "isResolved")]
    is_resolved: bool,
    #[serde(default)]
    comments: GithubReviewThreadCommentConnection,
}

#[derive(Debug, Default, Deserialize)]
struct GithubReviewThreadCommentConnection {
    #[serde(default, rename = "totalCount")]
    total_count: u64,
    #[serde(default, rename = "pageInfo")]
    page_info: GithubPageInfo,
    #[serde(default)]
    nodes: Vec<GithubReviewThreadComment>,
}

#[derive(Debug, Default, Deserialize)]
struct GithubReviewThreadComment {
    #[serde(default)]
    id: String,
    #[serde(default)]
    author: GithubLogin,
    #[serde(default)]
    path: String,
    line: Option<u64>,
    #[serde(default, rename = "originalLine")]
    original_line: Option<u64>,
    #[serde(default)]
    body: String,
    #[serde(default, rename = "createdAt", alias = "created_at")]
    created_at: String,
}

#[derive(Debug, Default, Deserialize)]
struct GithubLogin {
    #[serde(default)]
    login: String,
}

#[derive(Debug, Default, Deserialize)]
struct GhPrViewDetails {
    #[serde(default)]
    comments: Vec<GhPrComment>,
    #[serde(default)]
    reviews: Vec<GhPrReview>,
    #[serde(default)]
    files: Vec<GhPrFile>,
    #[serde(
        default,
        rename = "statusCheckRollup",
        deserialize_with = "deserialize_status_rollup"
    )]
    status_check_rollup: GithubStatusCheckRollup,
}

#[derive(Debug, Default, Deserialize)]
struct GhPrComment {
    #[serde(default, deserialize_with = "deserialize_github_rest_id")]
    id: String,
    #[serde(default)]
    author: GhActor,
    #[serde(default)]
    user: GhActor,
    #[serde(default)]
    body: String,
    #[serde(default, rename = "createdAt", alias = "created_at")]
    created_at: String,
}

#[derive(Debug, Default, Deserialize)]
struct GhPrReview {
    #[serde(default, deserialize_with = "deserialize_github_rest_id")]
    id: String,
    #[serde(default)]
    author: GhActor,
    #[serde(default)]
    user: GhActor,
    #[serde(default)]
    state: String,
    #[serde(default)]
    body: String,
    #[serde(default, rename = "submittedAt", alias = "submitted_at")]
    submitted_at: String,
}

fn deserialize_github_rest_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum GithubRestId {
        String(String),
        Number(u64),
    }

    Ok(match GithubRestId::deserialize(deserializer)? {
        GithubRestId::String(id) => id,
        GithubRestId::Number(id) => id.to_string(),
    })
}

#[derive(Debug, Default, Deserialize)]
struct GhActor {
    #[serde(default)]
    login: String,
}

#[derive(Debug, Default, Deserialize)]
struct GhPrFile {
    #[serde(default, alias = "filename")]
    path: String,
}

#[derive(Debug, Default, Deserialize)]
struct GhCheckRunsPage {
    #[serde(default)]
    total_count: u64,
    #[serde(default)]
    check_runs: Vec<GithubStatusContext>,
}

#[derive(Debug, Default, Deserialize)]
struct GhRunListItem {
    #[serde(default, rename = "databaseId", alias = "id")]
    database_id: u64,
    #[serde(default, rename = "workflowName")]
    workflow_name: String,
    #[serde(default, rename = "displayTitle", alias = "display_title")]
    display_title: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    conclusion: String,
    #[serde(default)]
    status: String,
    #[serde(default, rename = "headSha", alias = "head_sha")]
    head_sha: String,
    #[serde(default, alias = "html_url")]
    url: String,
}

#[derive(Debug, Default, Deserialize)]
struct GhWorkflowRunsPage {
    #[serde(default)]
    total_count: u64,
    #[serde(default)]
    workflow_runs: Vec<GhRunListItem>,
}

#[derive(Debug, Default, Deserialize)]
struct GithubCommitConnection {
    #[serde(default)]
    nodes: Vec<GithubCommitNode>,
}

#[derive(Debug, Default, Deserialize)]
struct GithubCommitNode {
    #[serde(default)]
    commit: GithubCommit,
}

#[derive(Debug, Default, Deserialize)]
struct GithubCommit {
    #[serde(
        default,
        rename = "statusCheckRollup",
        deserialize_with = "deserialize_status_rollup"
    )]
    status_check_rollup: GithubStatusCheckRollup,
}

#[derive(Debug, Default, Deserialize)]
struct GithubStatusCheckRollup {
    #[serde(skip)]
    observed: bool,
    #[serde(default)]
    contexts: GithubStatusContextConnection,
    #[serde(default)]
    nodes: Vec<GithubStatusContext>,
}

fn deserialize_status_rollup<'de, D>(deserializer: D) -> Result<GithubStatusCheckRollup, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    if value.is_null() {
        return Ok(GithubStatusCheckRollup {
            observed: true,
            ..GithubStatusCheckRollup::default()
        });
    }
    if let Ok(nodes) = serde_json::from_value::<Vec<GithubStatusContext>>(value.clone()) {
        if nodes.len() >= 100 {
            return Err(serde::de::Error::custom(
                "check rollup contexts reached the unpaginated gh pr view limit",
            ));
        }
        return Ok(GithubStatusCheckRollup {
            observed: true,
            contexts: GithubStatusContextConnection::default(),
            nodes,
        });
    }
    let contexts = value
        .get("contexts")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| serde::de::Error::custom("check rollup is missing contexts"))?;
    if !contexts
        .get("nodes")
        .is_some_and(serde_json::Value::is_array)
    {
        return Err(serde::de::Error::custom(
            "check rollup contexts are missing nodes",
        ));
    }
    if contexts
        .get("pageInfo")
        .and_then(|page_info| page_info.get("hasNextPage"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return Err(serde::de::Error::custom(
            "check rollup contexts are truncated",
        ));
    }
    let mut rollup = serde_json::from_value::<GithubStatusCheckRollup>(value)
        .map_err(serde::de::Error::custom)?;
    rollup.observed = true;
    Ok(rollup)
}

#[derive(Debug, Default, Deserialize)]
struct GithubStatusContextConnection {
    #[serde(default)]
    nodes: Vec<GithubStatusContext>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct GithubStatusContext {
    name: Option<String>,
    context: Option<String>,
    status: Option<String>,
    conclusion: Option<String>,
    state: Option<String>,
}

pub fn refresh_pr_cache(
    repo: &Repository,
    branch: &str,
    cache: &mut PrCache,
    path: &std::path::Path,
    config: &Config,
    force_details: bool,
) -> Result<(), String> {
    let started_at = Instant::now();
    cache.begin_summary_poll(started_at);
    if !super::coordinator::cache_eligible_for_worktree(branch, path, config) {
        cache.finish_summary_poll(started_at);
        let mutation = cache.record_summary_observation(None, timestamp_label());
        super::store::persist_pr_summary_mutation(repo, branch, cache, mutation);
        return cache.refresh_result();
    }
    let source_push = super::dispatcher::prepare_push(path, config, branch).ok();
    let source_branch = source_push
        .as_ref()
        .map(|guard| guard.remote_branch.as_str())
        .unwrap_or(branch);
    let result = fetch_pr_summary(path, source_branch, config).map(|observation| {
        observation.filter(|(summary, _)| {
            super::coordinator::pr_summary_matches_worktree(
                summary,
                source_branch,
                cache
                    .summary_observed_in_process
                    .then_some(cache.summary.as_ref())
                    .flatten(),
                source_push.as_ref().map(|guard| &guard.repository),
                source_push
                    .as_ref()
                    .map(|guard| guard.expected_head_sha.as_str()),
            )
        })
    });
    match result {
        Ok(Some((summary, _raw))) => {
            if !cache.finish_summary_poll(started_at) {
                return Err("pull request summary refresh was superseded".to_string());
            }
            let mutation = cache.record_summary_observation(Some(summary), timestamp_label());
            if force_details || pr_details_due(cache) {
                let details_result = refresh_pr_details_cache(repo, branch, cache, path, config);
                super::store::persist_pr_summary_mutation(repo, branch, cache, mutation);
                details_result?;
            } else {
                super::store::persist_pr_summary_mutation(repo, branch, cache, mutation);
            }
        }
        Ok(None) => {
            if !cache.finish_summary_poll(started_at) {
                return Err("pull request summary refresh was superseded".to_string());
            }
            let mutation = cache.record_summary_observation(None, timestamp_label());
            super::store::persist_pr_summary_mutation(repo, branch, cache, mutation);
        }
        Err(error) => {
            if !cache.finish_summary_poll(started_at) {
                return Err("pull request summary refresh was superseded".to_string());
            }
            cache.record_summary_failure(error);
            super::store::persist_observation_errors(repo, branch, cache);
        }
    }
    cache.refresh_result()
}

pub fn wait_for_pr_merged(
    path: &std::path::Path,
    pr_number: u64,
    config: &Config,
) -> Result<bool, String> {
    let mut last_error = None;
    for attempt in 0..PR_MERGE_VERIFY_ATTEMPTS {
        match fetch_pr_merged_status(path, pr_number, config) {
            Ok(true) => return Ok(true),
            Ok(false) => last_error = None,
            Err(error) => last_error = Some(error),
        }
        if attempt + 1 < PR_MERGE_VERIFY_ATTEMPTS {
            std::thread::sleep(PR_MERGE_VERIFY_INTERVAL);
        }
    }
    match last_error {
        Some(error) => Err(error),
        None => Ok(false),
    }
}

pub(crate) fn create_pull_request(
    repo: &Repository,
    config: &Config,
    branch: &str,
    path: &std::path::Path,
    body: &str,
    target_repo: Option<&str>,
    cache: &mut PrCache,
) -> Result<(), String> {
    run_create_pull_request(
        config,
        path,
        body,
        target_repo,
        config.default_base.as_deref(),
        None,
    )?;
    refresh_pr_cache(repo, branch, cache, path, config, true)
}

pub(super) fn run_create_pull_request(
    config: &Config,
    path: &std::path::Path,
    body: &str,
    target_repo: Option<&str>,
    target_base: Option<&str>,
    source_head: Option<&str>,
) -> Result<(), String> {
    run_capture_named(
        Command::new(config.tool("gh"))
            .args(create_pr_args(target_base, body, target_repo, source_head))
            .current_dir(path),
        ProcessPolicy::NetworkQuery,
        ProcessDescriptor::new("gh.pr.create"),
    )?;
    Ok(())
}

pub(crate) fn merge_pull_request(
    config: &Config,
    path: &std::path::Path,
    pr_number: u64,
    expected_head_sha: &str,
    target_repo: Option<&str>,
) -> Result<(), String> {
    run_capture_named(
        Command::new(config.tool("gh"))
            .args(merge_pr_args(
                &pr_number.to_string(),
                config.merge_method,
                expected_head_sha,
                target_repo,
            ))
            .current_dir(path),
        ProcessPolicy::NetworkQuery,
        ProcessDescriptor::new("gh.pr.merge"),
    )?;
    Ok(())
}

fn create_pr_args(
    default_base: Option<&str>,
    body: &str,
    target_repo: Option<&str>,
    source_head: Option<&str>,
) -> Vec<String> {
    let mut args = vec![
        "pr".to_string(),
        "create".to_string(),
        "--fill".to_string(),
        "--body".to_string(),
        body.to_string(),
    ];
    if let Some(repo) = target_repo.map(str::trim).filter(|repo| !repo.is_empty()) {
        args.push("--repo".to_string());
        args.push(repo.to_string());
    }
    if let Some(base) = default_base.map(str::trim).filter(|base| !base.is_empty()) {
        args.push("--base".to_string());
        args.push(base.to_string());
    }
    if let Some(head) = source_head.map(str::trim).filter(|head| !head.is_empty()) {
        args.push("--head".to_string());
        args.push(head.to_string());
    }
    args
}

fn merge_pr_args(
    pr_number: &str,
    method: MergeMethod,
    expected_head_sha: &str,
    target_repo: Option<&str>,
) -> Vec<String> {
    let mut args = vec![
        "pr".to_string(),
        "merge".to_string(),
        pr_number.to_string(),
        method.gh_flag().to_string(),
        "--match-head-commit".to_string(),
        expected_head_sha.to_string(),
    ];
    if let Some(target_repo) = target_repo {
        args.push("--repo".to_string());
        args.push(target_repo.to_string());
    }
    args
}

fn fetch_pr_merged_status(
    path: &std::path::Path,
    pr_number: u64,
    config: &Config,
) -> Result<bool, String> {
    let output = run_output_allow_failure_named(
        Command::new(config.tool("gh"))
            .arg("pr")
            .arg("view")
            .arg(pr_number.to_string())
            .arg("--json")
            .arg("state,mergedAt")
            .current_dir(path),
        ProcessPolicy::NetworkQuery,
        ProcessDescriptor::new("gh.pr.view"),
    )?;
    if !output.status.success() {
        let stderr = output.stderr.trim().to_string();
        let message = if stderr.is_empty() {
            format!("exited with {}", output.status)
        } else {
            stderr
        };
        return Err(format!("gh pr view: {message}"));
    }
    Ok(parse_merged_status(&output.stdout))
}

pub fn refresh_pr_details_cache(
    repo: &Repository,
    branch: &str,
    cache: &mut PrCache,
    path: &std::path::Path,
    config: &Config,
) -> Result<(), String> {
    cache.begin_details_poll();
    refresh_pr_details_cache_state(branch, cache, path, config);
    let Some(association) = cache.summary_identity() else {
        cache.pending_details = None;
        return cache.refresh_result();
    };
    let persistence = if let Some(details) = cache.details.as_ref() {
        super::store::save_pr_details_cache_for_association(
            repo,
            branch,
            details,
            &association,
            &cache.details_errors,
            &cache.details_warnings,
        )
    } else if !cache.details_errors.is_empty() || !cache.details_warnings.is_empty() {
        super::store::save_pr_details_cache_for_association(
            repo,
            branch,
            &PrDetails::default(),
            &association,
            &cache.details_errors,
            &cache.details_warnings,
        )
    } else {
        Ok(())
    };
    cache.details_persistence_error = persistence.err();
    cache.rebuild_error();
    cache.pending_details = None;
    cache.refresh_result()
}

pub(crate) fn refresh_pr_details_cache_state(
    branch: &str,
    cache: &mut PrCache,
    path: &std::path::Path,
    config: &Config,
) {
    if !super::coordinator::cache_eligible_for_worktree(branch, path, config) {
        cache.details = None;
        cache.details_association = None;
        cache.details_quality = PrObservationQuality::AuthoritativeAbsence;
        cache.details_errors.clear();
        cache.details_warnings.clear();
        cache.rebuild_error();
        return;
    }
    let Some(summary) = cache.summary.clone() else {
        cache.details = None;
        cache.details_association = None;
        cache.details_quality = PrObservationQuality::Unknown;
        return;
    };
    match fetch_pr_details(
        path,
        branch,
        PrDetailsAssociation::from_summary(&summary),
        config,
    ) {
        Ok(observation) => {
            cache.record_details_observation(observation);
        }
        Err(error) => {
            cache.details_errors = vec![error];
            cache.details_warnings.clear();
            cache.details_association = Some(PrDetailsAssociation::from_summary(&summary));
            cache.details_quality = if cache.details.is_some() {
                PrObservationQuality::PreservedStale
            } else {
                PrObservationQuality::Failed
            };
            cache.rebuild_error();
        }
    }
}

#[cfg(test)]
pub(crate) fn record_pr_details_poll_result(
    repo: &Repository,
    branch: &str,
    cache: &mut PrCache,
    poll_result: PrCache,
) -> bool {
    if !apply_pr_details_poll_result(cache, poll_result) {
        return false;
    }
    let persistence = super::store::persist_pr_cache_snapshot(repo, branch, cache);
    cache.details_persistence_error = persistence.err();
    cache.rebuild_error();
    true
}

#[cfg(test)]
fn record_pr_details_observation(
    repo: &Repository,
    branch: &str,
    cache: &mut PrCache,
    observation: PrDetailsObservation,
) -> bool {
    let mut poll_result = cache.begin_details_poll();
    if !poll_result.record_details_observation(observation) {
        cache.pending_details = None;
        return false;
    }
    record_pr_details_poll_result(repo, branch, cache, poll_result)
}

pub(crate) fn record_pr_merged(repo: &Repository, branch: &str, cache: &mut PrCache) {
    let Some(mut summary) = cache.summary.clone() else {
        return;
    };
    summary.merged = true;
    summary.state = "MERGED".to_string();
    super::store::record_pr_summary(repo, branch, cache, summary);
}

pub(crate) fn github_remote_repo(
    path: &std::path::Path,
    config: &Config,
    remote_name: &str,
) -> Result<String, String> {
    let (owner, name) = github_remote_owner_repo(path, config, remote_name)?;
    Ok(format!("{owner}/{name}"))
}

pub fn fetch_pr_summary_index(
    path: &std::path::Path,
    config: &Config,
) -> Result<Vec<PrSummary>, String> {
    let repository = github_remote_repository_id(path, config, "origin")?;
    fetch_pr_summary_index_for_repository(path, config, &repository)
}

pub(super) fn fetch_pr_summary_index_for_repository(
    path: &std::path::Path,
    config: &Config,
    repository: &crate::remote::RemoteRepositoryId,
) -> Result<Vec<PrSummary>, String> {
    fetch_open_pr_summaries_for_repository(path, config, repository, None)
}

pub(super) fn fetch_open_pr_summaries_for_repository_head(
    path: &std::path::Path,
    config: &Config,
    repository: &crate::remote::RemoteRepositoryId,
    head_ref: &str,
) -> Result<Vec<PrSummary>, String> {
    fetch_open_pr_summaries_for_repository(path, config, repository, Some(head_ref))
}

fn fetch_open_pr_summaries_for_repository(
    path: &std::path::Path,
    config: &Config,
    repository: &crate::remote::RemoteRepositoryId,
    head_ref: Option<&str>,
) -> Result<Vec<PrSummary>, String> {
    if repository.provider() != crate::remote::ProviderKind::GitHub {
        return Err("GitHub summary adapter requires a GitHub repository".to_string());
    }
    let (owner, name) = repository
        .project_path()
        .split_once('/')
        .filter(|(_, name)| !name.contains('/'))
        .ok_or_else(|| "GitHub project path is malformed".to_string())?;
    let mut command = Command::new(config.tool("gh"));
    command
        .args(github_graphql_api_args(config, repository.host()))
        .args(["--paginate", "--slurp"])
        .arg("-F")
        .arg(format!("owner={owner}"))
        .arg("-F")
        .arg(format!("name={name}"));
    if let Some(head_ref) = head_ref {
        command.arg("-f").arg(format!("headRefName={head_ref}"));
    }
    let raw = run_capture_named(
        command
            .arg("-f")
            .arg(format!("query={PR_SUMMARY_INDEX_QUERY}"))
            .current_dir(path),
        ProcessPolicy::NetworkQuery,
        ProcessDescriptor::new("gh.api.graphql"),
    )?;
    try_parse_pr_summary_index_for_repository(&raw, Some(repository))
}

pub(super) fn fetch_pr_summary_for_repository_number(
    path: &std::path::Path,
    config: &Config,
    repository: &crate::remote::RemoteRepositoryId,
    number: u64,
) -> Result<Option<PrSummary>, String> {
    if repository.provider() != crate::remote::ProviderKind::GitHub {
        return Err("GitHub summary adapter requires a GitHub repository".to_string());
    }
    let (owner, name) = repository
        .project_path()
        .split_once('/')
        .filter(|(_, name)| !name.contains('/'))
        .ok_or_else(|| "GitHub project path is malformed".to_string())?;
    let raw = run_capture_named(
        Command::new(config.tool("gh"))
            .args(github_graphql_api_args(config, repository.host()))
            .arg("-F")
            .arg(format!("owner={owner}"))
            .arg("-F")
            .arg(format!("name={name}"))
            .arg("-F")
            .arg(format!("number={number}"))
            .arg("-f")
            .arg(format!("query={PR_SUMMARY_QUERY}"))
            .current_dir(path),
        ProcessPolicy::NetworkQuery,
        ProcessDescriptor::new("gh.api.graphql"),
    )?;
    try_parse_pr_summary_for_repository(&raw, repository)
}

pub(crate) fn refresh_repo_policy_cache(
    repo: &Repository,
    path: &std::path::Path,
    config: &Config,
) -> Result<RepoPolicyCache, String> {
    let repository = github_remote_repository_id(path, config, "origin")?;
    let target_branch = config.default_base.as_deref().unwrap_or("main");
    refresh_repo_policy_cache_for_repository(repo, path, config, &repository, target_branch)
}

pub(crate) fn refresh_repo_policy_cache_for_repository(
    repo: &Repository,
    path: &std::path::Path,
    config: &Config,
    repository: &crate::remote::RemoteRepositoryId,
    target_branch: &str,
) -> Result<RepoPolicyCache, String> {
    let remote = repository.project_path().to_string();
    let policy = match fetch_repo_policy(path, config, repository, target_branch) {
        Ok(mut policy) => {
            policy.repo_remote = remote.clone();
            policy.provider = Some(repository.provider());
            policy.canonical_host = Some(repository.host().to_string());
            policy.project_path = Some(repository.project_path().to_string());
            policy.target_branch = Some(target_branch.to_string());
            policy.identity_complete = true;
            policy
        }
        Err(error) => {
            if let Some(mut stale) =
                super::store::load_repo_policy_cache_for_identity(repo, repository, target_branch)
            {
                stale.error = Some(error);
                stale
            } else {
                RepoPolicyCache {
                    repo_remote: remote.clone(),
                    provider: Some(repository.provider()),
                    canonical_host: Some(repository.host().to_string()),
                    project_path: Some(repository.project_path().to_string()),
                    target_branch: Some(target_branch.to_string()),
                    identity_complete: false,
                    refreshed_unix_ms: unix_seconds().max(0) as u64,
                    error: Some(error),
                    ..RepoPolicyCache::default()
                }
            }
        }
    };
    super::store::save_repo_policy_cache(repo, &policy)?;
    Ok(policy)
}

pub(crate) fn resolve_review_thread(
    path: &std::path::Path,
    config: &Config,
    host: &crate::remote::HostIdentity,
    thread_id: &str,
) -> Result<(), String> {
    let raw = run_capture_named(
        Command::new(config.tool("gh"))
            .args(resolve_review_thread_args(config, host, thread_id))
            .current_dir(path),
        ProcessPolicy::NetworkQuery,
        ProcessDescriptor::new("gh.api.graphql"),
    )?;
    let value = serde_json::from_str::<serde_json::Value>(&raw)
        .map_err(|error| format!("parse review thread resolution: {error}"))?;
    let thread = value
        .pointer("/data/resolveReviewThread/thread")
        .ok_or_else(|| "review thread resolution response is missing the thread".to_string())?;
    if !thread
        .get("isResolved")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return Err(format!("review thread {thread_id} was not resolved"));
    }
    if let Some(returned_id) = thread.get("id").and_then(serde_json::Value::as_str)
        && returned_id != thread_id
    {
        return Err(format!(
            "review thread resolution returned {returned_id}, expected {thread_id}"
        ));
    }
    Ok(())
}

fn resolve_review_thread_args(
    config: &Config,
    host: &crate::remote::HostIdentity,
    thread_id: &str,
) -> Vec<String> {
    let mut args = github_graphql_api_args(config, host);
    args.extend([
        "-F".to_string(),
        format!("thread={thread_id}"),
        "-f".to_string(),
        format!("query={RESOLVE_REVIEW_THREAD_MUTATION}"),
    ]);
    args
}

fn github_graphql_api_args(config: &Config, host: &crate::remote::HostIdentity) -> Vec<String> {
    let endpoint = config.remote_api_override(host).map_or_else(
        || "graphql".to_string(),
        |base| {
            base.strip_suffix("/api/v3").map_or_else(
                || format!("{}/graphql", base.trim_end_matches('/')),
                |root| format!("{root}/api/graphql"),
            )
        },
    );
    vec![
        "api".to_string(),
        endpoint,
        "--hostname".to_string(),
        host.to_string(),
    ]
}

fn github_api_endpoint(
    config: &Config,
    host: &crate::remote::HostIdentity,
    endpoint: &str,
) -> String {
    config.remote_api_override(host).map_or_else(
        || endpoint.to_string(),
        |base| {
            format!(
                "{}/{}",
                base.trim_end_matches('/'),
                endpoint.trim_start_matches('/')
            )
        },
    )
}

const RESOLVE_REVIEW_THREAD_MUTATION: &str = r#"
mutation($thread: ID!) {
  resolveReviewThread(input: {threadId: $thread}) {
    thread {
      id
      isResolved
    }
  }
}
"#;

fn fetch_repo_policy(
    path: &std::path::Path,
    config: &Config,
    repository: &crate::remote::RemoteRepositoryId,
    target_branch: &str,
) -> Result<RepoPolicyCache, String> {
    let (owner, name) = repository
        .project_path()
        .split_once('/')
        .filter(|(_, name)| !name.contains('/'))
        .ok_or_else(|| "GitHub project path is malformed".to_string())?;
    let owner = encode_path_segment(owner);
    let name = encode_path_segment(name);
    let branch = encode_path_segment(target_branch);
    let classic_endpoint = format!("/repos/{owner}/{name}/branches/{branch}/protection");
    let rules_endpoint = format!("/repos/{owner}/{name}/rules/branches/{branch}?per_page=100");

    let classic_endpoint = github_api_endpoint(config, repository.host(), &classic_endpoint);
    let rules_endpoint = github_api_endpoint(config, repository.host(), &rules_endpoint);
    let classic = fetch_classic_branch_protection(
        path,
        config,
        &repository.host().to_string(),
        &classic_endpoint,
    )?;
    let raw_rules = run_capture_named(
        Command::new(config.tool("gh"))
            .args(github_policy_api_args(
                &repository.host().to_string(),
                &rules_endpoint,
                true,
            ))
            .current_dir(path),
        ProcessPolicy::NetworkQuery,
        ProcessDescriptor::new("gh.api.rules.branch"),
    )?;
    let rulesets = parse_evaluated_branch_rules(&raw_rules)?;
    let facts = classic.combine(rulesets);

    Ok(RepoPolicyCache {
        repo_remote: repository.project_path().to_string(),
        provider: None,
        canonical_host: None,
        project_path: None,
        target_branch: Some(target_branch.to_string()),
        identity_complete: false,
        default_branch: Some(target_branch.to_string()),
        required_approvals: facts.required_approvals,
        require_conversation_resolution: facts.require_conversation_resolution,
        require_branch_up_to_date: facts.require_branch_up_to_date,
        required_checks: facts.required_checks,
        merge_queue_required: facts.merge_queue_required,
        refreshed_unix_ms: unix_seconds().max(0) as u64,
        error: None,
    })
}

fn github_policy_api_args(host: &str, endpoint: &str, paginate: bool) -> Vec<String> {
    let mut args = vec![
        "api".to_string(),
        "--hostname".to_string(),
        host.to_string(),
        "--method".to_string(),
        "GET".to_string(),
        "-H".to_string(),
        "Accept: application/vnd.github+json".to_string(),
    ];
    if paginate {
        args.push("--paginate".to_string());
        args.push("--slurp".to_string());
    }
    args.push(endpoint.to_string());
    args
}

fn fetch_classic_branch_protection(
    path: &std::path::Path,
    config: &Config,
    host: &str,
    endpoint: &str,
) -> Result<GithubPolicyFacts, String> {
    let output = run_output_allow_failure_named(
        Command::new(config.tool("gh"))
            .args(github_policy_api_args(host, endpoint, false))
            .current_dir(path),
        ProcessPolicy::NetworkQuery,
        ProcessDescriptor::new("gh.api.branch.protection"),
    )?;
    if output.status.success() {
        if output.stdout_truncated {
            return Err(format!(
                "GitHub classic branch protection response was truncated from {} bytes",
                output.stdout_total_bytes
            ));
        }
        return parse_classic_branch_protection(&output.stdout);
    }
    if !output.stderr_truncated && is_unprotected_branch_response(&output.stderr) {
        return Ok(GithubPolicyFacts::default());
    }
    let message = output.stderr.trim();
    Err(if message.is_empty() {
        format!(
            "gh api classic branch protection exited with {}",
            output.status
        )
    } else {
        format!("gh api classic branch protection: {message}")
    })
}

fn is_unprotected_branch_response(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    stderr.contains("branch not protected") && stderr.contains("http 404")
}

#[derive(Debug, Default)]
struct GithubPolicyFacts {
    required_approvals: u64,
    require_conversation_resolution: bool,
    require_branch_up_to_date: bool,
    required_checks: Vec<String>,
    merge_queue_required: bool,
}

impl GithubPolicyFacts {
    fn combine(mut self, other: Self) -> Self {
        self.required_approvals = self.required_approvals.max(other.required_approvals);
        self.require_conversation_resolution |= other.require_conversation_resolution;
        self.require_branch_up_to_date |= other.require_branch_up_to_date;
        self.merge_queue_required |= other.merge_queue_required;
        self.required_checks.extend(other.required_checks);
        self.required_checks = normalized_required_checks(&self.required_checks);
        self
    }
}

#[derive(Debug, Deserialize)]
struct GithubClassicBranchProtection {
    url: String,
    #[serde(default)]
    required_pull_request_reviews: Option<GithubClassicReviewRequirement>,
    #[serde(default)]
    required_status_checks: Option<GithubClassicStatusRequirement>,
    #[serde(default)]
    required_conversation_resolution: Option<GithubEnabledRequirement>,
}

#[derive(Debug, Deserialize)]
struct GithubClassicReviewRequirement {
    required_approving_review_count: u64,
    #[serde(default)]
    require_code_owner_reviews: bool,
    #[serde(default)]
    require_last_push_approval: bool,
}

#[derive(Debug, Deserialize)]
struct GithubClassicStatusRequirement {
    strict: bool,
    #[serde(default)]
    contexts: Vec<String>,
    #[serde(default)]
    checks: Vec<GithubRequiredStatusCheck>,
}

#[derive(Debug, Deserialize)]
struct GithubRequiredStatusCheck {
    context: String,
}

#[derive(Debug, Deserialize)]
struct GithubEnabledRequirement {
    enabled: bool,
}

fn parse_classic_branch_protection(raw: &str) -> Result<GithubPolicyFacts, String> {
    let protection = serde_json::from_str::<GithubClassicBranchProtection>(raw)
        .map_err(|error| format!("parse GitHub classic branch protection: {error}"))?;
    if protection.url.trim().is_empty() {
        return Err("parse GitHub classic branch protection: missing URL".to_string());
    }
    let reviews = protection.required_pull_request_reviews;
    let statuses = protection.required_status_checks;
    let mut required_checks = statuses
        .as_ref()
        .map(|statuses| statuses.contexts.clone())
        .unwrap_or_default();
    required_checks.extend(
        statuses
            .as_ref()
            .into_iter()
            .flat_map(|statuses| statuses.checks.iter())
            .map(|check| check.context.clone()),
    );
    Ok(GithubPolicyFacts {
        required_approvals: reviews
            .map(|reviews| {
                reviews.required_approving_review_count.max(u64::from(
                    reviews.require_code_owner_reviews || reviews.require_last_push_approval,
                ))
            })
            .unwrap_or(0),
        require_conversation_resolution: protection
            .required_conversation_resolution
            .map(|requirement| requirement.enabled)
            .unwrap_or(false),
        require_branch_up_to_date: statuses
            .as_ref()
            .map(|statuses| statuses.strict)
            .unwrap_or(false),
        required_checks: normalized_required_checks(&required_checks),
        merge_queue_required: false,
    })
}

#[derive(Debug, Deserialize)]
struct GithubEvaluatedBranchRule {
    #[serde(rename = "type")]
    rule_type: String,
    #[serde(default)]
    parameters: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct GithubRulesetPullRequestParameters {
    required_approving_review_count: u64,
    required_review_thread_resolution: bool,
    require_code_owner_review: bool,
    require_last_push_approval: bool,
    #[serde(default)]
    required_reviewers: Vec<GithubRulesetRequiredReviewer>,
}

#[derive(Debug, Deserialize)]
struct GithubRulesetRequiredReviewer {
    minimum_approvals: u64,
}

#[derive(Debug, Deserialize)]
struct GithubRulesetStatusCheckParameters {
    strict_required_status_checks_policy: bool,
    required_status_checks: Vec<GithubRequiredStatusCheck>,
}

#[derive(Debug, Deserialize)]
struct GithubMergeQueueParameters {
    #[serde(rename = "check_response_timeout_minutes")]
    _check_response_timeout_minutes: u64,
    #[serde(rename = "grouping_strategy")]
    _grouping_strategy: String,
    #[serde(rename = "max_entries_to_build")]
    _max_entries_to_build: u64,
    #[serde(rename = "max_entries_to_merge")]
    _max_entries_to_merge: u64,
    #[serde(rename = "merge_method")]
    _merge_method: String,
    #[serde(rename = "min_entries_to_merge")]
    _min_entries_to_merge: u64,
    #[serde(rename = "min_entries_to_merge_wait_minutes")]
    _min_entries_to_merge_wait_minutes: u64,
}

fn parse_evaluated_branch_rules(raw: &str) -> Result<GithubPolicyFacts, String> {
    let pages = serde_json::from_str::<Vec<Vec<GithubEvaluatedBranchRule>>>(raw)
        .map_err(|error| format!("parse GitHub evaluated branch rules: {error}"))?;
    if pages.is_empty() {
        return Err(
            "parse GitHub evaluated branch rules: missing paginated response envelope".to_string(),
        );
    }
    let mut facts = GithubPolicyFacts::default();
    for rule in pages.into_iter().flatten() {
        match rule.rule_type.as_str() {
            "pull_request" => {
                let parameters = parse_rule_parameters::<GithubRulesetPullRequestParameters>(
                    rule.parameters,
                    "pull_request",
                )?;
                facts.required_approvals = facts
                    .required_approvals
                    .max(parameters.required_approving_review_count)
                    .max(u64::from(
                        parameters.require_code_owner_review
                            || parameters.require_last_push_approval,
                    ))
                    .max(
                        parameters
                            .required_reviewers
                            .iter()
                            .map(|reviewer| reviewer.minimum_approvals)
                            .max()
                            .unwrap_or(0),
                    );
                facts.require_conversation_resolution |=
                    parameters.required_review_thread_resolution;
            }
            "required_status_checks" => {
                let parameters = parse_rule_parameters::<GithubRulesetStatusCheckParameters>(
                    rule.parameters,
                    "required_status_checks",
                )?;
                facts.require_branch_up_to_date |= parameters.strict_required_status_checks_policy;
                facts.required_checks.extend(
                    parameters
                        .required_status_checks
                        .into_iter()
                        .map(|check| check.context),
                );
            }
            "merge_queue" => {
                let _parameters = parse_rule_parameters::<GithubMergeQueueParameters>(
                    rule.parameters,
                    "merge_queue",
                )?;
                facts.merge_queue_required = true;
            }
            "creation"
            | "update"
            | "deletion"
            | "required_linear_history"
            | "required_signatures"
            | "non_fast_forward"
            | "commit_message_pattern"
            | "commit_author_email_pattern"
            | "committer_email_pattern"
            | "branch_name_pattern"
            | "file_path_restriction"
            | "max_file_path_length"
            | "file_extension_restriction"
            | "max_file_size"
            | "copilot_code_review" => {}
            "workflows" | "required_deployments" | "code_scanning" => {
                return Err(format!(
                    "GitHub policy evidence is unknown: safety-relevant evaluated branch rule {} is not supported",
                    rule.rule_type
                ));
            }
            "" => {
                return Err("parse GitHub evaluated branch rules: rule type is empty".to_string());
            }
            unsupported => {
                return Err(format!(
                    "GitHub policy evidence is unknown: unrecognized evaluated branch rule {unsupported}"
                ));
            }
        }
    }
    facts.required_checks = normalized_required_checks(&facts.required_checks);
    Ok(facts)
}

fn parse_rule_parameters<T: serde::de::DeserializeOwned>(
    parameters: Option<serde_json::Value>,
    rule_type: &str,
) -> Result<T, String> {
    let parameters = parameters.ok_or_else(|| {
        format!("parse GitHub evaluated branch rules: {rule_type} rule is missing parameters")
    })?;
    serde_json::from_value(parameters).map_err(|error| {
        format!("parse GitHub evaluated branch rules: malformed {rule_type} parameters: {error}")
    })
}

fn encode_path_segment(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[(byte >> 4) as usize]));
            encoded.push(char::from(HEX[(byte & 0x0f) as usize]));
        }
    }
    encoded
}

const PR_SUMMARY_INDEX_QUERY: &str = r#"
query($owner: String!, $name: String!, $headRefName: String, $endCursor: String) {
  repository(owner: $owner, name: $name) {
    pullRequests(first: 100, after: $endCursor, states: OPEN, headRefName: $headRefName, orderBy: {field: UPDATED_AT, direction: DESC}) {
      pageInfo {
        hasNextPage
        endCursor
      }
      nodes {
        id
        number
        title
        author {
          login
        }
        body
        url
        state
        reviewDecision
        reviewRequests(first: 10) {
          nodes {
            requestedReviewer {
              __typename
              ... on User {
                login
              }
              ... on Team {
                slug
              }
            }
          }
        }
        headRefName
        baseRefName
        headRefOid
        headRepository {
          nameWithOwner
        }
        baseRepository {
          nameWithOwner
        }
        updatedAt
        mergeStateStatus
        mergeQueueEntry {
          state
        }
        merged
        isDraft
        comments {
          totalCount
        }
        reviewThreads(first: 1) {
          totalCount
        }
        commits(last: 1) {
          nodes {
            commit {
              statusCheckRollup {
                contexts(first: 50) {
                  pageInfo {
                    hasNextPage
                  }
                  nodes {
                    __typename
                    ... on CheckRun {
                      name
                      status
                      conclusion
                    }
                    ... on StatusContext {
                      context
                      state
                    }
                  }
                }
              }
            }
          }
        }
      }
    }
  }
}
"#;

const PR_SUMMARY_QUERY: &str = r#"
query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      id
      number
      title
      author {
        login
      }
      body
      url
      state
      reviewDecision
      reviewRequests(first: 10) {
        nodes {
          requestedReviewer {
            __typename
            ... on User {
              login
            }
            ... on Team {
              slug
            }
          }
        }
      }
      headRefName
      baseRefName
      headRefOid
      headRepository {
        nameWithOwner
      }
      baseRepository {
        nameWithOwner
      }
      updatedAt
      mergeStateStatus
      mergeQueueEntry {
        state
      }
      merged
      isDraft
      comments {
        totalCount
      }
      reviewThreads(first: 1) {
        totalCount
      }
      commits(last: 1) {
        nodes {
          commit {
            statusCheckRollup {
              contexts(first: 50) {
                pageInfo {
                  hasNextPage
                }
                nodes {
                  __typename
                  ... on CheckRun {
                    name
                    status
                    conclusion
                  }
                  ... on StatusContext {
                    context
                    state
                  }
                }
              }
            }
          }
        }
      }
    }
  }
}
"#;

fn github_owner_repo(path: &std::path::Path, config: &Config) -> Result<(String, String), String> {
    github_remote_owner_repo(path, config, "origin")
}

fn github_remote_owner_repo(
    path: &std::path::Path,
    config: &Config,
    remote_name: &str,
) -> Result<(String, String), String> {
    let repository = github_remote_repository_id(path, config, remote_name)?;
    let (owner, name) = repository
        .project_path()
        .split_once('/')
        .ok_or_else(|| format!("{remote_name} GitHub project path is malformed"))?;
    if name.contains('/') {
        return Err(format!("{remote_name} GitHub project path is malformed"));
    }
    Ok((owner.to_string(), name.to_string()))
}

fn github_remote_repository_id(
    path: &std::path::Path,
    config: &Config,
    remote_name: &str,
) -> Result<crate::remote::RemoteRepositoryId, String> {
    let remote = crate::remote::discover_git_remote(
        path,
        config,
        remote_name,
        crate::remote::RemoteUrlKind::Fetch,
    )
    .map_err(|error| error.to_string())?;
    if remote.repository.id.provider() != crate::remote::ProviderKind::GitHub {
        return Err(format!("{remote_name} remote is not a GitHub repository"));
    }
    Ok(remote.repository.id)
}

#[cfg(test)]
fn parse_github_remote(remote: &str) -> Option<(String, String)> {
    let remote = crate::remote::RemoteDiscovery::default()
        .discover(remote)
        .ok()?;
    if remote.repository.id.provider() != crate::remote::ProviderKind::GitHub {
        return None;
    }
    let (owner, name) = remote.repository.id.project_path().split_once('/')?;
    (!name.contains('/')).then(|| (owner.to_string(), name.to_string()))
}

#[cfg(test)]
fn parse_pr_summary_index(raw: &str) -> Vec<PrSummary> {
    try_parse_pr_summary_index(raw).unwrap_or_default()
}

fn try_parse_pr_summary_index(raw: &str) -> Result<Vec<PrSummary>, String> {
    try_parse_pr_summary_index_for_repository(raw, None)
}

fn try_parse_pr_summary_index_for_repository(
    raw: &str,
    repository: Option<&crate::remote::RemoteRepositoryId>,
) -> Result<Vec<PrSummary>, String> {
    let value = serde_json::from_str::<serde_json::Value>(raw)
        .map_err(|error| format!("parse GitHub PR summary index: {error}"))?;
    let pages = match &value {
        serde_json::Value::Array(pages) if !pages.is_empty() => pages.as_slice(),
        serde_json::Value::Array(_) => {
            return Err("parse GitHub PR summary index: missing paginated response".to_string());
        }
        _ => std::slice::from_ref(&value),
    };
    let mut summaries = Vec::new();
    for (index, page) in pages.iter().enumerate() {
        if !page
            .pointer("/data/repository/pullRequests/nodes")
            .is_some_and(serde_json::Value::is_array)
        {
            return Err(
                "parse GitHub PR summary index: missing pull request connection".to_string(),
            );
        }
        if !page
            .pointer("/data/repository/pullRequests/pageInfo/hasNextPage")
            .is_some_and(serde_json::Value::is_boolean)
        {
            return Err(
                "parse GitHub PR summary index: missing pull request pagination".to_string(),
            );
        }
        validate_pr_summary_check_pagination(page)?;
        let response = serde_json::from_value::<GithubPrSummaryIndexResponse>(page.clone())
            .map_err(|error| format!("parse GitHub PR summary index: {error}"))?;
        let has_next_page = response
            .data
            .repository
            .pull_requests
            .page_info
            .has_next_page;
        if has_next_page != (index + 1 < pages.len()) {
            return Err(
                "GitHub PR summary index pagination is incomplete or inconsistent".to_string(),
            );
        }
        summaries.extend(
            response
                .data
                .repository
                .pull_requests
                .nodes
                .iter()
                .map(|node| pr_summary_from_node(node, repository))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| {
                    "parse GitHub PR summary index: pull request is missing identity".to_string()
                })?,
        );
    }
    Ok(summaries)
}

fn try_parse_pr_summary_for_repository(
    raw: &str,
    repository: &crate::remote::RemoteRepositoryId,
) -> Result<Option<PrSummary>, String> {
    let value = serde_json::from_str::<serde_json::Value>(raw)
        .map_err(|error| format!("parse GitHub PR summary: {error}"))?;
    if value
        .get("errors")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|errors| !errors.is_empty())
    {
        return Err("GitHub PR summary query returned GraphQL errors".to_string());
    }
    let Some(node) = value.pointer("/data/repository/pullRequest") else {
        return Err("parse GitHub PR summary: missing pull request field".to_string());
    };
    if node.is_null() {
        return Ok(None);
    }
    validate_pr_summary_check_pagination(&serde_json::json!({
        "data": {"repository": {"pullRequests": {"nodes": [node]}}}
    }))?;
    let node = serde_json::from_value::<GithubPullRequest>(node.clone())
        .map_err(|error| format!("parse GitHub PR summary: {error}"))?;
    pr_summary_from_node(&node, Some(repository))
        .map(Some)
        .ok_or_else(|| "parse GitHub PR summary: pull request is missing identity".to_string())
}

fn validate_pr_summary_check_pagination(value: &serde_json::Value) -> Result<(), String> {
    let pull_requests = value
        .pointer("/data/repository/pullRequests/nodes")
        .and_then(serde_json::Value::as_array)
        .expect("pull request nodes were validated above");
    for pull_request in pull_requests {
        let Some(commits) = pull_request
            .pointer("/commits/nodes")
            .and_then(serde_json::Value::as_array)
        else {
            continue;
        };
        for commit in commits {
            let Some(rollup) = commit.pointer("/commit/statusCheckRollup") else {
                continue;
            };
            if rollup.is_null() {
                continue;
            }
            let has_next_page = rollup
                .pointer("/contexts/pageInfo/hasNextPage")
                .and_then(serde_json::Value::as_bool)
                .ok_or_else(|| {
                    "parse GitHub PR summary index: missing check rollup pagination".to_string()
                })?;
            if has_next_page {
                return Err(
                    "GitHub check rollup is truncated after the first 50 contexts".to_string(),
                );
            }
        }
    }
    Ok(())
}

fn normalized_required_checks(checks: &[String]) -> Vec<String> {
    let mut normalized = Vec::new();
    for check in checks {
        let check = check.trim();
        if check.is_empty() || normalized.iter().any(|existing| existing == check) {
            continue;
        }
        normalized.push(check.to_string());
    }
    normalized
}

fn pr_summary_from_node(
    node: &GithubPullRequest,
    repository: Option<&crate::remote::RemoteRepositoryId>,
) -> Option<PrSummary> {
    let number = node.number?;
    let queue_evidence = match &node.merge_queue_entry {
        GithubMergeQueueObservation::NotObserved => Vec::new(),
        GithubMergeQueueObservation::NotQueued => vec!["null".to_string()],
        GithubMergeQueueObservation::Entry(entry) => vec![entry.state.clone()],
    };
    Some(PrSummary {
        number,
        change_request_identity: github_change_request_identity(node, repository),
        native_state_evidence: crate::remote::NativeStateEvidence {
            lifecycle: crate::remote::NativeStateEvidence::retain([node.state.clone()]),
            review: crate::remote::NativeStateEvidence::retain(node.review_decision.clone()),
            mergeability: crate::remote::NativeStateEvidence::retain([node
                .merge_state_status
                .clone()]),
            check: crate::remote::NativeStateEvidence::retain(
                status_contexts_for_pr(node)
                    .into_iter()
                    .flat_map(|context| {
                        [context.status, context.conclusion, context.state]
                            .into_iter()
                            .flatten()
                    }),
            ),
            queue: crate::remote::NativeStateEvidence::retain(queue_evidence),
        },
        title: node.title.clone(),
        author: node.author.login.clone(),
        body: node.body.clone(),
        url: node.url.clone(),
        state: node.state.clone(),
        review_decision: node
            .review_decision
            .as_deref()
            .filter(|decision| !decision.trim().is_empty())
            .unwrap_or("UNKNOWN")
            .to_string(),
        requested_reviewers: requested_reviewers_from_requests(&node.review_requests),
        head_ref: node.head_ref_name.clone(),
        base_ref: node.base_ref_name.clone(),
        head_sha: node.head_ref_oid.clone(),
        updated_at: node.updated_at.clone(),
        check_status: check_status_for_pr(node),
        merge_state_status: node.merge_state_status.clone(),
        queue_state: match &node.merge_queue_entry {
            GithubMergeQueueObservation::NotObserved => "unknown".to_string(),
            GithubMergeQueueObservation::NotQueued => "not_queued".to_string(),
            GithubMergeQueueObservation::Entry(entry) if !entry.state.trim().is_empty() => {
                entry.state.clone()
            }
            GithubMergeQueueObservation::Entry(_) => "unknown".to_string(),
        },
        comment_count: node.comments.total_count + node.review_threads.total_count,
        merged: merged_status_from_node(node),
        draft: node.is_draft,
    })
}

fn github_change_request_identity(
    node: &GithubPullRequest,
    repository: Option<&crate::remote::RemoteRepositoryId>,
) -> Option<crate::remote::CanonicalChangeRequestIdentity> {
    let repository = repository?;
    let native_id = crate::remote::NativeChangeRequestId::new(node.id.clone()).ok()?;
    let source_path = node.head_repository.name_with_owner.trim();
    let target_path = node.base_repository.name_with_owner.trim();
    let target_path = if target_path.is_empty() {
        repository.project_path()
    } else {
        target_path
    };
    if source_path.is_empty() {
        return None;
    }
    let source = crate::remote::RemoteRepositoryId::new(
        repository.provider(),
        repository.host().clone(),
        source_path,
    )
    .ok()?;
    let target = crate::remote::RemoteRepositoryId::new(
        repository.provider(),
        repository.host().clone(),
        target_path,
    )
    .ok()?;
    if &target != repository {
        return None;
    }
    Some(crate::remote::CanonicalChangeRequestIdentity::new(
        repository, &native_id, &source, &target,
    ))
}

fn fetch_pr_summary(
    path: &std::path::Path,
    branch: &str,
    config: &Config,
) -> Result<Option<(PrSummary, String)>, String> {
    if branch == "(detached)" {
        return Ok(None);
    }
    let repository = github_remote_repository_id(path, config, "origin")?;
    let fields = [
        "id",
        "number",
        "title",
        "author",
        "body",
        "url",
        "state",
        "reviewDecision",
        "reviewRequests",
        "headRefName",
        "baseRefName",
        "headRefOid",
        "headRepository",
        "updatedAt",
        "statusCheckRollup",
        "mergeStateStatus",
        "mergedAt",
        "isDraft",
    ]
    .join(",");
    let output = run_output_allow_failure_named(
        Command::new(config.tool("gh"))
            .arg("pr")
            .arg("view")
            .arg(branch)
            .arg("--json")
            .arg(fields)
            .current_dir(path),
        ProcessPolicy::NetworkQuery,
        ProcessDescriptor::new("gh.pr.view"),
    )?;
    if !output.status.success() {
        let stderr = output.stderr.trim().to_string();
        if stderr.contains("no pull requests found")
            || stderr.contains("not found")
            || stderr.contains("Could not resolve to a PullRequest")
        {
            return Ok(None);
        }
        let message = if stderr.is_empty() {
            format!("exited with {}", output.status)
        } else {
            stderr
        };
        return Err(format!("gh pr view: {message}"));
    }
    let raw = output.stdout;
    let node = serde_json::from_str::<GithubPullRequest>(&raw)
        .map_err(|error| format!("parse gh pr view output: {error}"))?;
    let summary = pr_summary_from_node(&node, Some(&repository))
        .ok_or_else(|| "parse gh pr view output: missing pull request number".to_string())?;
    Ok(Some((summary, raw)))
}

fn fetch_pr_details(
    path: &std::path::Path,
    branch: &str,
    association: PrDetailsAssociation,
    config: &Config,
) -> Result<PrDetailsObservation, String> {
    let repository = match association.change_request_identity.as_ref() {
        Some(identity) => identity
            .target_repository()
            .map_err(|error| format!("invalid canonical GitHub target repository: {error}"))?,
        None => github_remote_repository_id(path, config, "origin")?,
    };
    let details = fetch_pr_details_for_repository_number(
        path,
        config,
        &repository,
        association.pr_number,
        branch,
        &association.head_sha,
    )?;
    Ok(PrDetailsObservation {
        association,
        comments: details.comments,
        reviews: details.reviews,
        review_comments: details.review_comments,
        files: details.files,
        failing_checks: details.failing_checks,
        check_contexts: details.check_contexts,
        ci_failures: details.ci_failures,
        partial_errors: details.partial_errors,
    })
}

pub(super) fn fetch_pr_details_for_repository_number(
    path: &std::path::Path,
    config: &Config,
    repository: &crate::remote::RemoteRepositoryId,
    pr_number: u64,
    source_branch: &str,
    head_sha: &str,
) -> Result<ProviderDetailsObservation, String> {
    if repository.provider() != crate::remote::ProviderKind::GitHub {
        return Err("GitHub detail adapter requires a GitHub target repository".to_string());
    }
    let endpoint = |suffix: &str| github_repository_api_endpoint(repository, suffix);
    let comments = fetch_paginated_github_array::<GhPrComment>(
        path,
        config,
        repository,
        &endpoint(&format!("issues/{pr_number}/comments?per_page=100"))?,
        "pull request comments",
    )
    .map(|comments| parse_gh_comments(&comments));
    let reviews = fetch_paginated_github_array::<GhPrReview>(
        path,
        config,
        repository,
        &endpoint(&format!("pulls/{pr_number}/reviews?per_page=100"))?,
        "pull request reviews",
    )
    .map(|reviews| parse_gh_reviews(&reviews));
    let files = fetch_paginated_github_array::<GhPrFile>(
        path,
        config,
        repository,
        &endpoint(&format!("pulls/{pr_number}/files?per_page=100"))?,
        "pull request files",
    )
    .and_then(|files| {
        // GitHub documents a hard ceiling of 3,000 files for this endpoint. At the ceiling
        // pagination cannot prove that the observed set is complete.
        if files.len() >= 3_000 {
            return Err(
                "GitHub pull request files reached the 3,000-file API completeness limit"
                    .to_string(),
            );
        }
        Ok(files
            .into_iter()
            .map(|file| file.path)
            .filter(|path| !path.trim().is_empty())
            .collect())
    });
    let review_comments = fetch_inline_review_comments(path, repository, pr_number, config);
    let checks = fetch_complete_check_contexts(path, config, repository, head_sha);
    let (failing_checks, check_contexts) = match checks {
        Ok(contexts) => (
            Ok(collect_failing_checks_from_contexts(&contexts)),
            Ok(collect_check_contexts_from_contexts(&contexts)),
        ),
        Err(error) => (Err(error.clone()), Err(error)),
    };
    let ci_failures = match &failing_checks {
        Ok(failing_checks) if !failing_checks.is_empty() => {
            fetch_ci_failures(path, repository, source_branch, head_sha, config)
        }
        _ => Ok(Vec::new()),
    };
    Ok(ProviderDetailsObservation {
        comments,
        reviews,
        review_comments,
        files,
        failing_checks,
        check_contexts,
        ci_failures,
        partial_errors: Vec::new(),
    })
}

fn github_repository_api_endpoint(
    repository: &crate::remote::RemoteRepositoryId,
    suffix: &str,
) -> Result<String, String> {
    let (owner, name) = repository
        .project_path()
        .split_once('/')
        .filter(|(_, name)| !name.contains('/'))
        .ok_or_else(|| "GitHub project path is malformed".to_string())?;
    Ok(format!(
        "/repos/{}/{}/{suffix}",
        encode_path_segment(owner),
        encode_path_segment(name)
    ))
}

fn fetch_paginated_github_array<T: serde::de::DeserializeOwned>(
    path: &std::path::Path,
    config: &Config,
    repository: &crate::remote::RemoteRepositoryId,
    endpoint: &str,
    label: &str,
) -> Result<Vec<T>, String> {
    let raw = run_capture_named(
        Command::new(config.tool("gh"))
            .args(github_policy_api_args(
                &repository.host().to_string(),
                &github_api_endpoint(config, repository.host(), endpoint),
                true,
            ))
            .current_dir(path),
        ProcessPolicy::NetworkQuery,
        ProcessDescriptor::new("gh.api.paginated"),
    )?;
    let pages = serde_json::from_str::<Vec<Vec<T>>>(&raw)
        .map_err(|error| format!("parse paginated GitHub {label}: {error}"))?;
    if pages.is_empty() {
        return Err(format!(
            "parse paginated GitHub {label}: missing pagination envelope"
        ));
    }
    Ok(pages.into_iter().flatten().collect())
}

fn fetch_complete_check_contexts(
    path: &std::path::Path,
    config: &Config,
    repository: &crate::remote::RemoteRepositoryId,
    head_sha: &str,
) -> Result<Vec<GithubStatusContext>, String> {
    if head_sha.trim().is_empty() {
        return Err("GitHub checks cannot be fetched without a head SHA".to_string());
    }
    let check_runs_endpoint = github_repository_api_endpoint(
        repository,
        &format!(
            "commits/{}/check-runs?per_page=100",
            encode_path_segment(head_sha)
        ),
    )?;
    let raw = run_capture_named(
        Command::new(config.tool("gh"))
            .args(github_policy_api_args(
                &repository.host().to_string(),
                &github_api_endpoint(config, repository.host(), &check_runs_endpoint),
                true,
            ))
            .current_dir(path),
        ProcessPolicy::NetworkQuery,
        ProcessDescriptor::new("gh.api.check-runs"),
    )?;
    let pages = serde_json::from_str::<Vec<GhCheckRunsPage>>(&raw)
        .map_err(|error| format!("parse paginated GitHub check runs: {error}"))?;
    let Some(total_count) = pages.first().map(|page| page.total_count) else {
        return Err("parse paginated GitHub check runs: missing pagination envelope".to_string());
    };
    if pages.iter().any(|page| page.total_count != total_count) {
        return Err("GitHub check run pagination returned inconsistent totals".to_string());
    }
    let mut contexts = pages
        .into_iter()
        .flat_map(|page| page.check_runs)
        .collect::<Vec<_>>();
    if contexts.len() as u64 != total_count {
        return Err(format!(
            "GitHub returned only {} of {total_count} check runs",
            contexts.len()
        ));
    }
    let statuses_endpoint = github_repository_api_endpoint(
        repository,
        &format!(
            "commits/{}/statuses?per_page=100",
            encode_path_segment(head_sha)
        ),
    )?;
    contexts.extend(fetch_paginated_github_array::<GithubStatusContext>(
        path,
        config,
        repository,
        &statuses_endpoint,
        "commit statuses",
    )?);
    Ok(contexts)
}

#[cfg(test)]
fn parse_pr_details(raw: &str) -> PrDetails {
    try_parse_pr_details(raw).unwrap_or_default()
}

fn try_parse_pr_details(raw: &str) -> Result<PrDetails, String> {
    let value = serde_json::from_str::<serde_json::Value>(raw)
        .map_err(|error| format!("parse gh pr details output: {error}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| "parse gh pr details output: expected an object".to_string())?;
    for field in ["comments", "reviews", "files", "statusCheckRollup"] {
        if !object.contains_key(field) {
            return Err(format!("parse gh pr details output: missing {field}"));
        }
    }
    let details = serde_json::from_str::<GhPrViewDetails>(raw)
        .map_err(|error| format!("parse gh pr details output: {error}"))?;
    let comments = parse_pr_comments(&details);
    let reviews = parse_pr_reviews(&details);
    let check_contexts = collect_check_contexts(&details.status_check_rollup);
    let failing_checks = collect_failing_checks(&details.status_check_rollup);
    Ok(PrDetails {
        comments,
        reviews,
        review_comments: Vec::new(),
        files: details
            .files
            .into_iter()
            .map(|file| file.path)
            .filter(|path| !path.trim().is_empty())
            .collect(),
        failing_checks,
        check_contexts,
        ci_failures: Vec::new(),
    })
}

fn fetch_ci_failures(
    path: &std::path::Path,
    repository: &crate::remote::RemoteRepositoryId,
    _branch: &str,
    head_sha: &str,
    config: &Config,
) -> Result<Vec<CiFailure>, String> {
    let endpoint = github_repository_api_endpoint(
        repository,
        &format!(
            "actions/runs?head_sha={}&per_page=100",
            encode_path_segment(head_sha)
        ),
    )?;
    let raw = run_capture_named(
        Command::new(config.tool("gh"))
            .args(github_policy_api_args(
                &repository.host().to_string(),
                &github_api_endpoint(config, repository.host(), &endpoint),
                true,
            ))
            .current_dir(path),
        ProcessPolicy::NetworkQuery,
        ProcessDescriptor::new("gh.api.workflow-runs"),
    )?;
    let pages = serde_json::from_str::<Vec<GhWorkflowRunsPage>>(&raw)
        .map_err(|error| format!("parse paginated GitHub workflow runs: {error}"))?;
    let Some(total_count) = pages.first().map(|page| page.total_count) else {
        return Err(
            "parse paginated GitHub workflow runs: missing pagination envelope".to_string(),
        );
    };
    if pages.iter().any(|page| page.total_count != total_count) {
        return Err("GitHub workflow run pagination returned inconsistent totals".to_string());
    }
    let runs = pages
        .into_iter()
        .flat_map(|page| page.workflow_runs)
        .collect::<Vec<_>>();
    if runs.len() as u64 != total_count {
        return Err(format!(
            "GitHub returned only {} of {total_count} workflow runs",
            runs.len()
        ));
    }
    let mut failures = Vec::new();
    for run in runs {
        if !run.head_sha.trim().is_empty() && run.head_sha != head_sha {
            continue;
        }
        if !is_failure_conclusion(&run.conclusion) {
            continue;
        }
        let run_id = run.database_id.to_string();
        let log_tail = fetch_failed_run_log_tail(path, repository, &run_id, config)?;
        failures.push(CiFailure {
            workflow: first_non_empty([run.workflow_name.as_str(), run.name.as_str()]),
            name: first_non_empty([run.display_title.as_str(), run.name.as_str()]),
            conclusion: first_non_empty([run.conclusion.as_str(), run.status.as_str()]),
            url: run.url,
            run_id,
            log_tail,
        });
    }
    Ok(failures)
}

fn fetch_failed_run_log_tail(
    path: &std::path::Path,
    repository: &crate::remote::RemoteRepositoryId,
    run_id: &str,
    config: &Config,
) -> Result<String, String> {
    if run_id == "0" {
        return Ok(String::new());
    }
    let output = run_output_allow_failure_named(
        Command::new(config.tool("gh"))
            .arg("run")
            .arg("view")
            .arg(run_id)
            .arg("--log-failed")
            .arg("--repo")
            .arg(gh_repository_selector(repository))
            .current_dir(path),
        ProcessPolicy::NetworkQuery,
        ProcessDescriptor::new("gh.run.view"),
    )?;
    if !output.status.success() {
        let message = output.stderr.trim();
        return Err(if message.is_empty() {
            format!("gh run view exited with {}", output.status)
        } else {
            format!("gh run view: {message}")
        });
    }
    Ok(tail_lines(&strip_ansi(&output.stdout), 80))
}

fn gh_repository_selector(repository: &crate::remote::RemoteRepositoryId) -> String {
    if repository.host().to_string() == "github.com" {
        repository.project_path().to_string()
    } else {
        format!("{}/{}", repository.host(), repository.project_path())
    }
}

fn is_failure_conclusion(value: &str) -> bool {
    matches!(
        value.to_ascii_uppercase().as_str(),
        "FAILURE" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED"
    )
}

fn tail_lines(text: &str, max_lines: usize) -> String {
    let lines = text.lines().collect::<Vec<_>>();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}

fn fetch_inline_review_comments(
    path: &std::path::Path,
    repository: &crate::remote::RemoteRepositoryId,
    pr_number: u64,
    config: &Config,
) -> Result<Vec<PrReviewComment>, String> {
    let (owner, name) = repository
        .project_path()
        .split_once('/')
        .filter(|(_, name)| !name.contains('/'))
        .ok_or_else(|| "GitHub project path is malformed".to_string())?;
    let raw = run_capture_named(
        Command::new(config.tool("gh"))
            .args(github_graphql_api_args(config, repository.host()))
            .arg("-F")
            .arg(format!("owner={owner}"))
            .arg("-F")
            .arg(format!("name={name}"))
            .arg("-F")
            .arg(format!("number={pr_number}"))
            .arg("-f")
            .arg(format!("query={PR_REVIEW_THREADS_QUERY}"))
            .arg("--paginate")
            .arg("--slurp")
            .current_dir(path),
        ProcessPolicy::NetworkQuery,
        ProcessDescriptor::new("gh.api.graphql"),
    )?;
    try_parse_review_thread_comments(&raw)
}

const PR_REVIEW_THREADS_QUERY: &str = r#"
query($owner: String!, $name: String!, $number: Int!, $endCursor: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      reviewThreads(first: 100, after: $endCursor) {
        totalCount
        pageInfo {
          hasNextPage
          endCursor
        }
        nodes {
          id
          isResolved
          comments(first: 100) {
            totalCount
            pageInfo {
              hasNextPage
            }
            nodes {
              author {
                login
              }
              id
              path
              line
              originalLine
              body
              createdAt
            }
          }
        }
      }
    }
  }
}
"#;

fn parse_pr_comments(details: &GhPrViewDetails) -> Vec<PrComment> {
    parse_gh_comments(&details.comments)
}

fn parse_gh_comments(comments: &[GhPrComment]) -> Vec<PrComment> {
    comments
        .iter()
        .map(|object| PrComment {
            id: object.id.clone(),
            author: first_non_empty([object.author.login.as_str(), object.user.login.as_str()]),
            body: object.body.clone(),
            created_at: object.created_at.clone(),
        })
        .filter(|comment| !comment.body.trim().is_empty())
        .collect()
}

fn parse_pr_reviews(details: &GhPrViewDetails) -> Vec<PrReview> {
    parse_gh_reviews(&details.reviews)
}

fn parse_gh_reviews(reviews: &[GhPrReview]) -> Vec<PrReview> {
    reviews
        .iter()
        .map(|object| PrReview {
            id: object.id.clone(),
            author: first_non_empty([object.author.login.as_str(), object.user.login.as_str()]),
            state: object.state.clone(),
            body: object.body.clone(),
            submitted_at: object.submitted_at.clone(),
        })
        .filter(|review| !review.state.trim().is_empty() || !review.body.trim().is_empty())
        .collect()
}

#[cfg(test)]
fn parse_requested_reviewers(raw: &str) -> Vec<String> {
    serde_json::from_str::<GithubPullRequest>(raw)
        .map(|node| requested_reviewers_from_requests(&node.review_requests))
        .unwrap_or_default()
}

fn requested_reviewers_from_requests(requests: &GithubReviewRequests) -> Vec<String> {
    let mut reviewers: Vec<String> = Vec::new();
    for request in requests.nodes() {
        let name = request
            .requested_reviewer
            .login
            .as_deref()
            .or(request.requested_reviewer.slug.as_deref())
            .or(request.requested_reviewer.name.as_deref())
            .unwrap_or_default()
            .trim();
        if name.is_empty() || reviewers.iter().any(|existing| existing == name) {
            continue;
        }
        reviewers.push(name.to_string());
    }
    reviewers
}

#[cfg(test)]
fn parse_inline_review_comments(raw: &str) -> Vec<PrReviewComment> {
    #[derive(Default, Deserialize)]
    struct InlineComment {
        #[serde(default)]
        id: String,
        #[serde(default)]
        user: GhActor,
        #[serde(default)]
        path: String,
        line: Option<u64>,
        #[serde(default, rename = "original_line")]
        original_line: Option<u64>,
        #[serde(default)]
        body: String,
        #[serde(default, rename = "created_at")]
        created_at: String,
    }
    let Ok(comments) = serde_json::from_str::<Vec<InlineComment>>(raw) else {
        return Vec::new();
    };
    comments
        .into_iter()
        .map(|object| PrReviewComment {
            thread_id: String::new(),
            id: object.id,
            author: object.user.login,
            path: object.path,
            line: object
                .line
                .or(object.original_line)
                .map(|line| line.to_string())
                .unwrap_or_default(),
            body: object.body,
            created_at: object.created_at,
            resolved: false,
        })
        .filter(|comment| !comment.body.trim().is_empty())
        .take(100)
        .collect()
}

#[cfg(test)]
fn parse_review_thread_comments(raw: &str) -> Vec<PrReviewComment> {
    try_parse_review_thread_comments(raw).unwrap_or_default()
}

fn try_parse_review_thread_comments(raw: &str) -> Result<Vec<PrReviewComment>, String> {
    let value = serde_json::from_str::<serde_json::Value>(raw)
        .map_err(|error| format!("parse GitHub review threads: {error}"))?;
    let pages = match value {
        serde_json::Value::Array(pages) => pages,
        page => vec![page],
    };
    if pages.is_empty() {
        return Err("parse GitHub review threads: missing pagination envelope".to_string());
    }
    let page_count = pages.len();
    let mut review_threads = Vec::new();
    let mut total_count = None;
    for (page_index, page) in pages.into_iter().enumerate() {
        if !page
            .pointer("/data/repository/pullRequest/reviewThreads/nodes")
            .is_some_and(serde_json::Value::is_array)
        {
            return Err(
                "parse GitHub review threads: missing review thread connection".to_string(),
            );
        }
        let Some(page_total_count) = page
            .pointer("/data/repository/pullRequest/reviewThreads/totalCount")
            .and_then(serde_json::Value::as_u64)
        else {
            return Err("parse GitHub review threads: missing total count".to_string());
        };
        let Some(has_next_page) = page
            .pointer("/data/repository/pullRequest/reviewThreads/pageInfo/hasNextPage")
            .and_then(serde_json::Value::as_bool)
        else {
            return Err("parse GitHub review threads: missing pagination metadata".to_string());
        };
        if has_next_page != (page_index + 1 < page_count) {
            return Err("parse GitHub review threads: incomplete pagination sequence".to_string());
        }
        if total_count
            .replace(page_total_count)
            .is_some_and(|total| total != page_total_count)
        {
            return Err("parse GitHub review threads: inconsistent total count".to_string());
        }
        let Some(threads) = page
            .pointer("/data/repository/pullRequest/reviewThreads/nodes")
            .and_then(serde_json::Value::as_array)
        else {
            unreachable!("review thread nodes were validated above");
        };
        for thread in threads {
            let Some(comment_count) = thread
                .pointer("/comments/totalCount")
                .and_then(serde_json::Value::as_u64)
            else {
                return Err("parse GitHub review threads: missing thread comment count".to_string());
            };
            let Some(comments_have_next_page) = thread
                .pointer("/comments/pageInfo/hasNextPage")
                .and_then(serde_json::Value::as_bool)
            else {
                return Err(
                    "parse GitHub review threads: missing thread comment pagination".to_string(),
                );
            };
            let observed_comments = thread
                .pointer("/comments/nodes")
                .and_then(serde_json::Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            if comments_have_next_page || observed_comments as u64 != comment_count {
                return Err(format!(
                    "GitHub returned only {observed_comments} of {comment_count} comments for a review thread"
                ));
            }
        }
        let response = serde_json::from_value::<GithubPrSummaryIndexResponse>(page)
            .map_err(|error| format!("parse GitHub review threads: {error}"))?;
        let page_threads = response.data.repository.pull_request.review_threads;
        review_threads.extend(page_threads.nodes);
    }
    let total_count = total_count.unwrap_or(0);
    if total_count != review_threads.len() as u64 {
        return Err(format!(
            "GitHub returned only {} of {} review threads",
            review_threads.len(),
            total_count
        ));
    }
    let mut comments = Vec::new();
    let mut observed_thread_ids = Vec::new();
    for thread in review_threads {
        if thread.id.trim().is_empty()
            || observed_thread_ids
                .iter()
                .any(|observed| observed == &thread.id)
        {
            return Err(
                "parse GitHub review threads: missing or duplicate thread identity".to_string(),
            );
        }
        observed_thread_ids.push(thread.id.clone());
        if thread.comments.page_info.has_next_page
            || thread.comments.total_count != thread.comments.nodes.len() as u64
        {
            return Err(format!(
                "GitHub returned only {} of {} comments for review thread {}",
                thread.comments.nodes.len(),
                thread.comments.total_count,
                thread.id
            ));
        }
        for object in thread.comments.nodes {
            let comment = PrReviewComment {
                thread_id: thread.id.clone(),
                id: object.id,
                author: object.author.login,
                path: object.path,
                line: object
                    .line
                    .or(object.original_line)
                    .map(|line| line.to_string())
                    .unwrap_or_default(),
                body: object.body,
                created_at: object.created_at,
                resolved: thread.is_resolved,
            };
            if !comment.body.trim().is_empty() {
                comments.push(comment);
            }
        }
    }
    Ok(comments)
}

#[cfg(test)]
pub fn parse_check_status(raw: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return "unknown".to_string();
    };
    let mut contexts = Vec::new();
    collect_status_contexts_from_value(&value, &mut contexts);
    check_status_from_contexts(&contexts)
}

fn check_status_from_contexts(contexts: &[GithubStatusContext]) -> String {
    let statuses = contexts
        .iter()
        .filter_map(|context| context.status.as_deref())
        .collect::<Vec<_>>();
    let conclusions = contexts
        .iter()
        .filter_map(|context| context.conclusion.as_deref())
        .collect::<Vec<_>>();
    let states = contexts
        .iter()
        .filter_map(|context| context.state.as_deref())
        .filter(|value| !matches!(*value, "OPEN" | "CLOSED" | "MERGED"))
        .collect::<Vec<_>>();
    if conclusions.iter().any(|value| {
        matches!(
            *value,
            "FAILURE" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED"
        )
    }) || states
        .iter()
        .any(|value| matches!(*value, "ERROR" | "FAILURE"))
    {
        return "failed".to_string();
    }
    if statuses
        .iter()
        .any(|value| matches!(*value, "QUEUED" | "IN_PROGRESS" | "PENDING" | "REQUESTED"))
        || states.contains(&"PENDING")
    {
        return "running".to_string();
    }
    let conclusions_pass = conclusions
        .iter()
        .all(|value| matches!(*value, "SUCCESS" | "SKIPPED" | "NEUTRAL"));
    let states_pass = states.iter().all(|value| *value == "SUCCESS");
    if (!conclusions.is_empty() || !states.is_empty()) && conclusions_pass && states_pass {
        return "passed".to_string();
    }
    if statuses.is_empty() && conclusions.is_empty() && states.is_empty() {
        "unknown".to_string()
    } else {
        "mixed".to_string()
    }
}

fn collect_failing_checks(rollup: &GithubStatusCheckRollup) -> Vec<String> {
    collect_failing_checks_from_contexts(&status_contexts_from_rollup(rollup))
}

fn collect_failing_checks_from_contexts(contexts: &[GithubStatusContext]) -> Vec<String> {
    contexts
        .iter()
        .filter_map(|context| {
            (context
                .conclusion
                .as_deref()
                .is_some_and(is_failure_conclusion)
                || context.state.as_deref().is_some_and(|state| {
                    matches!(state.to_ascii_uppercase().as_str(), "FAILURE" | "ERROR")
                }))
            .then(|| context.name.clone().or_else(|| context.context.clone()))
            .flatten()
        })
        .collect()
}

fn collect_check_contexts(rollup: &GithubStatusCheckRollup) -> Vec<PrCheckContext> {
    collect_check_contexts_from_contexts(&status_contexts_from_rollup(rollup))
}

fn collect_check_contexts_from_contexts(contexts: &[GithubStatusContext]) -> Vec<PrCheckContext> {
    contexts
        .iter()
        .filter_map(|context| {
            let name = context.name.clone().or(context.context.clone())?;
            let name = name.trim().to_string();
            if name.is_empty() {
                return None;
            }
            Some(PrCheckContext {
                name,
                state: check_context_state(context),
            })
        })
        .collect()
}

fn check_context_state(context: &GithubStatusContext) -> PrCheckState {
    let conclusion = context
        .conclusion
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_ascii_uppercase();
    if matches!(
        conclusion.as_str(),
        "FAILURE" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED"
    ) {
        return PrCheckState::Failed;
    }
    if matches!(conclusion.as_str(), "SUCCESS" | "SKIPPED" | "NEUTRAL") {
        return PrCheckState::Success;
    }

    let status = context
        .status
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_ascii_uppercase();
    if matches!(
        status.as_str(),
        "QUEUED" | "IN_PROGRESS" | "PENDING" | "REQUESTED"
    ) {
        return PrCheckState::Pending;
    }

    match context
        .state
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_ascii_uppercase()
        .as_str()
    {
        "SUCCESS" => PrCheckState::Success,
        "FAILURE" | "ERROR" => PrCheckState::Failed,
        "PENDING" => PrCheckState::Pending,
        _ => PrCheckState::Unknown,
    }
}

fn parse_merged_status(raw: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return false;
    };
    value
        .get("merged")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or_else(|| {
            value
                .get("mergedAt")
                .and_then(serde_json::Value::as_str)
                .map(|value| !value.trim().is_empty())
                .unwrap_or_else(|| {
                    value
                        .get("state")
                        .and_then(serde_json::Value::as_str)
                        .map(|state| state == "MERGED")
                        .unwrap_or(false)
                })
        })
}

fn merged_status_from_node(node: &GithubPullRequest) -> bool {
    node.merged.unwrap_or_else(|| {
        node.merged_at
            .as_deref()
            .map(|value| !value.trim().is_empty())
            .unwrap_or_else(|| node.state == "MERGED")
    })
}

fn status_contexts_for_pr(node: &GithubPullRequest) -> Vec<GithubStatusContext> {
    if node.status_check_rollup.observed {
        return status_contexts_from_rollup(&node.status_check_rollup);
    }
    node.commits
        .nodes
        .iter()
        .filter(|node| node.commit.status_check_rollup.observed)
        .flat_map(|node| status_contexts_from_rollup(&node.commit.status_check_rollup))
        .collect()
}

fn check_status_for_pr(node: &GithubPullRequest) -> String {
    let observed = node.status_check_rollup.observed
        || node
            .commits
            .nodes
            .iter()
            .any(|node| node.commit.status_check_rollup.observed);
    let contexts = status_contexts_for_pr(node);
    if observed && contexts.is_empty() {
        "passed".to_string()
    } else {
        check_status_from_contexts(&contexts)
    }
}

fn status_contexts_from_rollup(rollup: &GithubStatusCheckRollup) -> Vec<GithubStatusContext> {
    rollup
        .contexts
        .nodes
        .iter()
        .chain(rollup.nodes.iter())
        .cloned()
        .collect()
}

#[cfg(test)]
fn collect_status_contexts_from_value(
    value: &serde_json::Value,
    contexts: &mut Vec<GithubStatusContext>,
) {
    if contexts.len() >= 64 {
        return;
    }
    match value {
        serde_json::Value::Object(object)
            if object.contains_key("status")
                || object.contains_key("conclusion")
                || object.contains_key("state") =>
        {
            if let Ok(context) = serde_json::from_value::<GithubStatusContext>(value.clone()) {
                contexts.push(context);
            }
        }
        serde_json::Value::Object(object) => {
            for value in object.values() {
                collect_status_contexts_from_value(value, contexts);
                if contexts.len() >= 64 {
                    break;
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_status_contexts_from_value(value, contexts);
                if contexts.len() >= 64 {
                    break;
                }
            }
        }
        _ => {}
    }
}

fn first_non_empty<const N: usize>(values: [&str; N]) -> String {
    values
        .into_iter()
        .find(|value| !value.trim().is_empty())
        .unwrap_or_default()
        .to_string()
}

fn unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}
