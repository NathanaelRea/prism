use std::process::Command;
use std::time::{Duration, Instant};

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::observability;
use crate::process::{run_capture, run_output_allow_failure};
use crate::repo::Repository;
use crate::session::Session;
use crate::util::{strip_ansi, timestamp_label};

pub const PR_SUMMARY_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);
pub const PR_DETAIL_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
const PR_MERGE_VERIFY_ATTEMPTS: usize = 6;
const PR_MERGE_VERIFY_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, Default)]
pub struct PrCache {
    pub summary: Option<PrSummary>,
    pub details: Option<PrDetails>,
    pub last_polled: Option<Instant>,
    pub details_last_polled: Option<Instant>,
    pub last_refreshed: Option<String>,
    pub signature: Option<String>,
    pub error: Option<String>,
}

pub(crate) struct PrCacheRepository<'a> {
    pub repo: &'a Repository,
    pub config: &'a Config,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrCacheSummaryMutation {
    SaveSummary,
    RemoveSummary,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrSummary {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub url: String,
    pub state: String,
    pub review_decision: String,
    pub requested_reviewers: Vec<String>,
    pub head_ref: String,
    pub base_ref: String,
    pub head_sha: String,
    pub updated_at: String,
    pub check_status: String,
    pub merge_state_status: String,
    pub comment_count: u64,
    pub merged: bool,
    pub draft: bool,
}

impl PrSummary {
    pub fn signature(&self) -> String {
        format!(
            "{}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
            self.number,
            self.state,
            self.review_decision,
            self.requested_reviewers.join(","),
            self.body,
            self.head_sha,
            self.updated_at,
            self.check_status,
            self.merge_state_status,
            self.comment_count
        )
    }

