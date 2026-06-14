use std::process::{Command, Stdio};
use std::time::Instant;

use rusqlite::params;

use crate::config::Config;
use crate::json::{
    collect_json_string_fields, json_bool_field, json_login_field, json_object_field,
    json_objects_in_array, json_string_field, json_u64_field,
};
use crate::observability;
use crate::process::run_capture;
use crate::repo::Repository;
use crate::util::timestamp_label;

pub const PR_SUMMARY_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);
pub const PR_DETAIL_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrSummary {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub url: String,
    pub state: String,
    pub review_decision: String,
    pub head_ref: String,
    pub base_ref: String,
    pub head_sha: String,
    pub updated_at: String,
    pub check_status: String,
    pub comment_count: u64,
    pub merged: bool,
    pub draft: bool,
}

impl PrSummary {
    pub fn signature(&self) -> String {
        format!(
            "{}:{}:{}:{}:{}:{}:{}:{}",
            self.number,
            self.state,
            self.review_decision,
            self.body,
            self.head_sha,
            self.updated_at,
            self.check_status,
            self.comment_count
        )
    }
}

#[derive(Clone, Debug, Default)]
pub struct PrDetails {
    pub comments: Vec<PrComment>,
    pub reviews: Vec<PrReview>,
    pub review_comments: Vec<PrReviewComment>,
    pub files: Vec<String>,
    pub failing_checks: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct PrComment {
    pub author: String,
    pub body: String,
    pub created_at: String,
}

#[derive(Clone, Debug, Default)]
pub struct PrReview {
    pub author: String,
    pub state: String,
    pub body: String,
    pub submitted_at: String,
}

#[derive(Clone, Debug, Default)]
pub struct PrReviewComment {
    pub author: String,
    pub path: String,
    pub line: String,
    pub body: String,
    pub created_at: String,
    pub resolved: bool,
}

pub fn load_pr_cache(repo: &Repository, branch: &str) -> PrCache {
    let Ok((summary, last_refreshed)) = observability::with_writable_db(repo, |conn| {
        conn.query_row(
            "select
                number, title, body, url, state, review_decision, head_ref, base_ref, head_sha,
                updated_at, check_status, comment_count, merged, draft, last_refreshed
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
                        head_ref: row.get(6)?,
                        base_ref: row.get(7)?,
                        head_sha: row.get(8)?,
                        updated_at: row.get(9)?,
                        check_status: row.get(10)?,
                        comment_count: row_u64(row, 11)?,
                        merged: row.get(12)?,
                        draft: row.get(13)?,
                    },
                    row.get::<_, String>(14)?,
                ))
            },
        )
        .map_err(|error| format!("read PR cache: {error}"))
    }) else {
        return PrCache::default();
    };
    let signature = Some(summary.signature());
    PrCache {
        summary: Some(summary),
        details: None,
        last_refreshed: Some(last_refreshed),
        signature,
        ..PrCache::default()
    }
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
    if config.is_default_branch(branch) {
        cache.summary = None;
        cache.details = None;
        cache.signature = None;
        cache.error = None;
        cache.last_refreshed = Some(timestamp_label());
        let _ = remove_pr_cache(repo, branch);
        return;
    }
    let result = fetch_pr_summary(path, branch, config);
    match result {
        Ok(Some((summary, _raw))) => {
            let signature = summary.signature();
            cache.summary = Some(summary);
            cache.error = None;
            cache.last_refreshed = Some(timestamp_label());
            if force_details && pr_details_due(cache) {
                refresh_pr_details_cache(branch, cache, path, config);
            }
            cache.signature = Some(signature);
            let _ = save_pr_cache(repo, branch, cache);
        }
        Ok(None) => {
            cache.summary = None;
            cache.details = None;
            cache.signature = None;
            cache.error = None;
            cache.last_refreshed = Some(timestamp_label());
            let _ = remove_pr_cache(repo, branch);
        }
        Err(error) => {
            cache.error = Some(error);
        }
    }
}

