use std::collections::BTreeSet;
use std::process::Command;
use std::time::{Duration, Instant};

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::config::MergeMethod;
use crate::git::current_head_sha;
use crate::observability;
use crate::process::{run_capture, run_output_allow_failure};
use crate::repo::Repository;
use crate::session::Session;
use crate::util::{strip_ansi, timestamp_label};

pub const PR_SUMMARY_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);
pub const PR_DETAIL_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
const PR_MERGE_VERIFY_ATTEMPTS: usize = 6;
const PR_MERGE_VERIFY_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum PrObservationQuality {
    #[default]
    Unknown,
    Fresh,
    AuthoritativeAbsence,
    PreservedStale,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PrDetailsAssociation {
    pr_number: u64,
    head_sha: String,
}

struct PersistedPrDetails {
    details: PrDetails,
    association: Option<PrDetailsAssociation>,
    errors: Vec<String>,
}

impl PrDetailsAssociation {
    fn from_summary(summary: &PrSummary) -> Self {
        Self {
            pr_number: summary.number,
            head_sha: summary.head_sha.clone(),
        }
    }

    fn matches(&self, summary: &PrSummary) -> bool {
        self.pr_number == summary.number && self.head_sha == summary.head_sha
    }
}

#[derive(Clone, Debug, Default)]
pub struct PrCache {
    summary: Option<PrSummary>,
    details: Option<PrDetails>,
    last_polled: Option<Instant>,
    details_last_polled: Option<Instant>,
    last_refreshed: Option<String>,
    signature: Option<String>,
    error: Option<String>,
    summary_quality: PrObservationQuality,
    details_quality: PrObservationQuality,
    details_association: Option<PrDetailsAssociation>,
    summary_error: Option<String>,
    details_errors: Vec<String>,
    persistence_error: Option<String>,
    details_persistence_error: Option<String>,
    next_generation: u64,
    pending_summary: Option<(u64, Instant)>,
    pending_details: Option<u64>,
    summary_observed_in_process: bool,
}

impl PrCache {
    #[cfg(test)]
    pub(crate) fn observed(summary: PrSummary, details: Option<PrDetails>) -> Self {
        let association = PrDetailsAssociation::from_summary(&summary);
        Self {
            signature: Some(summary.signature()),
            summary: Some(summary),
            details_quality: if details.is_some() {
                PrObservationQuality::Fresh
            } else {
                PrObservationQuality::Unknown
            },
            details,
            summary_quality: PrObservationQuality::Fresh,
            summary_observed_in_process: true,
            details_association: Some(association),
            ..Self::default()
        }
    }

    #[cfg(test)]
    pub(crate) fn mark_preserved_stale(&mut self) {
        self.summary_quality = PrObservationQuality::PreservedStale;
        if self.details.is_some() {
            self.details_quality = PrObservationQuality::PreservedStale;
        }
    }

    #[cfg(test)]
    pub(crate) fn stale_for_test(details: Option<PrDetails>, error: &str) -> Self {
        Self {
            details,
            error: Some(error.to_string()),
            summary_error: Some(error.to_string()),
            summary_quality: PrObservationQuality::PreservedStale,
            details_quality: PrObservationQuality::PreservedStale,
            ..Self::default()
        }
    }

    fn summary_identity(&self) -> Option<PrDetailsAssociation> {
        self.summary
            .as_ref()
            .map(PrDetailsAssociation::from_summary)
    }

    fn next_generation(&mut self) -> u64 {
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        self.next_generation
    }

    pub(crate) fn begin_summary_poll(&mut self, started_at: Instant) {
        let generation = self.next_generation();
        self.pending_summary = Some((generation, started_at));
        self.last_polled = Some(started_at);
    }

    fn accepts_summary_poll(&self, started_at: Instant) -> bool {
        self.pending_summary
            .is_some_and(|(_, pending_at)| pending_at == started_at)
    }

    fn finish_summary_poll(&mut self, started_at: Instant) -> bool {
        if !self.accepts_summary_poll(started_at) {
            return false;
        }
        self.pending_summary = None;
        true
    }

    pub(crate) fn begin_details_poll(&mut self) -> Self {
        let generation = self.next_generation();
        self.pending_details = Some(generation);
        self.details_last_polled = Some(Instant::now());
        self.clone()
    }

    fn accepts_details_poll(&self, result: &Self) -> bool {
        self.pending_details.is_some() && self.pending_details == result.pending_details
    }

    fn details_are_associated(&self) -> bool {
        self.summary.as_ref().is_some_and(|summary| {
            self.details_association
                .as_ref()
                .is_some_and(|association| association.matches(summary))
        })
    }

    fn rebuild_error(&mut self) {
        self.error = self
            .summary_error
            .iter()
            .chain(self.details_errors.iter())
            .chain(self.persistence_error.iter())
            .chain(self.details_persistence_error.iter())
            .next()
            .cloned();
    }

    fn record_persistence_result(&mut self, result: Result<(), String>) {
        match result {
            Ok(()) => self.persistence_error = None,
            Err(error) => self.persistence_error = Some(error),
        }
        self.rebuild_error();
    }

    fn refresh_result(&self) -> Result<(), String> {
        self.summary_error
            .as_ref()
            .or_else(|| self.details_errors.first())
            .or(self.persistence_error.as_ref())
            .or(self.details_persistence_error.as_ref())
            .map_or(Ok(()), |error| Err(error.clone()))
    }

    fn record_summary_failure(&mut self, error: String) {
        self.summary_error = Some(error);
        self.summary_quality = if self.summary.is_some() {
            PrObservationQuality::PreservedStale
        } else {
            PrObservationQuality::Failed
        };
        self.rebuild_error();
    }

    fn record_summary_observation(
        &mut self,
        summary: Option<PrSummary>,
        refreshed: String,
    ) -> PrCacheSummaryMutation {
        match summary {
            Some(summary) => {
                let signature = summary.signature();
                let association = PrDetailsAssociation::from_summary(&summary);
                if self.summary_identity().as_ref() != Some(&association) {
                    self.details = None;
                    self.details_last_polled = None;
                    self.details_association = None;
                    self.details_quality = PrObservationQuality::Unknown;
                    self.details_errors.clear();
                }
                self.summary = Some(summary);
                self.summary_observed_in_process = true;
                self.signature = Some(signature);
                self.summary_quality = PrObservationQuality::Fresh;
                self.summary_error = None;
                self.last_refreshed = Some(refreshed);
                self.rebuild_error();
                PrCacheSummaryMutation::SaveSummary
            }
            None => {
                self.summary = None;
                self.summary_observed_in_process = true;
                self.details = None;
                self.details_last_polled = None;
                self.signature = None;
                self.summary_quality = PrObservationQuality::AuthoritativeAbsence;
                self.details_quality = PrObservationQuality::AuthoritativeAbsence;
                self.details_association = None;
                self.summary_error = None;
                self.details_errors.clear();
                self.last_refreshed = Some(refreshed);
                self.rebuild_error();
                PrCacheSummaryMutation::RemoveSummary
            }
        }
    }

    fn record_details_observation(&mut self, observation: PrDetailsObservation) -> bool {
        if self.summary_identity().as_ref() != Some(&observation.association) {
            return false;
        }

        let mut details = self.details.take().unwrap_or_default();
        let mut errors = Vec::new();
        macro_rules! record_component {
            ($field:ident, $label:literal) => {
                match observation.$field {
                    Ok(value) => details.$field = value,
                    Err(error) => errors.push(format!("{}: {error}", $label)),
                }
            };
        }
        record_component!(comments, "comments");
        record_component!(reviews, "reviews");
        record_component!(review_comments, "review threads");
        record_component!(files, "files");
        record_component!(failing_checks, "checks");
        record_component!(check_contexts, "check contexts");
        record_component!(ci_failures, "CI logs");

        self.details = Some(details);
        self.details_association = Some(observation.association);
        self.details_quality = if errors.is_empty() {
            PrObservationQuality::Fresh
        } else {
            PrObservationQuality::PreservedStale
        };
        self.details_errors = errors;
        self.rebuild_error();
        true
    }

    pub(crate) fn summary_observation_quality(&self) -> PrObservationQuality {
        self.summary_quality
    }

    pub fn summary(&self) -> Option<&PrSummary> {
        self.summary.as_ref()
    }

    pub fn details(&self) -> Option<&PrDetails> {
        self.details.as_ref()
    }

    pub fn last_refreshed(&self) -> Option<&str> {
        self.last_refreshed.as_deref()
    }

    pub fn display_error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub(crate) fn has_summary(&self) -> bool {
        self.summary.is_some()
    }

    pub(crate) fn is_for_pr(&self, number: u64) -> bool {
        self.summary
            .as_ref()
            .is_some_and(|summary| summary.number == number)
    }

    fn pollable(&self, eligibility: PrCacheEligibility) -> bool {
        eligibility.can_observe() && !self.summary.as_ref().is_some_and(|summary| summary.merged)
    }

    pub(crate) fn details_observation_quality(&self) -> PrObservationQuality {
        self.details_quality
    }

    pub(crate) fn trusted_summary(&self) -> Result<Option<&PrSummary>, String> {
        if let Some(error) = self
            .summary_error
            .as_ref()
            .or(self.persistence_error.as_ref())
            .or(self.details_persistence_error.as_ref())
            .or_else(|| {
                (self.summary_quality == PrObservationQuality::Unknown)
                    .then_some(self.error.as_ref())
                    .flatten()
            })
        {
            return Err(error.clone());
        }
        if self.summary.is_some() && self.summary_quality != PrObservationQuality::Fresh {
            return Err("pull request summary has not been freshly observed".to_string());
        }
        Ok(self.summary.as_ref())
    }

    pub(crate) fn trusted_details(&self) -> Result<Option<&PrDetails>, String> {
        if let Some(error) = self
            .summary_error
            .as_ref()
            .or(self.persistence_error.as_ref())
            .or(self.details_persistence_error.as_ref())
            .or_else(|| self.details_errors.first())
        {
            return Err(error.clone());
        }
        if self.details_quality != PrObservationQuality::Fresh || !self.details_are_associated() {
            if self.details.is_none() && self.details_quality == PrObservationQuality::Unknown {
                return Ok(None);
            }
            return Err("pull request details have not been freshly observed".to_string());
        }
        Ok(self.details.as_ref())
    }

    pub(crate) fn trusted_summary_and_details(
        &self,
    ) -> Result<Option<(&PrSummary, Option<&PrDetails>)>, String> {
        let Some(summary) = self.trusted_summary()? else {
            return Ok(None);
        };
        Ok(Some((summary, self.trusted_details()?)))
    }

    pub(crate) fn reconcile_session_refresh(
        &mut self,
        previous: Self,
        branch: &str,
        config: &Config,
        hidden: bool,
    ) {
        if !Self::structurally_eligible(branch, config, hidden) {
            *self = Self::default();
            return;
        }
        if self.summary_observed_in_process
            && self.summary_quality == PrObservationQuality::AuthoritativeAbsence
        {
            return;
        }
        let loaded_identity = self.summary_identity();
        let previous_identity = previous.summary_identity();
        if loaded_identity.is_none() || loaded_identity == previous_identity {
            *self = previous;
        }
    }

    pub(crate) fn eligible_for_worktree(
        branch: &str,
        path: &std::path::Path,
        config: &Config,
        hidden: bool,
    ) -> bool {
        !hidden && PrCacheEligibility::for_worktree(branch, path, config).can_observe()
    }

    pub(crate) fn structurally_eligible(branch: &str, config: &Config, hidden: bool) -> bool {
        !hidden && branch != "(detached)" && !config.is_default_branch(branch)
    }

    pub(crate) fn enforce_eligibility(
        &mut self,
        repo: &Repository,
        branch: &str,
        path: &std::path::Path,
        config: &Config,
        hidden: bool,
    ) -> bool {
        if Self::eligible_for_worktree(branch, path, config, hidden) {
            return false;
        }
        let changed = self.summary.is_some()
            || self.details.is_some()
            || self.error.is_some()
            || self.last_refreshed.is_some();
        if changed {
            clear_pr_cache(repo, branch, self);
        }
        changed
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RepoPolicyCache {
    pub repo_remote: String,
    pub default_branch: Option<String>,
    pub required_approvals: u64,
    pub require_conversation_resolution: bool,
    pub require_branch_up_to_date: bool,
    pub required_checks: Vec<String>,
    pub merge_queue_required: bool,
    pub refreshed_unix_ms: u64,
    pub error: Option<String>,
}

pub(crate) struct PrCacheRepository<'a> {
    pub repo: &'a Repository,
    pub config: &'a Config,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PrCacheEligibility {
    is_default_branch: bool,
    is_detached: bool,
    has_github_remote: bool,
}

impl PrCacheEligibility {
    fn for_worktree(branch: &str, path: &std::path::Path, config: &Config) -> Self {
        Self {
            is_default_branch: config.is_default_branch(branch),
            is_detached: branch == "(detached)",
            has_github_remote: github_remote_configured(path, config),
        }
    }

    fn for_session(session: &Session, config: &Config) -> Self {
        Self::for_worktree(&session.branch, &session.path, config)
    }

    fn for_successful_index(session: &Session, config: &Config) -> Self {
        Self {
            is_default_branch: session.is_default_branch(config),
            is_detached: session.is_detached(),
            has_github_remote: true,
        }
    }

    fn can_observe(self) -> bool {
        !self.is_default_branch && !self.is_detached && self.has_github_remote
    }
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub enum PrCheckState {
    Pending,
    Success,
    Failed,
    Mixed,
    #[default]
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
    pub check_contexts: Vec<PrCheckContext>,
    pub ci_failures: Vec<CiFailure>,
}

#[derive(Debug)]
struct PrDetailsObservation {
    association: PrDetailsAssociation,
    comments: Result<Vec<PrComment>, String>,
    reviews: Result<Vec<PrReview>, String>,
    review_comments: Result<Vec<PrReviewComment>, String>,
    files: Result<Vec<String>, String>,
    failing_checks: Result<Vec<String>, String>,
    check_contexts: Result<Vec<PrCheckContext>, String>,
    ci_failures: Result<Vec<CiFailure>, String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct PrCheckContext {
    pub name: String,
    pub state: PrCheckState,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
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
    pub thread_id: String,
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
    #[serde(default, rename = "defaultBranchRef")]
    default_branch_ref: GithubBranchRef,
    #[serde(default, rename = "branchProtectionRules")]
    branch_protection_rules: GithubBranchProtectionRuleConnection,
}

#[derive(Debug, Default, Deserialize)]
struct GithubBranchRef {
    #[serde(default)]
    name: String,
}

#[derive(Debug, Default, Deserialize)]
struct GithubBranchProtectionRuleConnection {
    #[serde(default)]
    nodes: Vec<GithubBranchProtectionRule>,
}

#[derive(Debug, Default, Deserialize)]
struct GithubBranchProtectionRule {
    #[serde(default)]
    pattern: String,
    #[serde(default, rename = "requiredApprovingReviewCount")]
    required_approving_review_count: u64,
    #[serde(default, rename = "requiresConversationResolution")]
    requires_conversation_resolution: bool,
    #[serde(default, rename = "requiresStrictStatusChecks")]
    requires_strict_status_checks: bool,
    #[serde(default, rename = "requiredStatusCheckContexts")]
    required_status_check_contexts: Vec<String>,
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
    #[serde(default)]
    id: String,
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
    let loaded = observability::with_writable_db(repo, |conn| {
        conn.query_row(
            "select
                number, title, body, url, state, review_decision, requested_reviewers,
                head_ref, base_ref, head_sha, updated_at, check_status, merge_state_status,
                comment_count, merged, draft, last_refreshed, observation_error
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
                    row.get::<_, Option<String>>(17)?,
                ))
            },
        )
        .optional()
        .map_err(|error| format!("read PR cache: {error}"))
    });
    let (summary, last_refreshed, summary_error) = match loaded {
        Ok(Some(loaded)) => loaded,
        Ok(None) => return PrCache::default(),
        Err(error) => {
            let mut cache = PrCache::default();
            cache.record_summary_failure(error);
            return cache;
        }
    };
    let (details, details_association, details_errors) =
        match load_pr_details_cache_record(repo, branch) {
            Ok(Some(record)) => (Some(record.details), record.association, record.errors),
            Ok(None) => (None, None, Vec::new()),
            Err(error) => (None, None, vec![error]),
        };
    let association_matches = details_association
        .as_ref()
        .is_some_and(|association| association.matches(&summary));
    let association_conflicts = details_association.is_some() && !association_matches;
    let details = (!association_conflicts).then_some(details).flatten();
    let details_association = (!association_conflicts)
        .then_some(details_association)
        .flatten();
    let details_quality = if details.is_some() {
        PrObservationQuality::PreservedStale
    } else {
        PrObservationQuality::Unknown
    };
    let signature = Some(summary.signature());
    let mut cache = PrCache {
        summary: Some(summary),
        details,
        last_refreshed: Some(last_refreshed),
        signature,
        // Persistence is a display cache, not evidence of a successful observation in this
        // process. A refresh must re-authorize workflow decisions after every restart.
        summary_quality: PrObservationQuality::PreservedStale,
        details_quality,
        details_association,
        summary_error,
        details_errors,
        ..PrCache::default()
    };
    cache.rebuild_error();
    cache
}

pub(crate) fn load_pr_cache_for_branch(
    repo: &Repository,
    config: &Config,
    branch: &str,
    path: &std::path::Path,
) -> PrCache {
    if !PrCacheEligibility::for_worktree(branch, path, config).can_observe() {
        return remove_invalid_pr_cache(repo, branch);
    }
    let cache = load_pr_cache(repo, branch);
    if cache
        .summary
        .as_ref()
        .is_some_and(|summary| summary.head_ref != branch)
    {
        return remove_invalid_pr_cache(repo, branch);
    }
    cache
}

fn remove_invalid_pr_cache(repo: &Repository, branch: &str) -> PrCache {
    let mut cache = PrCache::default();
    cache.record_summary_observation(None, timestamp_label());
    cache.record_persistence_result(remove_pr_cache(repo, branch));
    cache
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
    if !PrCacheEligibility::for_worktree(branch, path, config).can_observe() {
        cache.finish_summary_poll(started_at);
        let mutation = cache.record_summary_observation(None, timestamp_label());
        persist_pr_summary_mutation(repo, branch, cache, mutation);
        return cache.refresh_result();
    }
    let result = fetch_pr_summary(path, branch, config);
    match result {
        Ok(Some((summary, _raw))) => {
            if !cache.finish_summary_poll(started_at) {
                return Err("pull request summary refresh was superseded".to_string());
            }
            let mutation = cache.record_summary_observation(Some(summary), timestamp_label());
            if force_details || pr_details_due(cache) {
                let details_result = refresh_pr_details_cache(repo, branch, cache, path, config);
                persist_pr_summary_mutation(repo, branch, cache, mutation);
                details_result?;
            } else {
                persist_pr_summary_mutation(repo, branch, cache, mutation);
            }
        }
        Ok(None) => {
            if !cache.finish_summary_poll(started_at) {
                return Err("pull request summary refresh was superseded".to_string());
            }
            let mutation = cache.record_summary_observation(None, timestamp_label());
            persist_pr_summary_mutation(repo, branch, cache, mutation);
        }
        Err(error) => {
            if !cache.finish_summary_poll(started_at) {
                return Err("pull request summary refresh was superseded".to_string());
            }
            cache.record_summary_failure(error);
            persist_observation_errors(repo, branch, cache);
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
    run_capture(
        Command::new(config.tool("gh"))
            .args(create_pr_args(
                config.default_base.as_deref(),
                body,
                target_repo,
            ))
            .current_dir(path),
    )?;
    refresh_pr_cache(repo, branch, cache, path, config, true)
}

pub(crate) fn merge_pull_request(
    config: &Config,
    path: &std::path::Path,
    pr_number: u64,
    expected_head_sha: &str,
) -> Result<(), String> {
    run_capture(
        Command::new(config.tool("gh"))
            .args(merge_pr_args(
                &pr_number.to_string(),
                config.merge_method,
                expected_head_sha,
            ))
            .current_dir(path),
    )?;
    Ok(())
}

fn create_pr_args(
    default_base: Option<&str>,
    body: &str,
    target_repo: Option<&str>,
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
    args
}

fn merge_pr_args(pr_number: &str, method: MergeMethod, expected_head_sha: &str) -> Vec<String> {
    vec![
        "pr".to_string(),
        "merge".to_string(),
        pr_number.to_string(),
        method.gh_flag().to_string(),
        "--match-head-commit".to_string(),
        expected_head_sha.to_string(),
    ]
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
        save_pr_details_cache_for_association(
            repo,
            branch,
            details,
            &association,
            &cache.details_errors,
        )
    } else if !cache.details_errors.is_empty() {
        save_pr_details_cache_for_association(
            repo,
            branch,
            &PrDetails::default(),
            &association,
            &cache.details_errors,
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
    if !PrCacheEligibility::for_worktree(branch, path, config).can_observe() {
        cache.details = None;
        cache.details_association = None;
        cache.details_quality = PrObservationQuality::AuthoritativeAbsence;
        cache.details_errors.clear();
        cache.rebuild_error();
        return;
    }
    let Some(summary) = cache.summary.clone() else {
        cache.details = None;
        cache.details_association = None;
        cache.details_quality = PrObservationQuality::Unknown;
        return;
    };
    match fetch_pr_details(path, branch, summary.number, &summary.head_sha, config) {
        Ok(observation) => {
            cache.record_details_observation(observation);
        }
        Err(error) => {
            cache.details_errors = vec![error];
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

pub(crate) fn record_pr_details_poll_result(
    repo: &Repository,
    branch: &str,
    cache: &mut PrCache,
    poll_result: PrCache,
) -> bool {
    if !cache.accepts_details_poll(&poll_result) {
        return false;
    }
    let current_identity = cache.summary_identity();
    let result_identity = poll_result
        .details_association
        .clone()
        .or_else(|| poll_result.summary_identity());
    if current_identity.is_none() || current_identity != result_identity {
        return false;
    }
    cache.details = poll_result.details;
    cache.details_last_polled = poll_result.details_last_polled;
    cache.details_association = result_identity;
    cache.details_quality = poll_result.details_quality;
    cache.details_errors = poll_result.details_errors;
    let persistence = if let Some(association) = &cache.details_association {
        if let Some(details) = &cache.details {
            save_pr_details_cache_for_association(
                repo,
                branch,
                details,
                association,
                &cache.details_errors,
            )
        } else if !cache.details_errors.is_empty() {
            save_pr_details_cache_for_association(
                repo,
                branch,
                &PrDetails::default(),
                association,
                &cache.details_errors,
            )
        } else {
            Ok(())
        }
    } else {
        Ok(())
    };
    cache.details_persistence_error = persistence.err();
    cache.pending_details = None;
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

#[cfg(test)]
pub(crate) fn refresh_pr_summary_index_for_sessions(
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
    );
}

pub(crate) fn refresh_pr_summary_index_for_target_sessions(
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
        let summary =
            if !PrCacheEligibility::for_successful_index(session, managed.config).can_observe() {
                None
            } else {
                summaries
                    .iter()
                    .find(|summary| {
                        pr_summary_matches_worktree(
                            summary,
                            &session.branch,
                            &session.path,
                            managed.config,
                            session
                                .pr
                                .summary_observed_in_process
                                .then_some(session.pr.summary.as_ref())
                                .flatten(),
                        )
                    })
                    .cloned()
            };
        let mutation = session
            .pr
            .record_summary_observation(summary, refreshed.clone());
        persist_pr_summary_mutation(managed.repo, &session.branch, &mut session.pr, mutation);
    }
}

fn pr_summary_matches_worktree(
    summary: &PrSummary,
    branch: &str,
    path: &std::path::Path,
    config: &Config,
    known_summary: Option<&PrSummary>,
) -> bool {
    if summary.head_ref != branch {
        return false;
    }
    if !summary.merged && summary.state.eq_ignore_ascii_case("open") {
        return current_head_sha(path, config).is_ok_and(|head| head == summary.head_sha)
            || known_summary.is_some_and(|known| known.number == summary.number);
    }
    current_head_sha(path, config).is_ok_and(|head| head == summary.head_sha)
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

pub(crate) fn pr_cache_pollable_for_session(session: &Session, config: &Config) -> bool {
    session
        .pr
        .pollable(PrCacheEligibility::for_session(session, config))
}

pub(crate) fn clear_pr_cache(repo: &Repository, branch: &str, cache: &mut PrCache) {
    let started_at = Instant::now();
    cache.begin_summary_poll(started_at);
    cache.finish_summary_poll(started_at);
    let mutation = cache.record_summary_observation(None, timestamp_label());
    persist_pr_summary_mutation(repo, branch, cache, mutation);
}

pub(crate) fn record_pr_summary_failure(
    repo: &Repository,
    branch: &str,
    cache: &mut PrCache,
    error: String,
    poll_started_at: Instant,
) -> bool {
    if !cache.finish_summary_poll(poll_started_at) {
        return false;
    }
    cache.record_summary_failure(error);
    persist_observation_errors(repo, branch, cache);
    true
}

pub(crate) fn record_pr_summary(
    repo: &Repository,
    branch: &str,
    cache: &mut PrCache,
    summary: PrSummary,
) {
    let started_at = Instant::now();
    cache.begin_summary_poll(started_at);
    cache.finish_summary_poll(started_at);
    let mutation = cache.record_summary_observation(Some(summary), timestamp_label());
    persist_pr_summary_mutation(repo, branch, cache, mutation);
}

pub(crate) fn record_pr_merged(repo: &Repository, branch: &str, cache: &mut PrCache) {
    let Some(mut summary) = cache.summary.clone() else {
        return;
    };
    summary.merged = true;
    summary.state = "MERGED".to_string();
    record_pr_summary(repo, branch, cache, summary);
}

pub(crate) fn pr_details_pollable(session: &Session, config: &Config) -> bool {
    pr_cache_pollable_for_session(session, config) && pr_details_due(&session.pr)
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
    cache.trusted_summary().map(|summary| summary.cloned())
}

pub(crate) fn trusted_pr_for_session<'a>(
    session: &'a Session,
    config: &Config,
) -> Result<Option<(&'a PrSummary, Option<&'a PrDetails>)>, String> {
    if !PrCache::structurally_eligible(&session.branch, config, session.hidden) {
        return Err("selected worktree is not eligible for pull request observation".to_string());
    }
    session.pr.trusted_summary_and_details()
}

pub(crate) fn pr_cache_render_signature(cache: &PrCache) -> String {
    format!(
        "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
        cache.summary,
        cache.details,
        cache.last_refreshed,
        cache.error,
        cache.summary_observation_quality(),
        cache.details_observation_quality()
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

fn persist_pr_summary_mutation(
    repo: &Repository,
    branch: &str,
    cache: &mut PrCache,
    mutation: PrCacheSummaryMutation,
) {
    let result = match mutation {
        PrCacheSummaryMutation::SaveSummary => save_pr_cache(repo, branch, cache).and_then(|()| {
            if let (Some(details), Some(association)) = (&cache.details, &cache.details_association)
            {
                save_pr_details_cache_for_association(
                    repo,
                    branch,
                    details,
                    association,
                    &cache.details_errors,
                )
            } else {
                remove_pr_details_cache(repo, branch)
            }
        }),
        PrCacheSummaryMutation::RemoveSummary => remove_pr_cache(repo, branch),
    };
    cache.record_persistence_result(result);
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
    try_parse_pr_summary_index(&raw)
}

pub(crate) fn refresh_repo_policy_cache(
    repo: &Repository,
    path: &std::path::Path,
    config: &Config,
) -> Result<RepoPolicyCache, String> {
    let remote = github_remote_repo(path, config, "origin")?;
    let policy = match fetch_repo_policy(path, config) {
        Ok(mut policy) => {
            policy.repo_remote = remote.clone();
            policy
        }
        Err(error) => RepoPolicyCache {
            repo_remote: remote.clone(),
            refreshed_unix_ms: unix_seconds().max(0) as u64,
            error: Some(error),
            ..RepoPolicyCache::default()
        },
    };
    save_repo_policy_cache(repo, &policy)?;
    Ok(policy)
}

pub(crate) fn resolve_review_thread(
    path: &std::path::Path,
    config: &Config,
    thread_id: &str,
) -> Result<(), String> {
    let raw = run_capture(
        Command::new(config.tool("gh"))
            .args(resolve_review_thread_args(thread_id))
            .current_dir(path),
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

fn resolve_review_thread_args(thread_id: &str) -> Vec<String> {
    vec![
        "api".to_string(),
        "graphql".to_string(),
        "-F".to_string(),
        format!("thread={thread_id}"),
        "-f".to_string(),
        format!("query={RESOLVE_REVIEW_THREAD_MUTATION}"),
    ]
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

pub(crate) fn load_repo_policy_cache(
    repo: &Repository,
    repo_remote: &str,
) -> Option<RepoPolicyCache> {
    observability::with_writable_db(repo, |conn| {
        conn.query_row(
            "select repo_remote, default_branch, required_approvals,
                    require_conversation_resolution, require_branch_up_to_date,
                    required_checks, merge_queue_required, refreshed_unix_ms, error
               from repo_policy_cache
              where repo_remote = ?1",
            params![repo_remote],
            |row| {
                Ok(RepoPolicyCache {
                    repo_remote: row.get(0)?,
                    default_branch: row.get(1)?,
                    required_approvals: row_u64(row, 2)?,
                    require_conversation_resolution: row.get::<_, i64>(3)? != 0,
                    require_branch_up_to_date: row.get::<_, i64>(4)? != 0,
                    required_checks: decode_string_values(&row.get::<_, String>(5)?),
                    merge_queue_required: row.get::<_, i64>(6)? != 0,
                    refreshed_unix_ms: row_u64(row, 7)?,
                    error: row.get(8)?,
                })
            },
        )
        .optional()
        .map_err(|error| format!("read repo policy cache: {error}"))
    })
    .ok()
    .flatten()
}

fn fetch_repo_policy(path: &std::path::Path, config: &Config) -> Result<RepoPolicyCache, String> {
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
            .arg(format!("query={REPO_POLICY_QUERY}"))
            .current_dir(path),
    )?;
    parse_repo_policy(&format!("{owner}/{name}"), &raw).ok_or_else(|| {
        "GitHub repository policy response did not include repository data".to_string()
    })
}

const REPO_POLICY_QUERY: &str = r#"
query($owner: String!, $name: String!) {
  repository(owner: $owner, name: $name) {
    defaultBranchRef {
      name
    }
    branchProtectionRules(first: 20) {
      nodes {
        pattern
        requiredApprovingReviewCount
        requiresConversationResolution
        requiresStrictStatusChecks
        requiredStatusCheckContexts
      }
    }
  }
}
"#;

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

#[cfg(test)]
fn parse_pr_summary_index(raw: &str) -> Vec<PrSummary> {
    try_parse_pr_summary_index(raw).unwrap_or_default()
}

fn try_parse_pr_summary_index(raw: &str) -> Result<Vec<PrSummary>, String> {
    let value = serde_json::from_str::<serde_json::Value>(raw)
        .map_err(|error| format!("parse GitHub PR summary index: {error}"))?;
    if !value
        .pointer("/data/repository/pullRequests/nodes")
        .is_some_and(serde_json::Value::is_array)
    {
        return Err("parse GitHub PR summary index: missing pull request connection".to_string());
    }
    let response = serde_json::from_str::<GithubPrSummaryIndexResponse>(raw)
        .map_err(|error| format!("parse GitHub PR summary index: {error}"))?;
    response
        .data
        .repository
        .pull_requests
        .nodes
        .iter()
        .map(pr_summary_from_node)
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| {
            "parse GitHub PR summary index: pull request is missing identity".to_string()
        })
}

pub(crate) fn parse_repo_policy(repo_remote: &str, raw: &str) -> Option<RepoPolicyCache> {
    let response = serde_json::from_str::<GithubPrSummaryIndexResponse>(raw).ok()?;
    let repository = response.data.repository;
    let default_branch = (!repository.default_branch_ref.name.trim().is_empty())
        .then_some(repository.default_branch_ref.name);
    let selected_rule = select_branch_protection_rule(
        &repository.branch_protection_rules.nodes,
        default_branch.as_deref(),
    );
    Some(RepoPolicyCache {
        repo_remote: repo_remote.to_string(),
        default_branch,
        required_approvals: selected_rule
            .map(|rule| rule.required_approving_review_count)
            .unwrap_or(0),
        require_conversation_resolution: selected_rule
            .map(|rule| rule.requires_conversation_resolution)
            .unwrap_or(false),
        require_branch_up_to_date: selected_rule
            .map(|rule| rule.requires_strict_status_checks)
            .unwrap_or(false),
        required_checks: selected_rule
            .map(|rule| normalized_required_checks(&rule.required_status_check_contexts))
            .unwrap_or_default(),
        merge_queue_required: false,
        refreshed_unix_ms: unix_seconds().max(0) as u64,
        error: None,
    })
}

fn select_branch_protection_rule<'a>(
    rules: &'a [GithubBranchProtectionRule],
    default_branch: Option<&str>,
) -> Option<&'a GithubBranchProtectionRule> {
    let default_branch = default_branch.unwrap_or_default();
    rules
        .iter()
        .find(|rule| rule.pattern == default_branch)
        .or_else(|| rules.iter().find(|rule| rule.pattern == "*"))
        .or_else(|| rules.first())
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
    let node = serde_json::from_str::<GithubPullRequest>(&raw)
        .map_err(|error| format!("parse gh pr view output: {error}"))?;
    let summary = pr_summary_from_node(&node)
        .ok_or_else(|| "parse gh pr view output: missing pull request number".to_string())?;
    Ok(Some((summary, raw)))
}

fn fetch_pr_details(
    path: &std::path::Path,
    branch: &str,
    pr_number: u64,
    head_sha: &str,
    config: &Config,
) -> Result<PrDetailsObservation, String> {
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
    let details = try_parse_pr_details(&raw)?;
    let review_comments = fetch_inline_review_comments(path, pr_number, config);
    let ci_failures = if details.failing_checks.is_empty() {
        Ok(Vec::new())
    } else {
        fetch_ci_failures(path, branch, head_sha, config)
    };
    Ok(PrDetailsObservation {
        association: PrDetailsAssociation {
            pr_number,
            head_sha: head_sha.to_string(),
        },
        comments: Ok(details.comments),
        reviews: Ok(details.reviews),
        review_comments,
        files: Ok(details.files),
        failing_checks: Ok(details.failing_checks),
        check_contexts: Ok(details.check_contexts),
        ci_failures,
    })
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
            .take(8)
            .collect(),
        failing_checks,
        check_contexts,
        ci_failures: Vec::new(),
    })
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
        let message = output.stderr.trim();
        return Err(if message.is_empty() {
            format!("gh run list exited with {}", output.status)
        } else {
            format!("gh run list: {message}")
        });
    }
    let runs = serde_json::from_str::<Vec<GhRunListItem>>(&output.stdout)
        .map_err(|error| format!("parse gh run list output: {error}"))?;
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
        let log_tail = fetch_failed_run_log_tail(path, &run_id, config)?;
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
        let message = output.stderr.trim();
        return Err(if message.is_empty() {
            format!("gh run view exited with {}", output.status)
        } else {
            format!("gh run view: {message}")
        });
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
    try_parse_review_thread_comments(&raw)
}

const PR_REVIEW_THREADS_QUERY: &str = r#"
query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      reviewThreads(first: 100) {
        nodes {
          id
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
    if !value
        .pointer("/data/repository/pullRequest/reviewThreads/nodes")
        .is_some_and(serde_json::Value::is_array)
    {
        return Err("parse GitHub review threads: missing review thread connection".to_string());
    }
    let response = serde_json::from_value::<GithubPrSummaryIndexResponse>(value)
        .map_err(|error| format!("parse GitHub review threads: {error}"))?;
    let mut comments = Vec::new();
    for thread in response.data.repository.pull_request.review_threads.nodes {
        for object in thread.comments.nodes {
            if comments.len() >= 100 {
                return Ok(comments);
            }
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

fn collect_check_contexts(rollup: &GithubStatusCheckRollup) -> Vec<PrCheckContext> {
    status_contexts_from_rollup(rollup)
        .into_iter()
        .filter_map(|context| {
            let name = context.name.clone().or(context.context.clone())?;
            let name = name.trim().to_string();
            if name.is_empty() {
                return None;
            }
            Some(PrCheckContext {
                name,
                state: check_context_state(&context),
            })
        })
        .take(64)
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
          refreshed_unix_ms integer not null,
          observation_error text
        );

        create table if not exists pr_details_cache (
          branch text primary key,
          pr_number integer,
          head_sha text,
          comments text not null,
          reviews text not null,
          review_comments text not null,
          files text not null,
          failing_checks text not null,
          check_contexts text not null default '[]',
          ci_failures text not null default '[]',
          refreshed_unix_ms integer not null,
          observation_error text
        );

        create table if not exists repo_policy_cache (
          repo_remote text primary key,
          default_branch text,
          required_approvals integer not null default 0,
          require_conversation_resolution integer not null default 0,
          require_branch_up_to_date integer not null default 0,
          required_checks text not null default '[]',
          merge_queue_required integer not null default 0,
          refreshed_unix_ms integer not null,
          error text
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
    if !table_has_column(conn, "pr_details_cache", "check_contexts")? {
        conn.execute(
            "alter table pr_details_cache add column check_contexts text not null default '[]'",
            [],
        )
        .map_err(|error| format!("migrate pr_details_cache check_contexts column: {error}"))?;
    }
    if !table_has_column(conn, "pr_details_cache", "pr_number")? {
        conn.execute(
            "alter table pr_details_cache add column pr_number integer",
            [],
        )
        .map_err(|error| format!("migrate pr_details_cache pr_number column: {error}"))?;
    }
    if !table_has_column(conn, "pr_details_cache", "head_sha")? {
        conn.execute("alter table pr_details_cache add column head_sha text", [])
            .map_err(|error| format!("migrate pr_details_cache head_sha column: {error}"))?;
    }
    if !table_has_column(conn, "pr_cache", "observation_error")? {
        conn.execute("alter table pr_cache add column observation_error text", [])
            .map_err(|error| format!("migrate pr_cache observation_error column: {error}"))?;
    }
    if !table_has_column(conn, "pr_details_cache", "observation_error")? {
        conn.execute(
            "alter table pr_details_cache add column observation_error text",
            [],
        )
        .map_err(|error| format!("migrate pr_details_cache observation_error column: {error}"))?;
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

fn remove_pr_cache(repo: &Repository, branch: &str) -> Result<(), String> {
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

fn remove_pr_details_cache(repo: &Repository, branch: &str) -> Result<(), String> {
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

fn load_pr_details_cache_record(
    repo: &Repository,
    branch: &str,
) -> Result<Option<PersistedPrDetails>, String> {
    observability::with_writable_db(repo, |conn| {
        conn.query_row(
            "select comments, reviews, review_comments, files, failing_checks, ci_failures,
                    check_contexts, pr_number, head_sha, observation_error
               from pr_details_cache
              where branch = ?1",
            params![branch],
            |row| {
                let pr_number = row.get::<_, Option<i64>>(7)?;
                let head_sha = row.get::<_, Option<String>>(8)?;
                let association = match (pr_number, head_sha) {
                    (Some(pr_number), Some(head_sha)) if pr_number >= 0 && !head_sha.is_empty() => {
                        Some(PrDetailsAssociation {
                            pr_number: pr_number as u64,
                            head_sha,
                        })
                    }
                    _ => None,
                };
                let errors = row
                    .get::<_, Option<String>>(9)?
                    .filter(|error| !error.is_empty())
                    .into_iter()
                    .collect();
                Ok(PersistedPrDetails {
                    details: PrDetails {
                        comments: decode_pr_comments(&row.get::<_, String>(0)?),
                        reviews: decode_pr_reviews(&row.get::<_, String>(1)?),
                        review_comments: decode_pr_review_comments(&row.get::<_, String>(2)?),
                        files: decode_string_values(&row.get::<_, String>(3)?),
                        failing_checks: decode_string_values(&row.get::<_, String>(4)?),
                        ci_failures: decode_ci_failures(&row.get::<_, String>(5)?),
                        check_contexts: decode_check_contexts(&row.get::<_, String>(6)?),
                    },
                    association,
                    errors,
                })
            },
        )
        .optional()
        .map_err(|error| format!("read PR details cache: {error}"))
    })
}

#[cfg(test)]
fn load_pr_details_cache(repo: &Repository, branch: &str) -> Option<PrDetails> {
    load_pr_details_cache_record(repo, branch)
        .ok()
        .flatten()
        .map(|record| record.details)
}

#[cfg(test)]
pub(crate) fn save_pr_details_cache(
    repo: &Repository,
    branch: &str,
    details: &PrDetails,
) -> Result<(), String> {
    let association = observability::with_writable_db(repo, |conn| {
        conn.query_row(
            "select number, head_sha from pr_cache where branch = ?1",
            params![branch],
            |row| {
                Ok(PrDetailsAssociation {
                    pr_number: row_u64(row, 0)?,
                    head_sha: row.get(1)?,
                })
            },
        )
        .map_err(|error| format!("read PR summary association: {error}"))
    })?;
    save_pr_details_cache_for_association(repo, branch, details, &association, &[])
}

fn save_pr_details_cache_for_association(
    repo: &Repository,
    branch: &str,
    details: &PrDetails,
    association: &PrDetailsAssociation,
    errors: &[String],
) -> Result<(), String> {
    observability::with_writable_db(repo, |conn| {
        conn.execute(
            "insert into pr_details_cache (
                branch, pr_number, head_sha, comments, reviews, review_comments, files,
                failing_checks, ci_failures, check_contexts, refreshed_unix_ms, observation_error
             ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
              on conflict(branch) do update set
                pr_number = excluded.pr_number,
                head_sha = excluded.head_sha,
                comments = excluded.comments,
                reviews = excluded.reviews,
                review_comments = excluded.review_comments,
                files = excluded.files,
                failing_checks = excluded.failing_checks,
                ci_failures = excluded.ci_failures,
                check_contexts = excluded.check_contexts,
                refreshed_unix_ms = excluded.refreshed_unix_ms,
                observation_error = excluded.observation_error",
            params![
                branch,
                sqlite_i64(association.pr_number, "PR number")?,
                association.head_sha.as_str(),
                encode_pr_comments(&details.comments),
                encode_pr_reviews(&details.reviews),
                encode_pr_review_comments(&details.review_comments),
                encode_string_values(&details.files),
                encode_string_values(&details.failing_checks),
                encode_ci_failures(&details.ci_failures),
                encode_check_contexts(&details.check_contexts),
                unix_seconds(),
                (!errors.is_empty()).then(|| errors.join("\n")),
            ],
        )
        .map_err(|error| format!("write PR details cache: {error}"))?;
        Ok(())
    })
}

fn persist_observation_errors(repo: &Repository, branch: &str, cache: &mut PrCache) {
    let result = observability::with_writable_db(repo, |conn| {
        conn.execute(
            "update pr_cache set observation_error = ?2 where branch = ?1",
            params![branch, cache.summary_error.as_deref()],
        )
        .map_err(|error| format!("write PR observation error: {error}"))?;
        Ok(())
    });
    if let Err(error) = result {
        cache.persistence_error = Some(error);
        cache.rebuild_error();
    }
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
                comment_count, merged, draft, last_refreshed, refreshed_unix_ms,
                observation_error
             ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)
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
                refreshed_unix_ms = excluded.refreshed_unix_ms,
                observation_error = excluded.observation_error",
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
                cache.summary_error.as_deref(),
            ],
        )
        .map_err(|error| format!("write PR cache: {error}"))?;
        Ok(())
    })
}

pub(crate) fn save_repo_policy_cache(
    repo: &Repository,
    policy: &RepoPolicyCache,
) -> Result<(), String> {
    observability::with_writable_db(repo, |conn| {
        conn.execute(
            "insert into repo_policy_cache (
                repo_remote, default_branch, required_approvals,
                require_conversation_resolution, require_branch_up_to_date,
                required_checks, merge_queue_required, refreshed_unix_ms, error
             ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
              on conflict(repo_remote) do update set
                default_branch = excluded.default_branch,
                required_approvals = excluded.required_approvals,
                require_conversation_resolution = excluded.require_conversation_resolution,
                require_branch_up_to_date = excluded.require_branch_up_to_date,
                required_checks = excluded.required_checks,
                merge_queue_required = excluded.merge_queue_required,
                refreshed_unix_ms = excluded.refreshed_unix_ms,
                error = excluded.error",
            params![
                policy.repo_remote.as_str(),
                policy.default_branch.as_deref(),
                sqlite_i64(policy.required_approvals, "required approvals")?,
                policy.require_conversation_resolution,
                policy.require_branch_up_to_date,
                encode_string_values(&policy.required_checks),
                policy.merge_queue_required,
                sqlite_i64(policy.refreshed_unix_ms, "policy refresh time")?,
                policy.error.as_deref(),
            ],
        )
        .map_err(|error| format!("write repo policy cache: {error}"))?;
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

fn encode_check_contexts(contexts: &[PrCheckContext]) -> String {
    serde_json::to_string(contexts).unwrap_or_else(|_| "[]".to_string())
}

fn decode_check_contexts(raw: &str) -> Vec<PrCheckContext> {
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
    use crate::config::Config;
    use crate::test_support::write_executable;
    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn migrates_existing_pr_cache_schema_additively_without_losing_rows() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            create table pr_cache (
              branch text primary key, number integer not null, title text not null,
              url text not null, state text not null, review_decision text not null,
              head_ref text not null, base_ref text not null, head_sha text not null,
              updated_at text not null, check_status text not null, merged integer not null,
              draft integer not null, last_refreshed text not null,
              refreshed_unix_ms integer not null
            );
            create table pr_details_cache (
              branch text primary key, comments text not null, reviews text not null,
              review_comments text not null, files text not null,
              failing_checks text not null, refreshed_unix_ms integer not null
            );
            insert into pr_cache values (
              'feature', 42, 'Old row', 'https://example.test/42', 'OPEN', '',
              'feature', 'main', 'head-a', '2026-01-01', 'pending', 0, 0,
              'before migration', 123
            );
            insert into pr_details_cache values (
              'feature', '[]', '[]', '[]', '[\"src/lib.rs\"]', '[]', 123
            );
            ",
        )
        .unwrap();

        migrate_pr_cache_schema(&conn).unwrap();

        assert!(table_has_column(&conn, "pr_cache", "body").unwrap());
        assert!(table_has_column(&conn, "pr_cache", "observation_error").unwrap());
        assert!(table_has_column(&conn, "pr_details_cache", "pr_number").unwrap());
        assert!(table_has_column(&conn, "pr_details_cache", "head_sha").unwrap());
        let old_row = conn
            .query_row(
                "select title, body, comment_count from pr_cache where branch = 'feature'",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(old_row, ("Old row".to_string(), String::new(), 0));
        let association = conn
            .query_row(
                "select pr_number, head_sha from pr_details_cache where branch = 'feature'",
                [],
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(association, (None, None));
    }

    #[test]
    fn direct_and_index_summary_paths_produce_equivalent_cache_facts() {
        let temp = unique_temp_dir("prism-pr-equivalent-summary-paths");
        fs::create_dir_all(&temp).unwrap();
        let direct_repo = Repository::with_config_dir_for_test(
            temp.join("direct-repo"),
            temp.join("direct-config"),
        );
        let index_repo = Repository::with_config_dir_for_test(
            temp.join("index-repo"),
            temp.join("index-config"),
        );
        let config = test_config();
        let old_summary = test_summary("feature", "head-a", 1);
        let new_summary = test_summary("feature", "head-a", 2);
        let details = PrDetails {
            comments: vec![PrComment {
                body: "preserved".to_string(),
                ..PrComment::default()
            }],
            ..PrDetails::default()
        };
        let mut direct = PrCache::observed(old_summary.clone(), Some(details.clone()));
        record_pr_summary(&direct_repo, "feature", &mut direct, new_summary.clone());

        let poll_started_at = Instant::now();
        let mut sessions = vec![test_session(
            "feature",
            PrCache::observed(old_summary, Some(details)),
        )];
        sessions[0].pr.begin_summary_poll(poll_started_at);
        refresh_pr_summary_index_for_sessions(
            &[PrCacheRepository {
                repo: &index_repo,
                config: &config,
            }],
            &mut sessions,
            0,
            vec![new_summary.clone()],
            poll_started_at,
        );

        assert_eq!(direct.summary(), Some(&new_summary));
        assert_eq!(sessions[0].pr.summary(), direct.summary());
        assert_eq!(
            sessions[0].pr.details().unwrap().comments[0].body,
            direct.details().unwrap().comments[0].body
        );
        assert!(direct.trusted_summary_and_details().is_ok());
        assert!(sessions[0].pr.trusted_summary_and_details().is_ok());

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn create_pr_uses_fill_with_explicit_empty_body_and_default_base_when_configured() {
        assert_eq!(
            create_pr_args(Some("main"), "", None),
            vec!["pr", "create", "--fill", "--body", "", "--base", "main"]
        );
        assert_eq!(
            create_pr_args(None, "manual description", None),
            vec!["pr", "create", "--fill", "--body", "manual description"]
        );
        assert_eq!(
            create_pr_args(Some("main"), "manual description", Some("owner/repo")),
            vec![
                "pr",
                "create",
                "--fill",
                "--body",
                "manual description",
                "--repo",
                "owner/repo",
                "--base",
                "main"
            ]
        );
    }

    #[test]
    fn merge_pr_args_use_configured_method() {
        assert_eq!(
            merge_pr_args("42", MergeMethod::Squash, "abc123"),
            vec![
                "pr",
                "merge",
                "42",
                "--squash",
                "--match-head-commit",
                "abc123"
            ]
        );
        assert_eq!(
            merge_pr_args("42", MergeMethod::Merge, "abc123"),
            vec![
                "pr",
                "merge",
                "42",
                "--merge",
                "--match-head-commit",
                "abc123"
            ]
        );
        assert_eq!(
            merge_pr_args("42", MergeMethod::Rebase, "abc123"),
            vec![
                "pr",
                "merge",
                "42",
                "--rebase",
                "--match-head-commit",
                "abc123"
            ]
        );
    }

    #[test]
    fn merge_pull_request_does_not_delegate_branch_deletion_to_gh() {
        let temp = unique_temp_dir("prism-merge-no-delete-branch-test");
        let worktree = temp.join("worktree");
        fs::create_dir_all(&worktree).unwrap();
        let log = temp.join("gh.log");
        let gh = temp.join("gh");
        write_executable(
            &gh,
            &format!(
                r#"#!/bin/sh
printf 'pwd=%s\nargs=%s\n' "$PWD" "$*" > '{}'
exit 0
"#,
                log.display()
            ),
        );

        let mut config = test_config();
        config
            .tools
            .insert("gh".to_string(), gh.display().to_string());

        merge_pull_request(&config, &worktree, 42, "abc123").unwrap();

        let commands = fs::read_to_string(&log).unwrap();
        let actual_pwd = commands
            .lines()
            .find_map(|line| line.strip_prefix("pwd="))
            .expect("gh shim should record its working directory");
        assert_eq!(
            PathBuf::from(actual_pwd).canonicalize().unwrap(),
            worktree.canonicalize().unwrap()
        );
        assert!(commands.contains("args=pr merge 42 --squash --match-head-commit abc123"));
        assert!(!commands.contains("--delete-branch"));

        let _ = fs::remove_dir_all(temp);
    }

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
        assert_eq!(details.check_contexts[0].name, "test");
        assert_eq!(details.check_contexts[0].state, PrCheckState::Failed);
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
    fn resolve_review_thread_args_target_exact_thread_id() {
        let args = resolve_review_thread_args("PRRT_thread_1");

        assert_eq!(args[0], "api");
        assert_eq!(args[1], "graphql");
        assert!(args.contains(&"thread=PRRT_thread_1".to_string()));
        assert!(args
            .iter()
            .any(|arg| arg.contains("resolveReviewThread") && arg.contains("threadId: $thread")));
    }

    #[test]
    fn phase_1_failed_forced_summary_keeps_stale_display_but_authoritative_access_errors() {
        let temp = unique_temp_dir("prism-phase-1-failed-summary-refresh");
        fs::create_dir_all(&temp).unwrap();
        let gh = temp.join("gh");
        write_executable(&gh, "#!/bin/sh\necho 'GitHub unavailable' >&2\nexit 1\n");
        let git = temp.join("git");
        write_executable(
            &git,
            "#!/bin/sh\ncase \"$*\" in *\"remote get-url origin\"*) echo git@github.com:owner/repo.git ;; esac\n",
        );
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let mut config = test_config();
        config
            .tools
            .insert("gh".to_string(), gh.display().to_string());
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        let stale_summary = test_summary("feature", "head-a", 2);
        let stale_details = PrDetails {
            files: vec!["src/stale.rs".to_string()],
            ..PrDetails::default()
        };
        let mut cache = PrCache::observed(stale_summary.clone(), Some(stale_details));
        cache.record_summary_observation(Some(stale_summary.clone()), "before failure".to_string());

        assert!(refresh_pr_cache(&repo, "feature", &mut cache, &temp, &config, true).is_err());

        assert_eq!(cache.summary(), Some(&stale_summary));
        assert_eq!(cache.details().unwrap().files, vec!["src/stale.rs"]);
        assert_eq!(cache.last_refreshed(), Some("before failure"));
        assert!(cache.display_error().is_some_and(|error| !error.is_empty()));
        assert!(pr_summary_or_error(&cache).is_err());

        let _ = fs::remove_dir_all(repo.prism_dir());
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn phase_1_details_for_head_a_are_rejected_after_same_pr_advances_to_head_b() {
        let temp = unique_temp_dir("prism-phase-1-stale-head-details");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let head_a = test_summary("feature", "head-a", 0);
        let mut cache = PrCache::observed(head_a.clone(), None);
        let mut poll_result = cache.begin_details_poll();
        let mut observation = successful_details_observation_for(&head_a);
        observation.review_comments = Ok(vec![PrReviewComment {
            thread_id: "PRRT_from_head_a".to_string(),
            body: "stale".to_string(),
            ..PrReviewComment::default()
        }]);
        poll_result.record_details_observation(observation);
        cache.record_summary_observation(
            Some(test_summary("feature", "head-b", 0)),
            "advanced".to_string(),
        );

        let applied = record_pr_details_poll_result(&repo, "feature", &mut cache, poll_result);

        assert!(!applied);
        assert!(cache.details().is_none());
        assert!(load_pr_details_cache(&repo, "feature").is_none());

        let _ = fs::remove_dir_all(repo.prism_dir());
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn phase_1_malformed_github_summary_output_is_failure_not_authoritative_absence() {
        let temp = unique_temp_dir("prism-phase-1-malformed-summary");
        fs::create_dir_all(&temp).unwrap();
        let gh = temp.join("gh");
        write_executable(&gh, "#!/bin/sh\nprintf '{not valid json'\n");
        let mut config = test_config();
        config
            .tools
            .insert("gh".to_string(), gh.display().to_string());

        let result = fetch_pr_summary(&temp, "feature", &config);

        assert!(
            result.is_err(),
            "malformed output must not mean no pull request"
        );

        let _ = fs::remove_dir_all(temp);
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
            check_contexts: vec![PrCheckContext {
                name: "test".to_string(),
                state: PrCheckState::Failed,
            }],
            ci_failures: vec![CiFailure {
                workflow: "CI".to_string(),
                name: "test".to_string(),
                conclusion: "failure".to_string(),
                url: "https://github.com/example/repo/actions/runs/99".to_string(),
                run_id: "99".to_string(),
                log_tail: "failed log".to_string(),
            }],
        };
        let mut cache = PrCache::observed(summary, Some(details));
        let observed = cache.summary().cloned();
        cache.record_summary_observation(observed, "now".to_string());

        save_pr_cache(&repo, "feature", &cache).unwrap();
        save_pr_details_cache(&repo, "feature", cache.details().unwrap()).unwrap();
        let loaded = load_pr_cache(&repo, "feature");
        let prism_dir = repo.prism_dir();

        assert_eq!(loaded.summary().unwrap().number, 42);
        assert_eq!(loaded.summary().unwrap().merge_state_status, "CLEAN");
        let loaded_details = loaded.details().unwrap();
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
        assert_eq!(loaded_details.check_contexts[0].name, "test");
        assert_eq!(loaded_details.check_contexts[0].state, PrCheckState::Failed);
        assert_eq!(loaded_details.ci_failures[0].log_tail, "");

        let _ = fs::remove_dir_all(prism_dir);
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn restart_accepts_only_details_associated_with_persisted_pr_and_head() {
        let temp = unique_temp_dir("prism-pr-details-association-test");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let summary = test_summary("feature", "head-a", 1);
        let details = PrDetails {
            comments: vec![PrComment {
                body: "associated".to_string(),
                ..PrComment::default()
            }],
            ..PrDetails::default()
        };
        let mut cache = PrCache::observed(summary.clone(), Some(details.clone()));
        cache.record_summary_observation(Some(summary.clone()), "now".to_string());
        save_pr_cache(&repo, "feature", &cache).unwrap();
        save_pr_details_cache(&repo, "feature", &details).unwrap();

        let associated = load_pr_cache(&repo, "feature");
        assert_eq!(
            associated.details_observation_quality(),
            PrObservationQuality::PreservedStale
        );
        assert!(associated.trusted_details().is_err());

        let moved = PrCache::observed(test_summary("feature", "head-b", 1), None);
        save_pr_cache(&repo, "feature", &moved).unwrap();
        let stale = load_pr_cache(&repo, "feature");
        assert!(stale.details().is_none());

        save_pr_cache(&repo, "feature", &cache).unwrap();
        observability::with_writable_db(&repo, |conn| {
            conn.execute(
                "update pr_details_cache set pr_number = null, head_sha = null where branch = ?1",
                params!["feature"],
            )
            .map_err(|error| error.to_string())?;
            Ok(())
        })
        .unwrap();
        let mut legacy = load_pr_cache(&repo, "feature");
        assert!(legacy.details().is_some());
        assert_eq!(
            legacy.details_observation_quality(),
            PrObservationQuality::PreservedStale
        );
        assert!(legacy.trusted_details().is_err());
        let mutation =
            legacy.record_summary_observation(Some(summary.clone()), "refreshed".to_string());
        persist_pr_summary_mutation(&repo, "feature", &mut legacy, mutation);
        assert!(load_pr_cache(&repo, "feature").details.is_none());

        save_pr_details_cache_for_association(
            &repo,
            "feature",
            &details,
            &PrDetailsAssociation::from_summary(&summary),
            &["review threads: unavailable".to_string()],
        )
        .unwrap();
        let partial = load_pr_cache(&repo, "feature");
        assert_eq!(
            partial.details_observation_quality(),
            PrObservationQuality::PreservedStale
        );
        assert!(partial.trusted_details().is_err());

        let _ = fs::remove_dir_all(repo.prism_dir());
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn successful_details_write_does_not_clear_previous_persistence_failure() {
        let temp = unique_temp_dir("prism-pr-persistence-error-test");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let mut cache = cache_with_observed_details();
        cache.record_persistence_result(Err("summary write failed".to_string()));
        save_pr_cache(&repo, "feature", &cache).unwrap();
        let poll_result = cache.begin_details_poll();

        assert!(record_pr_details_poll_result(
            &repo,
            "feature",
            &mut cache,
            poll_result,
        ));

        assert_eq!(cache.display_error(), Some("summary write failed"));
        assert!(cache.trusted_details().is_err());

        let _ = fs::remove_dir_all(repo.prism_dir());
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn obsolete_details_generation_is_rejected_for_same_pr_and_head() {
        let temp = unique_temp_dir("prism-obsolete-details-generation-test");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let mut cache = cache_with_observed_details();
        let obsolete = cache.begin_details_poll();
        let _current = cache.begin_details_poll();

        assert!(!record_pr_details_poll_result(
            &repo, "feature", &mut cache, obsolete,
        ));
        assert_eq!(cache.details().unwrap().comments[0].body, "old comment");

        let _ = fs::remove_dir_all(repo.prism_dir());
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
        let mut cache = PrCache::observed(summary.clone(), Some(details));
        cache.record_summary_failure("previous error".to_string());

        cache.record_summary_observation(Some(summary), "now".to_string());

        assert!(cache.details().is_some());
        assert!(cache.display_error().is_none());
        assert_eq!(cache.last_refreshed(), Some("now"));
    }

    #[test]
    fn pr_summary_refresh_drops_details_when_signature_changes() {
        let old_summary = test_summary("feature", "abc123", 2);
        let new_summary = test_summary("feature", "def456", 2);
        let mut cache = PrCache::observed(old_summary, Some(PrDetails::default()));

        cache.record_summary_observation(Some(new_summary.clone()), "now".to_string());

        assert_eq!(cache.summary(), Some(&new_summary));
        assert!(cache.details().is_none());
    }

    #[test]
    fn summary_refresh_preserves_details_when_pr_and_head_are_unchanged() {
        let old_summary = test_summary("feature", "abc123", 2);
        let mut new_summary = old_summary.clone();
        new_summary.review_decision = "APPROVED".to_string();
        new_summary.updated_at = "2026-01-02T00:00:00Z".to_string();
        let details = PrDetails {
            comments: vec![PrComment {
                body: "keep me".to_string(),
                ..PrComment::default()
            }],
            ..PrDetails::default()
        };
        let mut cache = PrCache::observed(old_summary, Some(details));

        cache.record_summary_observation(Some(new_summary), "now".to_string());

        assert_eq!(cache.details().unwrap().comments[0].body, "keep me");
        assert!(cache.trusted_details().is_ok());
    }

    fn cache_with_observed_details() -> PrCache {
        let summary = test_summary("feature", "abc123", 2);
        PrCache::observed(
            summary,
            Some(PrDetails {
                comments: vec![PrComment {
                    body: "old comment".to_string(),
                    ..PrComment::default()
                }],
                review_comments: vec![PrReviewComment {
                    thread_id: "old-thread".to_string(),
                    ..PrReviewComment::default()
                }],
                failing_checks: vec!["old-check".to_string()],
                check_contexts: vec![PrCheckContext {
                    name: "old-check".to_string(),
                    state: PrCheckState::Failed,
                }],
                ci_failures: vec![CiFailure {
                    run_id: "old-run".to_string(),
                    log_tail: "old log".to_string(),
                    ..CiFailure::default()
                }],
                ..PrDetails::default()
            }),
        )
    }

    fn successful_details_observation_for(summary: &PrSummary) -> PrDetailsObservation {
        PrDetailsObservation {
            association: PrDetailsAssociation::from_summary(summary),
            comments: Ok(Vec::new()),
            reviews: Ok(Vec::new()),
            review_comments: Ok(Vec::new()),
            files: Ok(Vec::new()),
            failing_checks: Ok(Vec::new()),
            check_contexts: Ok(Vec::new()),
            ci_failures: Ok(Vec::new()),
        }
    }

    #[test]
    fn partial_comment_failure_preserves_previous_comments() {
        let (temp, repo, mut cache, summary) = persisted_cache_with_observed_details();
        let mut observation = successful_details_observation_for(&summary);
        observation.comments = Err("comments unavailable".to_string());

        assert!(record_pr_details_observation(
            &repo,
            "feature",
            &mut cache,
            observation,
        ));

        assert_eq!(cache.details().unwrap().comments[0].body, "old comment");
        assert_eq!(
            cache.details_observation_quality(),
            PrObservationQuality::PreservedStale
        );
        assert!(cache.trusted_details().is_err());
        let loaded = load_pr_cache(&repo, "feature");
        assert_eq!(loaded.details().unwrap().comments[0].body, "old comment");
        assert_eq!(
            loaded.details_observation_quality(),
            PrObservationQuality::PreservedStale
        );
        assert!(
            loaded
                .display_error()
                .is_some_and(|error| error.contains("comments: comments unavailable"))
        );
        assert!(loaded.trusted_details().is_err());

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn partial_review_thread_failure_preserves_previous_threads() {
        let (temp, repo, mut cache, summary) = persisted_cache_with_observed_details();
        let mut observation = successful_details_observation_for(&summary);
        observation.review_comments = Err("threads unavailable".to_string());

        assert!(record_pr_details_observation(
            &repo,
            "feature",
            &mut cache,
            observation,
        ));

        assert_eq!(
            cache.details().unwrap().review_comments[0].thread_id,
            "old-thread"
        );
        assert!(cache.trusted_details().is_err());
        let loaded = load_pr_cache(&repo, "feature");
        assert_eq!(
            loaded.details().unwrap().review_comments[0].thread_id,
            "old-thread"
        );
        assert!(
            loaded
                .display_error()
                .is_some_and(|error| error.contains("review threads: threads unavailable"))
        );
        assert!(loaded.trusted_details().is_err());

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn partial_check_failure_preserves_previous_checks() {
        let (temp, repo, mut cache, summary) = persisted_cache_with_observed_details();
        let mut observation = successful_details_observation_for(&summary);
        observation.failing_checks = Err("checks unavailable".to_string());
        observation.check_contexts = Err("check contexts unavailable".to_string());

        assert!(record_pr_details_observation(
            &repo,
            "feature",
            &mut cache,
            observation,
        ));

        assert_eq!(cache.details().unwrap().failing_checks, vec!["old-check"]);
        assert_eq!(cache.details().unwrap().check_contexts[0].name, "old-check");
        assert!(cache.trusted_details().is_err());
        let loaded = load_pr_cache(&repo, "feature");
        assert_eq!(loaded.details().unwrap().failing_checks, vec!["old-check"]);
        assert_eq!(
            loaded.details().unwrap().check_contexts[0].name,
            "old-check"
        );
        assert!(
            loaded
                .display_error()
                .is_some_and(|error| error.contains("checks: checks unavailable"))
        );
        assert!(loaded.trusted_details().is_err());

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn partial_ci_log_failure_preserves_previous_logs() {
        let (temp, repo, mut cache, summary) = persisted_cache_with_observed_details();
        let mut observation = successful_details_observation_for(&summary);
        observation.ci_failures = Err("logs unavailable".to_string());

        assert!(record_pr_details_observation(
            &repo,
            "feature",
            &mut cache,
            observation,
        ));

        assert_eq!(cache.details().unwrap().ci_failures[0].log_tail, "old log");
        assert!(cache.trusted_details().is_err());
        let loaded = load_pr_cache(&repo, "feature");
        assert_eq!(loaded.details().unwrap().ci_failures[0].run_id, "old-run");
        assert_eq!(loaded.details().unwrap().ci_failures[0].log_tail, "");
        assert!(
            loaded
                .display_error()
                .is_some_and(|error| error.contains("CI logs: logs unavailable"))
        );
        assert!(loaded.trusted_details().is_err());

        let _ = fs::remove_dir_all(temp);
    }

    fn persisted_cache_with_observed_details() -> (PathBuf, Repository, PrCache, PrSummary) {
        let temp = unique_temp_dir("prism-partial-pr-details");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        let cache = cache_with_observed_details();
        let summary = cache.summary().unwrap().clone();
        save_pr_cache(&repo, "feature", &cache).unwrap();
        save_pr_details_cache(&repo, "feature", cache.details().unwrap()).unwrap();
        (temp, repo, cache, summary)
    }

    #[test]
    fn pr_summary_refresh_clears_cache_when_branch_has_no_pr() {
        let summary = test_summary("feature", "abc123", 2);
        let mut cache = PrCache::observed(summary, Some(PrDetails::default()));
        cache.record_summary_failure("previous error".to_string());

        cache.record_summary_observation(None, "now".to_string());

        assert!(cache.summary().is_none());
        assert!(cache.details().is_none());
        assert!(cache.display_error().is_none());
        assert_eq!(cache.last_refreshed(), Some("now"));
    }

    #[test]
    fn pr_cache_eligibility_excludes_default_detached_missing_remote_and_merged_prs() {
        let merged_summary = PrSummary {
            merged: true,
            ..test_summary("feature", "abc123", 0)
        };
        let mut merged = test_session("feature", PrCache::observed(merged_summary, None));
        merged.path = std::path::PathBuf::from("/not-used");

        assert!(
            !PrCacheEligibility {
                is_default_branch: true,
                is_detached: false,
                has_github_remote: true,
            }
            .can_observe()
        );
        assert!(
            !PrCacheEligibility {
                is_default_branch: false,
                is_detached: true,
                has_github_remote: true,
            }
            .can_observe()
        );
        assert!(
            !PrCacheEligibility {
                is_default_branch: false,
                is_detached: false,
                has_github_remote: false,
            }
            .can_observe()
        );
        assert!(!merged.pr.pollable(PrCacheEligibility {
            is_default_branch: false,
            is_detached: false,
            has_github_remote: true,
        }));
    }

    #[test]
    fn pr_cache_comment_count_prefers_loaded_details_over_summary() {
        let cache = PrCache::observed(
            test_summary("feature", "abc123", 12),
            Some(PrDetails {
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
        );

        assert_eq!(pr_cache_comment_count(&cache), 3);
        assert!(pr_cache_has_comments(&cache));
    }

    #[test]
    fn preserved_stale_cache_remains_displayable_but_has_distinct_render_signature() {
        let fresh = cache_with_observed_details();
        let mut stale = fresh.clone();
        stale.mark_preserved_stale();

        assert_eq!(stale.summary(), fresh.summary());
        assert!(stale.details().is_some());
        assert_ne!(
            pr_cache_render_signature(&stale),
            pr_cache_render_signature(&fresh)
        );
        assert!(stale.trusted_summary_and_details().is_err());
    }

    #[test]
    fn pr_summary_index_refresh_updates_sessions_and_pr_cache_storage() {
        let temp = unique_temp_dir("prism-pr-summary-index-test");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let mut config = test_config();
        config.default_base = Some("main".to_string());
        let git = temp.join("git");
        write_executable(&git, "#!/bin/sh\nprintf 'abc123\\n'\n");
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
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
                PrCache::observed(test_summary("main", "main", 0), None),
            ),
            test_session(
                "feature",
                PrCache::observed(feature_summary.clone(), Some(details.clone())),
            ),
            test_session("stale", PrCache::observed(stale_summary.clone(), None)),
        ];
        for session in &mut sessions {
            session.path = temp.clone();
        }

        let poll_started_at = Instant::now();
        for session in &mut sessions {
            session.pr.begin_summary_poll(poll_started_at);
        }
        refresh_pr_summary_index_for_sessions(
            &[PrCacheRepository {
                repo: &repo,
                config: &config,
            }],
            &mut sessions,
            0,
            vec![feature_summary.clone()],
            poll_started_at,
        );

        assert!(sessions[0].pr.summary().is_none());
        assert!(sessions[2].pr.summary().is_none());
        assert_eq!(sessions[1].pr.summary(), Some(&feature_summary));
        assert!(sessions[1].pr.details().is_some());

        let loaded = load_pr_cache(&repo, "feature");
        assert_eq!(loaded.summary(), Some(&feature_summary));
        assert_eq!(loaded.details().unwrap().comments[0].body, "new comment");
        assert!(load_pr_cache(&repo, "stale").summary().is_none());

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
        let mut cache = PrCache::observed(summary.clone(), None);
        cache.record_summary_observation(Some(summary.clone()), "created".to_string());
        cache.begin_summary_poll(poll_started_at);
        cache.begin_summary_poll(poll_started_at + std::time::Duration::from_millis(1));
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

        assert_eq!(sessions[0].pr.summary(), Some(&summary));
        assert_eq!(load_pr_cache(&repo, "feature").summary(), Some(&summary));

        let _ = fs::remove_dir_all(repo.prism_dir());
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn merged_pr_from_previous_branch_generation_is_not_reused() {
        let temp = unique_temp_dir("prism-reused-branch-pr-test");
        fs::create_dir_all(&temp).unwrap();
        let git = temp.join("git");
        fs::write(
            &git,
            "#!/bin/sh\ncase \"$*\" in *\"remote get-url origin\"*) echo git@github.com:owner/repo.git ;; *\"merge-base --is-ancestor\"*) exit 1 ;; *) printf 'new-head\\n' ;; esac\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&git).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&git, permissions).unwrap();

        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let mut config = test_config();
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        let mut old_summary = test_summary("feature", "old-head", 0);
        old_summary.state = "MERGED".to_string();
        old_summary.merged = true;
        let mut sessions = vec![test_session("feature", PrCache::default())];
        sessions[0].path = temp.join("feature");
        let old_cache = PrCache::observed(old_summary.clone(), None);
        save_pr_cache(&repo, "feature", &old_cache).unwrap();

        let loaded = load_pr_cache_for_branch(&repo, &config, "feature", &sessions[0].path);

        assert_eq!(loaded.summary(), Some(&old_summary));
        assert!(loaded.trusted_summary().is_err());

        let poll_started_at = Instant::now();
        for session in &mut sessions {
            session.pr.begin_summary_poll(poll_started_at);
        }
        refresh_pr_summary_index_for_sessions(
            &[PrCacheRepository {
                repo: &repo,
                config: &config,
            }],
            &mut sessions,
            0,
            vec![old_summary],
            poll_started_at,
        );

        assert!(sessions[0].pr.summary().is_none());
        assert!(load_pr_cache(&repo, "feature").summary().is_none());

        let _ = fs::remove_dir_all(repo.prism_dir());
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn open_pr_from_previous_branch_generation_is_not_reused_even_when_old_head_is_ancestor() {
        let temp = unique_temp_dir("prism-reused-open-branch-pr-test");
        fs::create_dir_all(&temp).unwrap();
        let git = temp.join("git");
        fs::write(
            &git,
            "#!/bin/sh\ncase \"$*\" in *\"remote get-url origin\"*) echo git@github.com:owner/repo.git ;; *\"merge-base --is-ancestor\"*) exit 0 ;; *) printf 'new-head\\n' ;; esac\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&git).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&git, permissions).unwrap();

        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let mut config = test_config();
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        let old_summary = test_summary("feature", "old-head", 0);
        let mut old_cache = PrCache::observed(old_summary.clone(), None);
        old_cache.record_summary_observation(Some(old_summary.clone()), "old".to_string());
        save_pr_cache(&repo, "feature", &old_cache).unwrap();

        let loaded = load_pr_cache_for_branch(&repo, &config, "feature", &temp);

        assert_eq!(loaded.summary(), Some(&old_summary));
        assert!(loaded.trusted_summary().is_err());

        let mut sessions = vec![test_session("feature", PrCache::default())];
        sessions[0].path = temp.clone();
        let poll_started_at = Instant::now();
        for session in &mut sessions {
            session.pr.begin_summary_poll(poll_started_at);
        }
        refresh_pr_summary_index_for_sessions(
            &[PrCacheRepository {
                repo: &repo,
                config: &config,
            }],
            &mut sessions,
            0,
            vec![old_summary],
            poll_started_at,
        );
        assert!(sessions[0].pr.summary().is_none());

        let _ = fs::remove_dir_all(repo.prism_dir());
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn known_open_pr_is_preserved_while_local_repair_is_unpushed() {
        let temp = unique_temp_dir("prism-known-open-pr-local-divergence-test");
        fs::create_dir_all(&temp).unwrap();
        let git = temp.join("git");
        fs::write(&git, "#!/bin/sh\nprintf 'local-repair-head\\n'\n").unwrap();
        let mut permissions = fs::metadata(&git).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&git, permissions).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let mut config = test_config();
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        let summary = test_summary("feature", "remote-pr-head", 0);
        let mut sessions = vec![test_session(
            "feature",
            PrCache::observed(summary.clone(), None),
        )];
        sessions[0].path = temp.clone();
        let poll_started_at = Instant::now();
        sessions[0].pr.begin_summary_poll(poll_started_at);

        refresh_pr_summary_index_for_sessions(
            &[PrCacheRepository {
                repo: &repo,
                config: &config,
            }],
            &mut sessions,
            0,
            vec![summary.clone()],
            poll_started_at,
        );

        assert_eq!(sessions[0].pr.summary(), Some(&summary));
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
    fn incomplete_graphql_summary_index_is_an_observation_failure() {
        let raw = r#"{"data":{"repository":{}}}"#;

        assert!(try_parse_pr_summary_index(raw).is_err());
    }

    #[test]
    fn parses_repo_policy_for_default_branch_rule() {
        let raw = r#"{
          "data": {
            "repository": {
              "defaultBranchRef": {"name": "main"},
              "branchProtectionRules": {
                "nodes": [
                  {
                    "pattern": "release/*",
                    "requiredApprovingReviewCount": 2,
                    "requiresConversationResolution": false,
                    "requiresStrictStatusChecks": false,
                    "requiredStatusCheckContexts": ["release"]
                  },
                  {
                    "pattern": "main",
                    "requiredApprovingReviewCount": 1,
                    "requiresConversationResolution": true,
                    "requiresStrictStatusChecks": true,
                    "requiredStatusCheckContexts": ["ci", "ci", " lint ", ""]
                  }
                ]
              }
            }
          }
        }"#;

        let policy = parse_repo_policy("owner/repo", raw).unwrap();

        assert_eq!(policy.repo_remote, "owner/repo");
        assert_eq!(policy.default_branch.as_deref(), Some("main"));
        assert_eq!(policy.required_approvals, 1);
        assert!(policy.require_conversation_resolution);
        assert!(policy.require_branch_up_to_date);
        assert_eq!(policy.required_checks, vec!["ci", "lint"]);
        assert!(!policy.merge_queue_required);
        assert!(policy.error.is_none());
    }

    #[test]
    fn repo_policy_cache_round_trips_success_and_error() {
        let temp = unique_temp_dir("prism-repo-policy-cache-test");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let policy = RepoPolicyCache {
            repo_remote: "owner/repo".to_string(),
            default_branch: Some("main".to_string()),
            required_approvals: 1,
            require_conversation_resolution: true,
            require_branch_up_to_date: true,
            required_checks: vec!["ci".to_string(), "lint".to_string()],
            merge_queue_required: false,
            refreshed_unix_ms: 123,
            error: None,
        };

        save_repo_policy_cache(&repo, &policy).unwrap();
        let loaded = load_repo_policy_cache(&repo, "owner/repo").unwrap();

        assert_eq!(loaded, policy);

        let error_policy = RepoPolicyCache {
            repo_remote: "owner/repo".to_string(),
            refreshed_unix_ms: 456,
            error: Some("gh auth failed".to_string()),
            ..RepoPolicyCache::default()
        };
        save_repo_policy_cache(&repo, &error_policy).unwrap();
        assert_eq!(
            load_repo_policy_cache(&repo, "owner/repo"),
            Some(error_policy)
        );

        let _ = fs::remove_dir_all(repo.prism_dir());
        let _ = fs::remove_dir_all(temp);
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
                      "id": "PRRT_kw123",
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
                      "id": "PRRT_kw456",
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
        assert_eq!(comments[0].thread_id, "PRRT_kw123");
        assert_eq!(comments[0].id, "PRRC_kw123");
        assert_eq!(comments[0].path, "src/main.rs");
        assert_eq!(comments[0].line, "12");
        assert!(comments[0].resolved);
        assert_eq!(comments[1].author, "maintainer");
        assert_eq!(comments[1].thread_id, "PRRT_kw456");
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
        crate::test_support::test_config()
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
            incarnation: String::new(),
            path_display: format!("/tmp/{branch}"),
            branch: branch.to_string(),
            prompt_summary: String::new(),
            classification: crate::session::SessionClassification::Work,
            visibility: 0,
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