    pub fn check_state(&self) -> PrCheckState {
        PrCheckState::from_label(&self.check_status)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrCheckState {
    Pending,
    Success,
    Failed,
    Mixed,
    Unknown,
}

impl PrCheckState {
    pub fn from_label(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "running" | "pending" => Self::Pending,
            "passed" | "success" => Self::Success,
            "failed" | "failure" => Self::Failed,
            "mixed" => Self::Mixed,
            _ => Self::Unknown,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "running",
            Self::Success => "passed",
            Self::Failed => "failed",
            Self::Mixed => "mixed",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct PrDetails {
    pub comments: Vec<PrComment>,
    pub reviews: Vec<PrReview>,
    pub review_comments: Vec<PrReviewComment>,
    pub files: Vec<String>,
    pub failing_checks: Vec<String>,
    pub ci_failures: Vec<CiFailure>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct CiFailure {
    pub workflow: String,
    pub name: String,
    pub conclusion: String,
    pub url: String,
    pub run_id: String,
    pub log_tail: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct PrComment {
    #[serde(default)]
    pub id: String,
    pub author: String,
    pub body: String,
    #[serde(default)]
    pub created_at: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct PrReview {
    #[serde(default)]
    pub id: String,
    pub author: String,
    pub state: String,
    pub body: String,
    #[serde(default)]
    pub submitted_at: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct PrReviewComment {
    #[serde(default)]
    pub id: String,
    pub author: String,
    pub path: String,
    pub line: String,
    pub body: String,
    pub created_at: String,
    pub resolved: bool,
}

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
struct GithubPullRequestConnection {
    #[serde(default)]
    nodes: Vec<GithubPullRequest>,
}

#[derive(Debug, Default, Deserialize)]
struct GithubPullRequest {
    number: Option<u64>,
    #[serde(default)]
    title: String,
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
    #[serde(default)]
    merged: Option<bool>,
    #[serde(default, rename = "mergedAt")]
    merged_at: Option<String>,
    #[serde(default, rename = "isDraft")]
    is_draft: bool,
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
    #[serde(default)]
    nodes: Vec<GithubReviewThread>,
}

#[derive(Debug, Default, Deserialize)]
struct GithubReviewThread {
    #[serde(default, rename = "isResolved")]
    is_resolved: bool,
    #[serde(default)]
    comments: GithubReviewThreadCommentConnection,
}

#[derive(Debug, Default, Deserialize)]
struct GithubReviewThreadCommentConnection {
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
    #[serde(default, rename = "createdAt")]
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
    #[serde(default)]
    id: String,
    #[serde(default)]
    author: GhActor,
    #[serde(default)]
    user: GhActor,
    #[serde(default)]
    body: String,
    #[serde(default, rename = "createdAt")]
    created_at: String,
}

#[derive(Debug, Default, Deserialize)]
struct GhPrReview {
    #[serde(default)]
    id: String,
    #[serde(default)]
    author: GhActor,
    #[serde(default)]
    user: GhActor,
    #[serde(default)]
    state: String,
    #[serde(default)]
    body: String,
    #[serde(default, rename = "submittedAt")]
    submitted_at: String,
}

#[derive(Debug, Default, Deserialize)]
struct GhActor {
    #[serde(default)]
    login: String,
}

#[derive(Debug, Default, Deserialize)]
struct GhPrFile {
    #[serde(default)]
    path: String,
}

#[derive(Debug, Default, Deserialize)]
struct GhRunListItem {
    #[serde(default, rename = "databaseId")]
    database_id: u64,
    #[serde(default, rename = "workflowName")]
    workflow_name: String,
    #[serde(default, rename = "displayTitle")]
    display_title: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    conclusion: String,
    #[serde(default)]
    status: String,
    #[serde(default, rename = "headSha")]
    head_sha: String,
    #[serde(default)]
    url: String,
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
        return Ok(GithubStatusCheckRollup::default());
    }
    if let Ok(nodes) = serde_json::from_value::<Vec<GithubStatusContext>>(value.clone()) {
        return Ok(GithubStatusCheckRollup {
            contexts: GithubStatusContextConnection::default(),
            nodes,
        });
    }
    serde_json::from_value(value).map_err(serde::de::Error::custom)
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

pub fn load_pr_cache(repo: &Repository, branch: &str) -> PrCache {
    let Ok((summary, last_refreshed)) = observability::with_writable_db(repo, |conn| {
        conn.query_row(
            "select
                number, title, body, url, state, review_decision, requested_reviewers,
                head_ref, base_ref, head_sha, updated_at, check_status, merge_state_status,
                comment_count, merged, draft, last_refreshed
              from pr_cache
              where branch = ?1",
            params![branch],
            |row| {
                Ok((
                    PrSummary {
                        number: row_u64(row, 0)?,
                        title: row.get(1)?,
                        body: row.get(2)?,
                        url: row.get(3)?,
                        state: row.get(4)?,
                        review_decision: row.get(5)?,
                        requested_reviewers: decode_requested_reviewers(&row.get::<_, String>(6)?),
                        head_ref: row.get(7)?,
                        base_ref: row.get(8)?,
                        head_sha: row.get(9)?,
                        updated_at: row.get(10)?,
                        check_status: row.get(11)?,
                        merge_state_status: row.get(12)?,
                        comment_count: row_u64(row, 13)?,
                        merged: row.get(14)?,
                        draft: row.get(15)?,
                    },
                    row.get::<_, String>(16)?,
                ))
            },
        )
        .map_err(|error| format!("read PR cache: {error}"))
    }) else {
        return PrCache::default();
    };
    let details = load_pr_details_cache(repo, branch);
    let signature = Some(summary.signature());
    PrCache {
        summary: Some(summary),
        details,
        last_refreshed: Some(last_refreshed),
        signature,
        ..PrCache::default()
    }
}

pub(crate) fn load_pr_cache_for_branch(
    repo: &Repository,
    config: &Config,
    branch: &str,
) -> PrCache {
    if pr_cache_excluded_branch(config, branch) {
        let _ = remove_pr_cache(repo, branch);
        return PrCache::default();
    }
    load_pr_cache(repo, branch)
}

pub fn refresh_pr_cache(
    repo: &Repository,
    branch: &str,
    cache: &mut PrCache,
    path: &std::path::Path,
    config: &Config,
    force_details: bool,
) {
    cache.last_polled = Some(Instant::now());
    if pr_cache_excluded_branch(config, branch) {
        let mutation = apply_pr_summary_refresh(cache, None, timestamp_label());
        persist_pr_summary_mutation(repo, branch, cache, mutation);
        return;
    }
    let result = fetch_pr_summary(path, branch, config);
    match result {
        Ok(Some((summary, _raw))) => {
            apply_pr_summary_refresh(cache, Some(summary), timestamp_label());
            if force_details || pr_details_due(cache) {
                refresh_pr_details_cache(branch, cache, path, config);
            }
            persist_pr_summary_mutation(repo, branch, cache, PrCacheSummaryMutation::SaveSummary);
        }
        Ok(None) => {
            let mutation = apply_pr_summary_refresh(cache, None, timestamp_label());
            persist_pr_summary_mutation(repo, branch, cache, mutation);
        }
        Err(error) => {
            cache.error = Some(error);
        }
    }
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

fn fetch_pr_merged_status(
    path: &std::path::Path,
    pr_number: u64,
    config: &Config,
) -> Result<bool, String> {
    let output = run_output_allow_failure(
        Command::new(config.tool("gh"))
            .arg("pr")
            .arg("view")
            .arg(pr_number.to_string())
            .arg("--json")
            .arg("state,mergedAt")
            .current_dir(path),
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
    branch: &str,
    cache: &mut PrCache,
    path: &std::path::Path,
    config: &Config,
) {
    cache.details_last_polled = Some(Instant::now());
    if pr_cache_excluded_branch(config, branch) {
        cache.details = None;
        cache.error = None;
        return;
    }
    let Some(summary) = &cache.summary else {
        cache.details = None;
        return;
    };
    match fetch_pr_details(path, branch, summary.number, &summary.head_sha, config) {
        Ok(details) => {
            cache.details = Some(details);
            cache.error = None;
        }
        Err(error) => cache.error = Some(error),
    }
}

pub(crate) fn apply_pr_details_poll_result(
    repo: &Repository,
    branch: &str,
    cache: &mut PrCache,
    poll_result: PrCache,
) -> bool {
    let current_pr = cache.summary.as_ref().map(|summary| summary.number);
    let result_pr = poll_result.summary.as_ref().map(|summary| summary.number);
    if current_pr != result_pr {
        return false;
    }
    cache.details = poll_result.details;
    cache.details_last_polled = poll_result.details_last_polled;
    cache.error = poll_result.error;
    if let Some(details) = &cache.details {
        let _ = save_pr_details_cache(repo, branch, details);
    }
    true
}

pub(crate) fn refresh_pr_summary_index_for_sessions(
    repos: &[PrCacheRepository<'_>],
    sessions: &mut [Session],
    repo_index: usize,
    summaries: Vec<PrSummary>,
    poll_started_at: Instant,
) {
    let Some(managed) = repos.get(repo_index) else {
        return;
    };
    let now = Instant::now();
    let refreshed = timestamp_label();
    for session in sessions
        .iter_mut()
        .filter(|session| session.repo_index == repo_index && !session.hidden)
    {
        if session
            .pr
            .last_polled
            .is_some_and(|last_polled| last_polled > poll_started_at)
        {
            continue;
        }
        session.pr.last_polled = Some(now);
        let summary = if pr_cache_excluded_branch(managed.config, &session.branch) {
            None
        } else {
            summaries
                .iter()
                .find(|summary| summary.head_ref == session.branch)
                .cloned()
        };
        let mutation = apply_pr_summary_refresh(&mut session.pr, summary, refreshed.clone());
        persist_pr_summary_mutation(managed.repo, &session.branch, &session.pr, mutation);
    }
}

pub fn pr_details_due(cache: &PrCache) -> bool {
    if cache.summary.is_none() {
        return false;
    }
    if cache.details.is_none() {
        return true;
    }
    cache
        .details_last_polled
        .map(|last| last.elapsed() >= PR_DETAIL_POLL_INTERVAL)
        .unwrap_or(true)
}

pub(crate) fn pr_cache_excluded_branch(config: &Config, branch: &str) -> bool {
    branch == "(detached)" || config.is_default_branch(branch)
}

pub(crate) fn pr_cache_pollable(config: &Config, branch: &str, cache: &PrCache) -> bool {
    !pr_cache_excluded_branch(config, branch)
        && !cache.summary.as_ref().is_some_and(|summary| summary.merged)
}

pub(crate) fn pr_details_pollable(config: &Config, branch: &str, cache: &PrCache) -> bool {
    pr_cache_pollable(config, branch, cache) && pr_details_due(cache)
}

pub(crate) fn github_remote_configured(path: &std::path::Path, config: &Config) -> bool {
    run_output_allow_failure(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(path)
            .args(["remote", "get-url", "origin"]),
    )
    .ok()
    .filter(|output| output.status.success())
    .and_then(|output| parse_github_remote(output.stdout.trim()))
    .is_some()
}

pub(crate) fn github_remote_repo(
    path: &std::path::Path,
    config: &Config,
    remote_name: &str,
) -> Result<String, String> {
    let (owner, name) = github_remote_owner_repo(path, config, remote_name)?;
    Ok(format!("{owner}/{name}"))
}

pub(crate) fn pr_summary_or_error(cache: &PrCache) -> Result<Option<PrSummary>, String> {
    if let Some(summary) = &cache.summary {
        Ok(Some(summary.clone()))
    } else if let Some(error) = &cache.error {
        Err(error.clone())
    } else {
        Ok(None)
    }
}

pub(crate) fn pr_cache_render_signature(cache: &PrCache) -> String {
    format!(
        "{:?}|{:?}|{:?}|{:?}",
        cache.summary, cache.details, cache.last_refreshed, cache.error
    )
}

pub(crate) fn pr_cache_comment_count(cache: &PrCache) -> usize {
    cache
        .details
        .as_ref()
        .map(|details| details.comments.len() + details.review_comments.len())
        .or_else(|| {
            cache
                .summary
                .as_ref()
                .map(|summary| summary.comment_count as usize)
        })
        .unwrap_or(0)
}

#[cfg(test)]
pub(crate) fn pr_cache_has_comments(cache: &PrCache) -> bool {
    pr_cache_comment_count(cache) > 0
}

fn apply_pr_summary_refresh(
    cache: &mut PrCache,
    summary: Option<PrSummary>,
    refreshed: String,
) -> PrCacheSummaryMutation {
    match summary {
        Some(summary) => {
            let signature = summary.signature();
            if cache.signature.as_deref() != Some(signature.as_str()) {
                cache.details = None;
                cache.details_last_polled = None;
            }
            cache.summary = Some(summary);
            cache.signature = Some(signature);
            cache.error = None;
            cache.last_refreshed = Some(refreshed);
            PrCacheSummaryMutation::SaveSummary
        }
        None => {
            cache.summary = None;
            cache.details = None;
            cache.details_last_polled = None;
            cache.signature = None;
            cache.error = None;
            cache.last_refreshed = Some(refreshed);
            PrCacheSummaryMutation::RemoveSummary
        }
    }
}

fn persist_pr_summary_mutation(
    repo: &Repository,
    branch: &str,
    cache: &PrCache,
    mutation: PrCacheSummaryMutation,
) {
    match mutation {
        PrCacheSummaryMutation::SaveSummary => {
            let _ = save_pr_cache(repo, branch, cache);
            if let Some(details) = &cache.details {
                let _ = save_pr_details_cache(repo, branch, details);
            } else {
                let _ = remove_pr_details_cache(repo, branch);
            }
        }
        PrCacheSummaryMutation::RemoveSummary => {
            let _ = remove_pr_cache(repo, branch);
        }
    }
}

pub fn fetch_pr_summary_index(
    path: &std::path::Path,
    config: &Config,
) -> Result<Vec<PrSummary>, String> {
    let (owner, name) = github_owner_repo(path, config)?;
    let raw = run_capture(
        Command::new(config.tool("gh"))
            .arg("api")
            .arg("graphql")
            .arg("-F")
            .arg(format!("owner={owner}"))
            .arg("-F")
            .arg(format!("name={name}"))
            .arg("-f")
            .arg(format!("query={PR_SUMMARY_INDEX_QUERY}"))
            .current_dir(path),
    )?;
    Ok(parse_pr_summary_index(&raw))
}

const PR_SUMMARY_INDEX_QUERY: &str = r#"
query($owner: String!, $name: String!) {
  repository(owner: $owner, name: $name) {
    pullRequests(first: 100, orderBy: {field: UPDATED_AT, direction: DESC}) {
      nodes {
        number
        title
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
        updatedAt
        mergeStateStatus
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

fn github_owner_repo(path: &std::path::Path, config: &Config) -> Result<(String, String), String> {
    github_remote_owner_repo(path, config, "origin")
}

fn github_remote_owner_repo(
    path: &std::path::Path,
    config: &Config,
    remote_name: &str,
) -> Result<(String, String), String> {
    let remote = run_capture(Command::new(config.tool("git")).arg("-C").arg(path).args([
        "remote",
        "get-url",
        remote_name,
    ]))?;
    parse_github_remote(remote.trim()).ok_or_else(|| {
        format!(
            "{remote_name} remote is not a GitHub repository: {}",
            remote.trim()
        )
    })
}

fn parse_github_remote(remote: &str) -> Option<(String, String)> {
    let path = remote
        .strip_prefix("git@github.com:")
        .or_else(|| remote.strip_prefix("ssh://git@github.com/"))
        .or_else(|| remote.strip_prefix("https://github.com/"))
        .or_else(|| remote.strip_prefix("http://github.com/"))?;
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.split('/');
    let owner = parts.next()?.to_string();
    let name = parts.next()?.to_string();
    if owner.is_empty() || name.is_empty() || parts.next().is_some() {
        None
    } else {
        Some((owner, name))
    }
}

pub fn parse_pr_summary_index(raw: &str) -> Vec<PrSummary> {
    let Ok(response) = serde_json::from_str::<GithubPrSummaryIndexResponse>(raw) else {
        return Vec::new();
    };
    response
        .data
        .repository
        .pull_requests
        .nodes
        .iter()
        .filter_map(pr_summary_from_node)
        .collect()
}

fn pr_summary_from_node(node: &GithubPullRequest) -> Option<PrSummary> {
    let number = node.number?;
    Some(PrSummary {
        number,
        title: node.title.clone(),
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
        check_status: check_status_from_contexts(&status_contexts_for_pr(node)),
        merge_state_status: node.merge_state_status.clone(),
        comment_count: node.comments.total_count + node.review_threads.total_count,
        merged: merged_status_from_node(node),
        draft: node.is_draft,
    })
}

fn fetch_pr_summary(
    path: &std::path::Path,
    branch: &str,
    config: &Config,
) -> Result<Option<(PrSummary, String)>, String> {
    if branch == "(detached)" {
        return Ok(None);
    }
    let fields = [
        "number",
        "title",
        "body",
        "url",
        "state",
        "reviewDecision",
        "reviewRequests",
        "headRefName",
        "baseRefName",
        "headRefOid",
        "updatedAt",
        "statusCheckRollup",
        "mergeStateStatus",
        "mergedAt",
        "isDraft",
    ]
    .join(",");
    let output = run_output_allow_failure(
        Command::new(config.tool("gh"))
            .arg("pr")
            .arg("view")
            .arg(branch)
            .arg("--json")
            .arg(fields)
            .current_dir(path),
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
    let Ok(node) = serde_json::from_str::<GithubPullRequest>(&raw) else {
        return Ok(None);
    };
    let Some(summary) = pr_summary_from_node(&node) else {
        return Ok(None);
    };
    Ok(Some((summary, raw)))
}

fn fetch_pr_details(
    path: &std::path::Path,
    branch: &str,
    pr_number: u64,
    head_sha: &str,
    config: &Config,
) -> Result<PrDetails, String> {
    let fields = ["comments", "reviews", "files", "statusCheckRollup"].join(",");
    let raw = run_capture(
        Command::new(config.tool("gh"))
            .arg("pr")
            .arg("view")
            .arg(branch)
            .arg("--json")
            .arg(fields)
            .current_dir(path),
    )?;
    let mut details = parse_pr_details(&raw);
    details.review_comments =
        fetch_inline_review_comments(path, pr_number, config).unwrap_or_else(|_| Vec::new());
    if !details.failing_checks.is_empty() {
        details.ci_failures = fetch_ci_failures(path, branch, head_sha, config).unwrap_or_default();
    }
    Ok(details)
}

pub fn parse_pr_details(raw: &str) -> PrDetails {
    let Ok(details) = serde_json::from_str::<GhPrViewDetails>(raw) else {
        return PrDetails::default();
    };
    let comments = parse_pr_comments(&details);
    let reviews = parse_pr_reviews(&details);
    let failing_checks = collect_failing_checks(&details.status_check_rollup);
    PrDetails {
        comments,
        reviews,
        review_comments: Vec::new(),
        files: details
            .files
            .into_iter()
            .map(|file| file.path)
            .filter(|path| !path.trim().is_empty())
            .take(8)
            .collect(),
        failing_checks,
        ci_failures: Vec::new(),
    }
}

fn fetch_ci_failures(
    path: &std::path::Path,
    branch: &str,
    head_sha: &str,
    config: &Config,
) -> Result<Vec<CiFailure>, String> {
    let output = run_output_allow_failure(
        Command::new(config.tool("gh"))
            .arg("run")
            .arg("list")
            .arg("--branch")
            .arg(branch)
            .arg("--commit")
            .arg(head_sha)
            .arg("--limit")
            .arg("20")
            .arg("--json")
            .arg("databaseId,workflowName,displayTitle,name,conclusion,status,headSha,url")
            .current_dir(path),
    )?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    let runs = serde_json::from_str::<Vec<GhRunListItem>>(&output.stdout).unwrap_or_default();
    let mut failures = Vec::new();
    for run in runs {
        if failures.len() >= 4 {
            break;
        }
        if !run.head_sha.trim().is_empty() && run.head_sha != head_sha {
            continue;
        }
        if !is_failure_conclusion(&run.conclusion) {
            continue;
        }
        let run_id = run.database_id.to_string();
        let log_tail = fetch_failed_run_log_tail(path, &run_id, config).unwrap_or_default();
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
    run_id: &str,
    config: &Config,
) -> Result<String, String> {
    if run_id == "0" {
        return Ok(String::new());
    }
    let output = run_output_allow_failure(
        Command::new(config.tool("gh"))
            .arg("run")
            .arg("view")
            .arg(run_id)
            .arg("--log-failed")
            .current_dir(path),
    )?;
    if !output.status.success() {
        return Ok(String::new());
    }
    Ok(tail_lines(&strip_ansi(&output.stdout), 80))
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
    pr_number: u64,
    config: &Config,
) -> Result<Vec<PrReviewComment>, String> {
    let (owner, name) = github_owner_repo(path, config)?;
    let raw = run_capture(
        Command::new(config.tool("gh"))
            .arg("api")
            .arg("graphql")
            .arg("-F")
            .arg(format!("owner={owner}"))
            .arg("-F")
            .arg(format!("name={name}"))
            .arg("-F")
            .arg(format!("number={pr_number}"))
            .arg("-f")
            .arg(format!("query={PR_REVIEW_THREADS_QUERY}"))
            .current_dir(path),
    )?;
    Ok(parse_review_thread_comments(&raw))
}

const PR_REVIEW_THREADS_QUERY: &str = r#"
query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      reviewThreads(first: 100) {
        nodes {
          isResolved
          comments(first: 100) {
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
    details
        .comments
        .iter()
        .map(|object| PrComment {
            id: object.id.clone(),
            author: first_non_empty([object.author.login.as_str(), object.user.login.as_str()]),
            body: object.body.clone(),
            created_at: object.created_at.clone(),
        })
        .filter(|comment| !comment.body.trim().is_empty())
        .take(20)
        .collect()
}

fn parse_pr_reviews(details: &GhPrViewDetails) -> Vec<PrReview> {
    details
        .reviews
        .iter()
        .map(|object| PrReview {
            id: object.id.clone(),
            author: first_non_empty([object.author.login.as_str(), object.user.login.as_str()]),
            state: object.state.clone(),
            body: object.body.clone(),
            submitted_at: object.submitted_at.clone(),
        })
        .filter(|review| !review.state.trim().is_empty() || !review.body.trim().is_empty())
        .take(20)
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

pub fn parse_review_thread_comments(raw: &str) -> Vec<PrReviewComment> {
    let Ok(response) = serde_json::from_str::<GithubPrSummaryIndexResponse>(raw) else {
        return Vec::new();
    };
    let mut comments = Vec::new();
    for thread in response.data.repository.pull_request.review_threads.nodes {
        for object in thread.comments.nodes {
            if comments.len() >= 100 {
                return comments;
            }
            let comment = PrReviewComment {
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
    comments
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
    status_contexts_from_rollup(rollup)
        .into_iter()
        .filter_map(|context| {
            matches!(
                context.conclusion.as_deref().unwrap_or_default(),
                "FAILURE" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED"
            )
            .then(|| context.name.or(context.context))
            .flatten()
        })
        .take(8)
        .collect()
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
    if !node.status_check_rollup.contexts.nodes.is_empty()
        || !node.status_check_rollup.nodes.is_empty()
    {
        return status_contexts_from_rollup(&node.status_check_rollup);
    }
    node.commits
        .nodes
        .iter()
        .flat_map(|node| status_contexts_from_rollup(&node.commit.status_check_rollup))
        .collect()
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

pub(crate) fn migrate_pr_cache_schema(conn: &rusqlite::Connection) -> Result<(), String> {
    conn.execute_batch(
        "
        create table if not exists pr_cache (
          branch text primary key,
          number integer not null,
          title text not null,
          body text not null default '',
          url text not null,
          state text not null,
          review_decision text not null,
          requested_reviewers text not null default '',
          head_ref text not null,
          base_ref text not null,
          head_sha text not null,
          updated_at text not null,
          check_status text not null,
          merge_state_status text not null default '',
          comment_count integer not null default 0,
          merged integer not null,
          draft integer not null,
          last_refreshed text not null,
          refreshed_unix_ms integer not null
        );

        create table if not exists pr_details_cache (
          branch text primary key,
          comments text not null,
          reviews text not null,
          review_comments text not null,
          files text not null,
          failing_checks text not null,
          ci_failures text not null default '[]',
          refreshed_unix_ms integer not null
        );
        ",
    )
    .map_err(|error| format!("create PR cache schema: {error}"))?;
    if !table_has_column(conn, "pr_cache", "body")? {
        conn.execute(
            "alter table pr_cache add column body text not null default ''",
            [],
        )
        .map_err(|error| format!("migrate pr_cache body column: {error}"))?;
    }
    if !table_has_column(conn, "pr_cache", "comment_count")? {
        conn.execute(
            "alter table pr_cache add column comment_count integer not null default 0",
            [],
        )
        .map_err(|error| format!("migrate pr_cache comment_count column: {error}"))?;
    }
    if !table_has_column(conn, "pr_cache", "merge_state_status")? {
        conn.execute(
            "alter table pr_cache add column merge_state_status text not null default ''",
            [],
        )
        .map_err(|error| format!("migrate pr_cache merge_state_status column: {error}"))?;
    }
    if !table_has_column(conn, "pr_cache", "requested_reviewers")? {
        conn.execute(
            "alter table pr_cache add column requested_reviewers text not null default ''",
            [],
        )
        .map_err(|error| format!("migrate pr_cache requested_reviewers column: {error}"))?;
    }
    if !table_has_column(conn, "pr_details_cache", "ci_failures")? {
        conn.execute(
            "alter table pr_details_cache add column ci_failures text not null default '[]'",
            [],
        )
        .map_err(|error| format!("migrate pr_details_cache ci_failures column: {error}"))?;
    }
    Ok(())
}

fn table_has_column(
    conn: &rusqlite::Connection,
    table: &str,
    column: &str,
) -> Result<bool, String> {
    let mut statement = conn
        .prepare(&format!("pragma table_info({table})"))
        .map_err(|error| format!("prepare table info: {error}"))?;
    let mut rows = statement
        .query([])
        .map_err(|error| format!("read table info: {error}"))?;
    while let Some(row) = rows
        .next()
        .map_err(|error| format!("read column info: {error}"))?
    {
        let name = row
            .get::<_, String>(1)
            .map_err(|error| format!("read column name: {error}"))?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn remove_pr_cache(repo: &Repository, branch: &str) -> Result<(), String> {
    observability::with_writable_db(repo, |conn| remove_pr_cache_with_conn(conn, branch))
}

pub(crate) fn remove_pr_cache_with_conn(
    conn: &rusqlite::Connection,
    branch: &str,
) -> Result<(), String> {
    conn.execute("delete from pr_cache where branch = ?1", params![branch])
        .map_err(|error| format!("remove PR cache: {error}"))?;
    remove_pr_details_cache_with_conn(conn, branch)?;
    Ok(())
}

pub fn remove_pr_details_cache(repo: &Repository, branch: &str) -> Result<(), String> {
    observability::with_writable_db(repo, |conn| remove_pr_details_cache_with_conn(conn, branch))
}

fn remove_pr_details_cache_with_conn(
    conn: &rusqlite::Connection,
    branch: &str,
) -> Result<(), String> {
    conn.execute(
        "delete from pr_details_cache where branch = ?1",
        params![branch],
    )
    .map_err(|error| format!("remove PR details cache: {error}"))?;
    Ok(())
}

fn load_pr_details_cache(repo: &Repository, branch: &str) -> Option<PrDetails> {
    observability::with_writable_db(repo, |conn| {
        conn.query_row(
            "select comments, reviews, review_comments, files, failing_checks, ci_failures
               from pr_details_cache
              where branch = ?1",
            params![branch],
            |row| {
                Ok(PrDetails {
                    comments: decode_pr_comments(&row.get::<_, String>(0)?),
                    reviews: decode_pr_reviews(&row.get::<_, String>(1)?),
                    review_comments: decode_pr_review_comments(&row.get::<_, String>(2)?),
                    files: decode_string_values(&row.get::<_, String>(3)?),
                    failing_checks: decode_string_values(&row.get::<_, String>(4)?),
                    ci_failures: decode_ci_failures(&row.get::<_, String>(5)?),
                })
            },
        )
        .optional()
        .map_err(|error| format!("read PR details cache: {error}"))
    })
    .ok()
    .flatten()
}

pub fn save_pr_details_cache(
    repo: &Repository,
    branch: &str,
    details: &PrDetails,
) -> Result<(), String> {
    observability::with_writable_db(repo, |conn| {
        conn.execute(
            "insert into pr_details_cache (
                branch, comments, reviews, review_comments, files, failing_checks, ci_failures, refreshed_unix_ms
             ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
              on conflict(branch) do update set
                comments = excluded.comments,
                reviews = excluded.reviews,
                review_comments = excluded.review_comments,
                files = excluded.files,
                failing_checks = excluded.failing_checks,
                ci_failures = excluded.ci_failures,
                refreshed_unix_ms = excluded.refreshed_unix_ms",
            params![
                branch,
                encode_pr_comments(&details.comments),
                encode_pr_reviews(&details.reviews),
                encode_pr_review_comments(&details.review_comments),
                encode_string_values(&details.files),
                encode_string_values(&details.failing_checks),
                encode_ci_failures(&details.ci_failures),
                unix_seconds(),
            ],
        )
        .map_err(|error| format!("write PR details cache: {error}"))?;
        Ok(())
    })
}

pub fn save_pr_cache(repo: &Repository, branch: &str, cache: &PrCache) -> Result<(), String> {
    let Some(summary) = &cache.summary else {
        return Ok(());
    };
    let number = sqlite_i64(summary.number, "PR number")?;
    let comment_count = sqlite_i64(summary.comment_count, "PR comment count")?;
    observability::with_writable_db(repo, |conn| {
        conn.execute(
            "insert into pr_cache (
                branch, number, title, body, url, state, review_decision, requested_reviewers,
                head_ref, base_ref, head_sha, updated_at, check_status, merge_state_status,
                comment_count, merged, draft, last_refreshed, refreshed_unix_ms
             ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)
              on conflict(branch) do update set
                number = excluded.number,
                title = excluded.title,
                body = excluded.body,
                url = excluded.url,
                state = excluded.state,
                review_decision = excluded.review_decision,
                requested_reviewers = excluded.requested_reviewers,
                head_ref = excluded.head_ref,
                base_ref = excluded.base_ref,
                head_sha = excluded.head_sha,
                updated_at = excluded.updated_at,
                check_status = excluded.check_status,
                merge_state_status = excluded.merge_state_status,
                comment_count = excluded.comment_count,
                merged = excluded.merged,
                draft = excluded.draft,
                last_refreshed = excluded.last_refreshed,
                refreshed_unix_ms = excluded.refreshed_unix_ms",
            params![
                branch,
                number,
                summary.title.as_str(),
                summary.body.as_str(),
                summary.url.as_str(),
                summary.state.as_str(),
                summary.review_decision.as_str(),
                encode_requested_reviewers(&summary.requested_reviewers),
                summary.head_ref.as_str(),
                summary.base_ref.as_str(),
                summary.head_sha.as_str(),
                summary.updated_at.as_str(),
                summary.check_status.as_str(),
                summary.merge_state_status.as_str(),
                comment_count,
                summary.merged,
                summary.draft,
                cache.last_refreshed.as_deref().unwrap_or(""),
                unix_seconds(),
            ],
        )
        .map_err(|error| format!("write PR cache: {error}"))?;
        Ok(())
    })
}

fn encode_requested_reviewers(reviewers: &[String]) -> String {
    reviewers.join("\n")
}

fn decode_requested_reviewers(value: &str) -> Vec<String> {
    value
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

fn encode_pr_comments(comments: &[PrComment]) -> String {
    serde_json::to_string(comments).unwrap_or_else(|_| "[]".to_string())
}

fn decode_pr_comments(raw: &str) -> Vec<PrComment> {
    serde_json::from_str(raw).unwrap_or_default()
}

fn encode_pr_reviews(reviews: &[PrReview]) -> String {
    serde_json::to_string(reviews).unwrap_or_else(|_| "[]".to_string())
}

fn decode_pr_reviews(raw: &str) -> Vec<PrReview> {
    serde_json::from_str(raw).unwrap_or_default()
}

fn encode_pr_review_comments(comments: &[PrReviewComment]) -> String {
    serde_json::to_string(comments).unwrap_or_else(|_| "[]".to_string())
}

fn decode_pr_review_comments(raw: &str) -> Vec<PrReviewComment> {
    serde_json::from_str(raw).unwrap_or_default()
}

fn encode_ci_failures(failures: &[CiFailure]) -> String {
    let failures_without_logs: Vec<CiFailure> = failures
        .iter()
        .cloned()
        .map(|mut failure| {
            failure.log_tail.clear();
            failure
        })
        .collect();
    serde_json::to_string(&failures_without_logs).unwrap_or_else(|_| "[]".to_string())
}

fn decode_ci_failures(raw: &str) -> Vec<CiFailure> {
    serde_json::from_str(raw).unwrap_or_default()
}

fn encode_string_values(values: &[String]) -> String {
    serde_json::to_string(values).unwrap_or_else(|_| "[]".to_string())
}

fn decode_string_values(raw: &str) -> Vec<String> {
    serde_json::from_str(raw).unwrap_or_default()
}

fn row_u64(row: &rusqlite::Row<'_>, idx: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(idx)?;
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(idx, value))
}

fn sqlite_i64(value: u64, name: &str) -> Result<i64, String> {
    i64::try_from(value).map_err(|_| format!("{name} {value} exceeds SQLite integer range"))
}

fn unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Checks, Config, EscapeKey};
    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn pr_json_parser_reads_summary_details_and_missing_fields() {
        let raw = r#"{
            "number": 42,
            "title": "Fix review",
            "mergedAt": "2026-01-01T00:00:00Z",
            "isDraft": true,
            "comments": [{
                "id": "PRC_kw123",
                "author": {"login": "reviewer"},
                "body": "hello",
                "createdAt": "2026-01-01T00:00:00Z"
            }],
            "reviews": [{
                "id": "PRR_kw123",
                "author": {"login": "maintainer"},
                "state": "CHANGES_REQUESTED",
                "body": "review body",
                "submittedAt": "2026-01-01T00:01:00Z"
            }],
            "files": [{"path": "src/main.rs"}],
            "statusCheckRollup": {
                "contexts": {
                    "nodes": [{"name": "test", "status": "COMPLETED", "conclusion": "FAILURE"}]
                }
            }
        }"#;
        assert!(parse_merged_status(raw));
        assert_eq!(parse_check_status(raw), "failed");
        let details = parse_pr_details(raw);
        assert_eq!(details.files, vec!["src/main.rs"]);
        assert_eq!(details.failing_checks, vec!["test"]);
        assert_eq!(details.comments[0].id, "PRC_kw123");
        assert_eq!(details.comments[0].body, "hello");
        assert_eq!(details.comments[0].created_at, "2026-01-01T00:00:00Z");
        assert_eq!(details.reviews[0].id, "PRR_kw123");
        assert_eq!(details.reviews[0].state, "CHANGES_REQUESTED");
        assert_eq!(details.reviews[0].body, "review body");
        assert_eq!(details.reviews[0].submitted_at, "2026-01-01T00:01:00Z");
    }

    #[test]
    fn check_state_normalizes_display_labels_for_workflow_decisions() {
        assert_eq!(PrCheckState::from_label("running"), PrCheckState::Pending);
        assert_eq!(PrCheckState::from_label("pending"), PrCheckState::Pending);
        assert_eq!(PrCheckState::from_label("passed"), PrCheckState::Success);
        assert_eq!(PrCheckState::from_label("success"), PrCheckState::Success);
        assert_eq!(PrCheckState::from_label("failed"), PrCheckState::Failed);
        assert_eq!(PrCheckState::from_label("mixed"), PrCheckState::Mixed);
        assert_eq!(PrCheckState::from_label(""), PrCheckState::Unknown);
    }

    #[test]
    fn pr_cache_round_trips_details() {
        let temp = unique_temp_dir("prism-pr-details-cache-test");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let summary = PrSummary {
            number: 42,
            title: "Fix review".to_string(),
            body: "Body with \"quotes\"".to_string(),
            url: "https://github.com/example/repo/pull/42".to_string(),
            state: "OPEN".to_string(),
            review_decision: "CHANGES_REQUESTED".to_string(),
            requested_reviewers: vec!["alice".to_string()],
            head_ref: "feature".to_string(),
            base_ref: "main".to_string(),
            head_sha: "abc123".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            check_status: "failed".to_string(),
            merge_state_status: "CLEAN".to_string(),
            comment_count: 2,
            merged: false,
            draft: false,
        };
        let details = PrDetails {
            comments: vec![PrComment {
                author: "reviewer".to_string(),
                body: "please fix\nthis".to_string(),
                created_at: "2026-01-01T00:00:00Z".to_string(),
                ..PrComment::default()
            }],
            reviews: vec![PrReview {
                author: "maintainer".to_string(),
                state: "CHANGES_REQUESTED".to_string(),
                body: "needs work".to_string(),
                submitted_at: "2026-01-01T00:01:00Z".to_string(),
                ..PrReview::default()
            }],
            review_comments: vec![PrReviewComment {
                author: "reviewer".to_string(),
                path: "src/main.rs".to_string(),
                line: "12".to_string(),
                body: "inline note".to_string(),
                created_at: "2026-01-01T00:00:00Z".to_string(),
                resolved: true,
                ..PrReviewComment::default()
            }],
            files: vec!["src/main.rs".to_string()],
            failing_checks: vec!["test".to_string()],
            ci_failures: vec![CiFailure {
                workflow: "CI".to_string(),
                name: "test".to_string(),
                conclusion: "failure".to_string(),
                url: "https://github.com/example/repo/actions/runs/99".to_string(),
                run_id: "99".to_string(),
                log_tail: "failed log".to_string(),
            }],
        };
        let cache = PrCache {
            summary: Some(summary),
            details: Some(details),
            last_refreshed: Some("now".to_string()),
            ..PrCache::default()
        };

        save_pr_cache(&repo, "feature", &cache).unwrap();
        save_pr_details_cache(&repo, "feature", cache.details.as_ref().unwrap()).unwrap();
        let loaded = load_pr_cache(&repo, "feature");
        let prism_dir = repo.prism_dir();

        assert_eq!(loaded.summary.as_ref().unwrap().number, 42);
        assert_eq!(loaded.summary.as_ref().unwrap().merge_state_status, "CLEAN");
        let loaded_details = loaded.details.as_ref().unwrap();
        assert_eq!(loaded_details.comments[0].author, "reviewer");
        assert_eq!(loaded_details.comments[0].body, "please fix\nthis");
        assert_eq!(
            loaded_details.comments[0].created_at,
            "2026-01-01T00:00:00Z"
        );
        assert_eq!(loaded_details.reviews[0].state, "CHANGES_REQUESTED");
        assert_eq!(
            loaded_details.reviews[0].submitted_at,
            "2026-01-01T00:01:00Z"
        );
        assert_eq!(loaded_details.review_comments[0].path, "src/main.rs");
        assert!(loaded_details.review_comments[0].resolved);
        assert_eq!(loaded_details.files, vec!["src/main.rs"]);
        assert_eq!(loaded_details.failing_checks, vec!["test"]);
        assert_eq!(loaded_details.ci_failures[0].log_tail, "");

        let _ = fs::remove_dir_all(prism_dir);
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn pr_summary_refresh_preserves_details_when_signature_matches() {
        let summary = test_summary("feature", "abc123", 2);
        let details = PrDetails {
            review_comments: vec![PrReviewComment {
                author: "reviewer".to_string(),
                path: "src/main.rs".to_string(),
                line: "12".to_string(),
                body: "inline note".to_string(),
                created_at: "2026-01-01T00:00:00Z".to_string(),
                resolved: false,
                ..PrReviewComment::default()
            }],
            ..PrDetails::default()
        };
        let mut cache = PrCache {
            summary: Some(summary.clone()),
            details: Some(details),
            details_last_polled: Some(Instant::now()),
            signature: Some(summary.signature()),
            error: Some("previous error".to_string()),
            ..PrCache::default()
        };

        let mutation = apply_pr_summary_refresh(&mut cache, Some(summary), "now".to_string());

        assert_eq!(mutation, PrCacheSummaryMutation::SaveSummary);
        assert!(cache.details.is_some());
        assert!(cache.details_last_polled.is_some());
        assert_eq!(cache.error, None);
        assert_eq!(cache.last_refreshed.as_deref(), Some("now"));
    }

    #[test]
    fn pr_summary_refresh_drops_details_when_signature_changes() {
        let old_summary = test_summary("feature", "abc123", 2);
        let new_summary = test_summary("feature", "def456", 2);
        let mut cache = PrCache {
            summary: Some(old_summary.clone()),
            details: Some(PrDetails::default()),
            details_last_polled: Some(Instant::now()),
            signature: Some(old_summary.signature()),
            ..PrCache::default()
        };

        let mutation =
            apply_pr_summary_refresh(&mut cache, Some(new_summary.clone()), "now".to_string());

        assert_eq!(mutation, PrCacheSummaryMutation::SaveSummary);
        assert_eq!(cache.summary.as_ref(), Some(&new_summary));
        assert_eq!(
            cache.signature.as_deref(),
            Some(new_summary.signature().as_str())
        );
        assert!(cache.details.is_none());
        assert!(cache.details_last_polled.is_none());
    }

    #[test]
    fn pr_summary_refresh_clears_cache_when_branch_has_no_pr() {
        let summary = test_summary("feature", "abc123", 2);
        let mut cache = PrCache {
            summary: Some(summary.clone()),
            details: Some(PrDetails::default()),
            details_last_polled: Some(Instant::now()),
            signature: Some(summary.signature()),
            error: Some("previous error".to_string()),
            ..PrCache::default()
        };

        let mutation = apply_pr_summary_refresh(&mut cache, None, "now".to_string());

        assert_eq!(mutation, PrCacheSummaryMutation::RemoveSummary);
        assert!(cache.summary.is_none());
        assert!(cache.details.is_none());
        assert!(cache.details_last_polled.is_none());
        assert!(cache.signature.is_none());
        assert!(cache.error.is_none());
        assert_eq!(cache.last_refreshed.as_deref(), Some("now"));
    }

    #[test]
    fn pr_cache_pollability_excludes_default_detached_and_merged_branches() {
        let mut config = test_config();
        config.default_base = Some("main".to_string());
        let merged_summary = PrSummary {
            merged: true,
            ..test_summary("feature", "abc123", 0)
        };
        let merged_cache = PrCache {
            summary: Some(merged_summary),
            ..PrCache::default()
        };

        assert!(!pr_cache_pollable(&config, "main", &PrCache::default()));
        assert!(!pr_cache_pollable(
            &config,
            "(detached)",
            &PrCache::default()
        ));
        assert!(!pr_cache_pollable(&config, "feature", &merged_cache));
        assert!(pr_cache_pollable(&config, "feature", &PrCache::default()));
    }

    #[test]
    fn pr_cache_comment_count_prefers_loaded_details_over_summary() {
        let cache = PrCache {
            summary: Some(test_summary("feature", "abc123", 12)),
            details: Some(PrDetails {
                comments: vec![PrComment {
                    author: "reviewer".to_string(),
                    body: "top-level".to_string(),
                    ..PrComment::default()
                }],
                review_comments: vec![
                    PrReviewComment {
                        author: "reviewer".to_string(),
                        path: "src/main.rs".to_string(),
                        line: "10".to_string(),
                        body: "inline".to_string(),
                        created_at: "2026-01-01T00:00:00Z".to_string(),
                        resolved: false,
                        ..PrReviewComment::default()
                    },
                    PrReviewComment {
                        author: "reviewer".to_string(),
                        path: "src/lib.rs".to_string(),
                        line: "20".to_string(),
                        body: "resolved".to_string(),
                        created_at: "2026-01-02T00:00:00Z".to_string(),
                        resolved: true,
                        ..PrReviewComment::default()
                    },
                ],
                ..PrDetails::default()
            }),
            ..PrCache::default()
        };

        assert_eq!(pr_cache_comment_count(&cache), 3);
        assert!(pr_cache_has_comments(&cache));
    }

    #[test]
    fn pr_summary_index_refresh_updates_sessions_and_pr_cache_storage() {
        let temp = unique_temp_dir("prism-pr-summary-index-test");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let mut config = test_config();
        config.default_base = Some("main".to_string());
        let feature_summary = test_summary("feature", "abc123", 2);
        let stale_summary = test_summary("stale", "old", 1);
        let details = PrDetails {
            comments: vec![PrComment {
                author: "reviewer".to_string(),
                body: "new comment".to_string(),
                ..PrComment::default()
            }],
            ..PrDetails::default()
        };
        let mut sessions = vec![
            test_session(
                "main",
                PrCache {
                    summary: Some(test_summary("main", "main", 0)),
                    signature: Some(test_summary("main", "main", 0).signature()),
                    ..PrCache::default()
                },
            ),
            test_session(
                "feature",
                PrCache {
                    summary: Some(feature_summary.clone()),
                    details: Some(details.clone()),
                    details_last_polled: Some(Instant::now()),
                    signature: Some(feature_summary.signature()),
                    ..PrCache::default()
                },
            ),
            test_session(
                "stale",
                PrCache {
                    summary: Some(stale_summary.clone()),
                    signature: Some(stale_summary.signature()),
                    ..PrCache::default()
                },
            ),
        ];

        refresh_pr_summary_index_for_sessions(
            &[PrCacheRepository {
                repo: &repo,
                config: &config,
            }],
            &mut sessions,
            0,
            vec![feature_summary.clone()],
            Instant::now(),
        );

        assert!(sessions[0].pr.summary.is_none());
        assert!(sessions[2].pr.summary.is_none());
        assert_eq!(sessions[1].pr.summary.as_ref(), Some(&feature_summary));
        assert!(sessions[1].pr.details.is_some());

        let loaded = load_pr_cache(&repo, "feature");
        assert_eq!(loaded.summary.as_ref(), Some(&feature_summary));
        assert_eq!(
            loaded.details.as_ref().unwrap().comments[0].body,
            "new comment"
        );
        assert!(load_pr_cache(&repo, "stale").summary.is_none());

        let _ = fs::remove_dir_all(repo.prism_dir());
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn stale_pr_summary_index_refresh_does_not_clear_newer_direct_refresh() {
        let temp = unique_temp_dir("prism-stale-pr-summary-index-test");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let mut config = test_config();
        config.default_base = Some("main".to_string());
        let poll_started_at = Instant::now();
        let summary = test_summary("feature", "abc123", 2);
        let cache = PrCache {
            summary: Some(summary.clone()),
            last_polled: Some(poll_started_at + std::time::Duration::from_millis(1)),
            last_refreshed: Some("created".to_string()),
            signature: Some(summary.signature()),
            ..PrCache::default()
        };
        save_pr_cache(&repo, "feature", &cache).unwrap();
        let mut sessions = vec![test_session("feature", cache)];

        refresh_pr_summary_index_for_sessions(
            &[PrCacheRepository {
                repo: &repo,
                config: &config,
            }],
            &mut sessions,
            0,
            Vec::new(),
            poll_started_at,
        );

        assert_eq!(sessions[0].pr.summary.as_ref(), Some(&summary));
        assert_eq!(
            load_pr_cache(&repo, "feature").summary.as_ref(),
            Some(&summary)
        );

        let _ = fs::remove_dir_all(repo.prism_dir());
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn parses_graphql_pr_summary_index() {
        let raw = r#"{
          "data": {
            "repository": {
              "pullRequests": {
                "nodes": [
                  {
                    "number": 9,
                    "title": "Batch polling",
                    "body": "summary",
                    "url": "https://github.com/example/repo/pull/9",
                    "state": "OPEN",
                    "reviewDecision": null,
                    "reviewRequests": {
                      "nodes": [
                        {"requestedReviewer": {"__typename": "User", "login": "alice"}},
                        {"requestedReviewer": {"__typename": "Team", "slug": "backend"}}
                      ]
                    },
                    "headRefName": "feature",
                    "baseRefName": "main",
                    "headRefOid": "abc123",
                    "updatedAt": "2026-01-01T00:00:00Z",
                    "mergeStateStatus": "DIRTY",
                    "merged": false,
                    "isDraft": false,
                    "comments": {"totalCount": 2},
                    "reviewThreads": {"totalCount": 3},
                    "commits": {
                      "nodes": [
                        {
                          "commit": {
                            "statusCheckRollup": {
                              "contexts": {
                                "nodes": [
                                  {
                                    "__typename": "StatusContext",
                                    "context": "ci",
                                    "state": "SUCCESS"
                                  }
                                ]
                              }
                            }
                          }
                        }
                      ]
                    }
                  }
                ]
              }
            }
          }
        }"#;

        let summaries = parse_pr_summary_index(raw);

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].number, 9);
        assert_eq!(summaries[0].head_ref, "feature");
        assert_eq!(summaries[0].review_decision, "UNKNOWN");
        assert_eq!(summaries[0].requested_reviewers, vec!["alice", "backend"]);
        assert_eq!(summaries[0].comment_count, 5);
        assert_eq!(summaries[0].check_status, "passed");
        assert_eq!(summaries[0].merge_state_status, "DIRTY");
    }

    #[test]
    fn parses_requested_reviewers_from_gh_pr_view() {
        let raw = r#"{
          "reviewRequests": [
            {"requestedReviewer": {"login": "alice"}},
            {"requestedReviewer": {"slug": "backend"}},
            {"requestedReviewer": {"login": "alice"}}
          ]
        }"#;

        assert_eq!(parse_requested_reviewers(raw), vec!["alice", "backend"]);
    }

    #[test]
    fn parses_github_remote_urls() {
        assert_eq!(
            parse_github_remote("git@github.com:owner/repo.git"),
            Some(("owner".to_string(), "repo".to_string()))
        );
        assert_eq!(
            parse_github_remote("https://github.com/owner/repo"),
            Some(("owner".to_string(), "repo".to_string()))
        );
        assert_eq!(parse_github_remote("https://example.com/owner/repo"), None);
    }

    #[test]
    fn parses_inline_review_comments() {
        let raw = r#"[
            {
                "path": "src/main.rs",
                "line": 12,
                "id": "PRRC_kw123",
                "body": "please simplify",
                "created_at": "2026-01-01T00:00:00Z",
                "user": {"login": "reviewer"}
            }
        ]"#;
        let comments = parse_inline_review_comments(raw);
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].path, "src/main.rs");
        assert_eq!(comments[0].id, "PRRC_kw123");
        assert_eq!(comments[0].line, "12");
        assert_eq!(comments[0].author, "reviewer");
        assert_eq!(comments[0].created_at, "2026-01-01T00:00:00Z");
        assert!(!comments[0].resolved);
    }

    #[test]
    fn parses_review_thread_resolution_status() {
        let raw = r#"{
          "data": {
            "repository": {
              "pullRequest": {
                "reviewThreads": {
                  "nodes": [
                    {
                      "isResolved": true,
                      "comments": {
                        "nodes": [
                          {
                            "id": "PRRC_kw123",
                            "path": "src/main.rs",
                            "line": 12,
                            "body": "please simplify",
                            "createdAt": "2026-01-01T00:00:00Z",
                            "author": {"login": "reviewer"}
                          }
                        ]
                      }
                    },
                    {
                      "isResolved": false,
                      "comments": {
                        "nodes": [
                          {
                            "id": "PRRC_kw456",
                            "path": "src/lib.rs",
                            "originalLine": 20,
                            "body": "still needs work",
                            "createdAt": "2026-01-02T00:00:00Z",
                            "author": {"login": "maintainer"}
                          }
                        ]
                      }
                    }
                  ]
                }
              }
            }
          }
        }"#;

        let comments = parse_review_thread_comments(raw);

        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].author, "reviewer");
        assert_eq!(comments[0].id, "PRRC_kw123");
        assert_eq!(comments[0].path, "src/main.rs");
        assert_eq!(comments[0].line, "12");
        assert!(comments[0].resolved);
        assert_eq!(comments[1].author, "maintainer");
        assert_eq!(comments[1].id, "PRRC_kw456");
        assert_eq!(comments[1].path, "src/lib.rs");
        assert_eq!(comments[1].line, "20");
        assert!(!comments[1].resolved);
    }

    #[test]
    fn fetch_pr_summary_uses_merged_at_instead_of_removed_merged_field() {
        let temp = unique_temp_dir("prism-gh-summary-test");
        let bin = temp.join("bin");
        let repo = temp.join("repo");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&repo).unwrap();
        let gh = bin.join("gh");
        fs::write(
            &gh,
            r#"#!/bin/sh
for arg in "$@"; do
  case "$arg" in
    merged|merged,*|*,merged|*,merged,*)
      echo 'Unknown JSON field: "merged"' >&2
      exit 1
      ;;
  esac
done
cat <<'JSON'
{
  "number": 7,
  "title": "Test PR",
  "url": "https://github.com/example/repo/pull/7",
  "state": "CLOSED",
  "reviewDecision": null,
  "headRefName": "feature",
  "baseRefName": "main",
  "headRefOid": "abc123",
  "updatedAt": "2026-01-01T00:00:00Z",
  "statusCheckRollup": [],
  "mergedAt": "2026-01-02T00:00:00Z",
  "isDraft": false
}
JSON
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&gh).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&gh, permissions).unwrap();

        let mut config = test_config();
        config
            .tools
            .insert("gh".to_string(), gh.display().to_string());

        let summary = fetch_pr_summary(&repo, "feature", &config)
            .unwrap()
            .unwrap()
            .0;

        assert_eq!(summary.number, 7);
        assert_eq!(summary.review_decision, "UNKNOWN");
        assert!(summary.merged);

        let _ = fs::remove_dir_all(temp);
    }

    fn test_config() -> Config {
        Config {
            default_agent: "ask".to_string(),
            default_base: None,
            plan_dir: "plans".to_string(),
            review_packet_dir: ".agent/review".to_string(),
            worktree_command: "wt".to_string(),
            opencode_port_base: 41_000,
            opencode_port_span: 1_000,
            opencode_shutdown_owned_servers: false,
            opencode_plan_plugin: false,
            escape_key: EscapeKey::EscEsc,
            merge_method: crate::config::MergeMethod::Squash,
            icon_style: crate::config::IconStyle::Unicode,
            icon_style_configured: false,
            auto: crate::config::AutoConfig::default(),
            layout: crate::config::LayoutConfig::default(),
            checks: Checks::default(),
            worktree_columns: Vec::new(),
            tools: BTreeMap::new(),
            agent_commands: BTreeMap::new(),
            agent_prompt_modes: BTreeMap::new(),
            prompt_templates: BTreeMap::new(),
            user_path: PathBuf::from("/tmp/prism-user-config.toml"),
            repo_config_path: PathBuf::from("/tmp/prism-repo-config.toml"),
        }
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }

    fn test_summary(head_ref: &str, head_sha: &str, comment_count: u64) -> PrSummary {
        PrSummary {
            number: 42,
            title: "Fix review".to_string(),
            body: "Body".to_string(),
            url: "https://github.com/example/repo/pull/42".to_string(),
            state: "OPEN".to_string(),
            review_decision: "CHANGES_REQUESTED".to_string(),
            requested_reviewers: vec!["alice".to_string()],
            head_ref: head_ref.to_string(),
            base_ref: "main".to_string(),
            head_sha: head_sha.to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            check_status: "failed".to_string(),
            merge_state_status: "CLEAN".to_string(),
            comment_count,
            merged: false,
            draft: false,
        }
    }

    fn test_session(branch: &str, pr: PrCache) -> Session {
        Session {
            repo_index: 0,
            repo_label: "repo".to_string(),
            repo_key: None,
            path: PathBuf::from("/tmp").join(branch),
            path_display: format!("/tmp/{branch}"),
            branch: branch.to_string(),
            prompt_summary: String::new(),
            classification: crate::session::SessionClassification::Work,
            adopted: false,
            hidden: false,
            status_label: String::new(),
            agent_state: crate::agent::AgentState::Idle,
            opencode_status: None,
            pr,
            wt_columns: BTreeMap::new(),
            unseen_comments: false,
        }
    }
}