pub fn refresh_pr_summary_index(
    repo: &Repository,
    sessions: &mut [crate::session::Session],
    summaries: Vec<PrSummary>,
    config: &Config,
) {
    let now = Instant::now();
    let refreshed = timestamp_label();
    for session in sessions {
        session.pr.last_polled = Some(now);
        if session.branch == "(detached)" || config.is_default_branch(&session.branch) {
            session.pr.summary = None;
            session.pr.details = None;
            session.pr.signature = None;
            session.pr.error = None;
            session.pr.last_refreshed = Some(refreshed.clone());
            let _ = remove_pr_cache(repo, &session.branch);
            continue;
        }
        let summary = summaries
            .iter()
            .find(|summary| summary.head_ref == session.branch)
            .cloned();
        if let Some(summary) = summary {
            let signature = summary.signature();
            if session.pr.signature.as_deref() != Some(signature.as_str()) {
                session.pr.details = None;
                session.pr.details_last_polled = None;
            }
            session.pr.summary = Some(summary);
            session.pr.signature = Some(signature);
            session.pr.error = None;
            session.pr.last_refreshed = Some(refreshed.clone());
            let _ = save_pr_cache(repo, &session.branch, &session.pr);
        } else {
            session.pr.summary = None;
            session.pr.details = None;
            session.pr.signature = None;
            session.pr.error = None;
            session.pr.last_refreshed = Some(refreshed.clone());
            let _ = remove_pr_cache(repo, &session.branch);
        }
    }
}

pub fn refresh_pr_details_cache(
    branch: &str,
    cache: &mut PrCache,
    path: &std::path::Path,
    config: &Config,
) {
    cache.details_last_polled = Some(Instant::now());
    if config.is_default_branch(branch) {
        cache.details = None;
        cache.error = None;
        return;
    }
    let Some(summary) = &cache.summary else {
        cache.details = None;
        return;
    };
    match fetch_pr_details(path, branch, summary.number, config) {
        Ok(details) => {
            cache.details = Some(details);
            cache.error = None;
        }
        Err(error) => cache.error = Some(error),
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
        headRefName
        baseRefName
        headRefOid
        updatedAt
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
    let remote = run_capture(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(path)
            .args(["remote", "get-url", "origin"]),
    )?;
    parse_github_remote(remote.trim()).ok_or_else(|| {
        format!(
            "origin remote is not a GitHub repository: {}",
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
    json_objects_in_array(raw, "nodes")
        .into_iter()
        .filter_map(parse_pr_summary_node)
        .collect()
}

fn parse_pr_summary_node(raw: &str) -> Option<PrSummary> {
    let number = json_u64_field(raw, "number")?;
    let comments = json_object_field(raw, "comments")
        .and_then(|object| json_u64_field(object, "totalCount"))
        .unwrap_or(0);
    let review_threads = json_object_field(raw, "reviewThreads")
        .and_then(|object| json_u64_field(object, "totalCount"))
        .unwrap_or(0);
    Some(PrSummary {
        number,
        title: json_string_field(raw, "title").unwrap_or_default(),
        body: json_string_field(raw, "body").unwrap_or_default(),
        url: json_string_field(raw, "url").unwrap_or_default(),
        state: json_string_field(raw, "state").unwrap_or_default(),
        review_decision: json_string_field(raw, "reviewDecision")
            .unwrap_or_else(|| "UNKNOWN".to_string()),
        head_ref: json_string_field(raw, "headRefName").unwrap_or_default(),
        base_ref: json_string_field(raw, "baseRefName").unwrap_or_default(),
        head_sha: json_string_field(raw, "headRefOid").unwrap_or_default(),
        updated_at: json_string_field(raw, "updatedAt").unwrap_or_default(),
        check_status: parse_check_status(raw),
        comment_count: comments + review_threads,
        merged: parse_merged_status(raw),
        draft: json_bool_field(raw, "isDraft").unwrap_or(false),
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
        "headRefName",
        "baseRefName",
        "headRefOid",
        "updatedAt",
        "statusCheckRollup",
        "mergedAt",
        "isDraft",
    ]
    .join(",");
    let output = Command::new(config.tool("gh"))
        .arg("pr")
        .arg("view")
        .arg(branch)
        .arg("--json")
        .arg(fields)
        .current_dir(path)
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("gh pr view: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
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
    let raw = String::from_utf8_lossy(&output.stdout).to_string();
    let Some(number) = json_u64_field(&raw, "number") else {
        return Ok(None);
    };
    let summary = PrSummary {
        number,
        title: json_string_field(&raw, "title").unwrap_or_default(),
        body: json_string_field(&raw, "body").unwrap_or_default(),
        url: json_string_field(&raw, "url").unwrap_or_default(),
        state: json_string_field(&raw, "state").unwrap_or_default(),
        review_decision: json_string_field(&raw, "reviewDecision")
            .unwrap_or_else(|| "UNKNOWN".to_string()),
        head_ref: json_string_field(&raw, "headRefName").unwrap_or_default(),
        base_ref: json_string_field(&raw, "baseRefName").unwrap_or_default(),
        head_sha: json_string_field(&raw, "headRefOid").unwrap_or_default(),
        updated_at: json_string_field(&raw, "updatedAt").unwrap_or_default(),
        check_status: parse_check_status(&raw),
        comment_count: 0,
        merged: parse_merged_status(&raw),
        draft: json_bool_field(&raw, "isDraft").unwrap_or(false),
    };
    Ok(Some((summary, raw)))
}

fn fetch_pr_details(
    path: &std::path::Path,
    branch: &str,
    pr_number: u64,
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
    Ok(details)
}

pub fn parse_pr_details(raw: &str) -> PrDetails {
    PrDetails {
        comments: parse_pr_comments(raw),
        reviews: parse_pr_reviews(raw),
        review_comments: Vec::new(),
        files: collect_json_string_fields(raw, "path", 8),
        failing_checks: collect_failing_checks(raw),
    }
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

fn parse_pr_comments(raw: &str) -> Vec<PrComment> {
    json_objects_in_array(raw, "comments")
        .into_iter()
        .map(|object| PrComment {
            author: json_login_field(object).unwrap_or_default(),
            body: json_string_field(object, "body").unwrap_or_default(),
            created_at: json_string_field(object, "createdAt")
                .or_else(|| json_string_field(object, "created_at"))
                .unwrap_or_default(),
        })
        .filter(|comment| !comment.body.trim().is_empty())
        .take(20)
        .collect()
}

fn parse_pr_reviews(raw: &str) -> Vec<PrReview> {
    json_objects_in_array(raw, "reviews")
        .into_iter()
        .map(|object| PrReview {
            author: json_login_field(object).unwrap_or_default(),
            state: json_string_field(object, "state").unwrap_or_default(),
            body: json_string_field(object, "body").unwrap_or_default(),
            submitted_at: json_string_field(object, "submittedAt")
                .or_else(|| json_string_field(object, "submitted_at"))
                .unwrap_or_default(),
        })
        .filter(|review| !review.state.trim().is_empty() || !review.body.trim().is_empty())
        .take(20)
        .collect()
}

#[cfg(test)]
fn parse_inline_review_comments(raw: &str) -> Vec<PrReviewComment> {
    crate::json::json_top_level_objects(raw)
        .into_iter()
        .map(|object| PrReviewComment {
            author: json_login_field(object).unwrap_or_default(),
            path: json_string_field(object, "path").unwrap_or_default(),
            line: json_u64_field(object, "line")
                .or_else(|| json_u64_field(object, "original_line"))
                .map(|line| line.to_string())
                .unwrap_or_default(),
            body: json_string_field(object, "body").unwrap_or_default(),
            created_at: json_string_field(object, "created_at")
                .or_else(|| json_string_field(object, "createdAt"))
                .unwrap_or_default(),
            resolved: false,
        })
        .filter(|comment| !comment.body.trim().is_empty())
        .take(100)
        .collect()
}

pub fn parse_review_thread_comments(raw: &str) -> Vec<PrReviewComment> {
    let Some(review_threads) = json_object_field(raw, "reviewThreads") else {
        return Vec::new();
    };
    let mut comments = Vec::new();
    for thread in json_objects_in_array(review_threads, "nodes") {
        let resolved = json_bool_field(thread, "isResolved").unwrap_or(false);
        let Some(thread_comments) = json_object_field(thread, "comments") else {
            continue;
        };
        for object in json_objects_in_array(thread_comments, "nodes") {
            if comments.len() >= 100 {
                return comments;
            }
            let comment = PrReviewComment {
                author: json_login_field(object).unwrap_or_default(),
                path: json_string_field(object, "path").unwrap_or_default(),
                line: json_u64_field(object, "line")
                    .or_else(|| json_u64_field(object, "originalLine"))
                    .map(|line| line.to_string())
                    .unwrap_or_default(),
                body: json_string_field(object, "body").unwrap_or_default(),
                created_at: json_string_field(object, "createdAt")
                    .or_else(|| json_string_field(object, "created_at"))
                    .unwrap_or_default(),
                resolved,
            };
            if !comment.body.trim().is_empty() {
                comments.push(comment);
            }
        }
    }
    comments
}

pub fn parse_check_status(raw: &str) -> String {
    let statuses = collect_json_string_fields(raw, "status", 64);
    let conclusions = collect_json_string_fields(raw, "conclusion", 64);
    let states = collect_json_string_fields(raw, "state", 64)
        .into_iter()
        .filter(|value| !matches!(value.as_str(), "OPEN" | "CLOSED" | "MERGED"))
        .collect::<Vec<_>>();
    if conclusions.iter().any(|value| {
        matches!(
            value.as_str(),
            "FAILURE" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED"
        )
    }) || states
        .iter()
        .any(|value| matches!(value.as_str(), "ERROR" | "FAILURE"))
    {
        return "failed".to_string();
    }
    if statuses.iter().any(|value| {
        matches!(
            value.as_str(),
            "QUEUED" | "IN_PROGRESS" | "PENDING" | "REQUESTED"
        )
    }) || states.iter().any(|value| value == "PENDING")
    {
        return "running".to_string();
    }
    let conclusions_pass = conclusions
        .iter()
        .all(|value| matches!(value.as_str(), "SUCCESS" | "SKIPPED" | "NEUTRAL"));
    let states_pass = states.iter().all(|value| value == "SUCCESS");
    if (!conclusions.is_empty() || !states.is_empty()) && conclusions_pass && states_pass {
        return "passed".to_string();
    }
    if statuses.is_empty() && conclusions.is_empty() && states.is_empty() {
        "unknown".to_string()
    } else {
        "mixed".to_string()
    }
}

fn collect_failing_checks(raw: &str) -> Vec<String> {
    let names = collect_json_string_fields(raw, "name", 64);
    let conclusions = collect_json_string_fields(raw, "conclusion", 64);
    names
        .into_iter()
        .zip(conclusions)
        .filter_map(|(name, conclusion)| {
            matches!(
                conclusion.as_str(),
                "FAILURE" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED"
            )
            .then_some(name)
        })
        .take(8)
        .collect()
}

fn parse_merged_status(raw: &str) -> bool {
    json_bool_field(raw, "merged").unwrap_or_else(|| {
        json_string_field(raw, "mergedAt")
            .map(|value| !value.trim().is_empty())
            .unwrap_or_else(|| {
                json_string_field(raw, "state")
                    .map(|state| state == "MERGED")
                    .unwrap_or(false)
            })
    })
}

pub fn remove_pr_cache(repo: &Repository, branch: &str) -> Result<(), String> {
    observability::with_writable_db(repo, |conn| {
        conn.execute("delete from pr_cache where branch = ?1", params![branch])
            .map_err(|error| format!("remove PR cache: {error}"))?;
        Ok(())
    })
}

fn save_pr_cache(repo: &Repository, branch: &str, cache: &PrCache) -> Result<(), String> {
    let Some(summary) = &cache.summary else {
        return Ok(());
    };
    let number = sqlite_i64(summary.number, "PR number")?;
    let comment_count = sqlite_i64(summary.comment_count, "PR comment count")?;
    observability::with_writable_db(repo, |conn| {
        conn.execute(
            "insert into pr_cache (
                branch, number, title, body, url, state, review_decision, head_ref, base_ref,
                head_sha, updated_at, check_status, comment_count, merged, draft, last_refreshed,
                refreshed_unix_ms
             ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
             on conflict(branch) do update set
                number = excluded.number,
                title = excluded.title,
                body = excluded.body,
                url = excluded.url,
                state = excluded.state,
                review_decision = excluded.review_decision,
                head_ref = excluded.head_ref,
                base_ref = excluded.base_ref,
                head_sha = excluded.head_sha,
                updated_at = excluded.updated_at,
                check_status = excluded.check_status,
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
                summary.head_ref.as_str(),
                summary.base_ref.as_str(),
                summary.head_sha.as_str(),
                summary.updated_at.as_str(),
                summary.check_status.as_str(),
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
    fn pr_json_helpers_parse_summary_fields() {
        let raw = r#"{
            "number": 42,
            "title": "Fix review",
            "mergedAt": "2026-01-01T00:00:00Z",
            "isDraft": true,
            "comments": [{"body": "hello"}],
            "reviews": [{"state": "CHANGES_REQUESTED"}],
            "files": [{"path": "src/main.rs"}],
            "statusCheckRollup": [{"name": "test", "status": "COMPLETED", "conclusion": "FAILURE"}]
        }"#;
        assert_eq!(json_u64_field(raw, "number"), Some(42));
        assert_eq!(json_bool_field(raw, "isDraft"), Some(true));
        assert!(parse_merged_status(raw));
        assert_eq!(parse_check_status(raw), "failed");
        let details = parse_pr_details(raw);
        assert_eq!(details.files, vec!["src/main.rs"]);
        assert_eq!(details.failing_checks, vec!["test"]);
        assert_eq!(details.comments[0].body, "hello");
        assert_eq!(details.reviews[0].state, "CHANGES_REQUESTED");
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
                    "reviewDecision": "REVIEW_REQUIRED",
                    "headRefName": "feature",
                    "baseRefName": "main",
                    "headRefOid": "abc123",
                    "updatedAt": "2026-01-01T00:00:00Z",
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
        assert_eq!(summaries[0].comment_count, 5);
        assert_eq!(summaries[0].check_status, "passed");
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
                "body": "please simplify",
                "created_at": "2026-01-01T00:00:00Z",
                "user": {"login": "reviewer"}
            }
        ]"#;
        let comments = parse_inline_review_comments(raw);
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].path, "src/main.rs");
        assert_eq!(comments[0].line, "12");
        assert_eq!(comments[0].author, "reviewer");
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
        assert_eq!(comments[0].path, "src/main.rs");
        assert_eq!(comments[0].line, "12");
        assert!(comments[0].resolved);
        assert_eq!(comments[1].author, "maintainer");
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
  "reviewDecision": "",
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
            escape_key: EscapeKey::EscEsc,
            checks: Checks::default(),
            tools: BTreeMap::new(),
            agent_commands: BTreeMap::new(),
            agent_prompt_modes: BTreeMap::new(),
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
}
