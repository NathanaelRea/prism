#[cfg(test)]
use std::collections::BTreeSet;
use std::process::Command;
use std::time::{Duration, Instant};

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::config::MergeMethod;
use crate::git::current_head_sha;
use crate::observability;
use crate::process::{
    ProcessDescriptor, ProcessPolicy, run_capture_named, run_output_allow_failure_named,
};
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
    change_request_identity: Option<crate::remote::CanonicalChangeRequestIdentity>,
}

struct PersistedPrDetails {
    details: PrDetails,
    association: Option<PrDetailsAssociation>,
    errors: Vec<String>,
    warnings: Vec<String>,
}

impl PrDetailsAssociation {
    pub(super) fn from_summary(summary: &PrSummary) -> Self {
        Self {
            pr_number: summary.number,
            head_sha: summary.head_sha.clone(),
            change_request_identity: summary.change_request_identity.clone(),
        }
    }

    fn matches(&self, summary: &PrSummary) -> bool {
        self.pr_number == summary.number
            && self.head_sha == summary.head_sha
            && self.change_request_identity == summary.change_request_identity
    }
}

pub(super) struct ProviderDetailsObservation {
    pub comments: Result<Vec<PrComment>, String>,
    pub reviews: Result<Vec<PrReview>, String>,
    pub review_comments: Result<Vec<PrReviewComment>, String>,
    pub files: Result<Vec<String>, String>,
    pub failing_checks: Result<Vec<String>, String>,
    pub check_contexts: Result<Vec<PrCheckContext>, String>,
    pub ci_failures: Result<Vec<CiFailure>, String>,
    pub partial_errors: Vec<String>,
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
    details_warnings: Vec<String>,
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

    pub(crate) fn require_reconciliation(&mut self, reason: &str) {
        if self.summary.is_some() {
            self.summary_quality = PrObservationQuality::PreservedStale;
        }
        if self.details.is_some() {
            self.details_quality = PrObservationQuality::PreservedStale;
        }
        self.summary_error = Some(reason.to_string());
        self.rebuild_error();
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
            .chain(self.details_warnings.iter())
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

    pub(crate) fn record_remote_unavailable(&mut self, error: String) -> bool {
        let before = pr_cache_render_signature(self);
        self.record_summary_failure(error);
        before != pr_cache_render_signature(self)
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
                    self.details_warnings.clear();
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
                self.details_warnings.clear();
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
        let mut errors = observation.partial_errors;
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
        let mut warnings = Vec::new();
        match observation.ci_failures {
            Ok(value) => details.ci_failures = value,
            Err(error) => {
                warnings.push(format!("CI logs unavailable: {error}"));
            }
        }

        self.details = Some(details);
        self.details_association = Some(observation.association);
        self.details_quality = if errors.is_empty() {
            PrObservationQuality::Fresh
        } else {
            PrObservationQuality::PreservedStale
        };
        self.details_errors = errors;
        self.details_warnings = warnings;
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

    pub(crate) fn structurally_eligible(branch: &str, config: &Config, hidden: bool) -> bool {
        !hidden && branch != "(detached)" && !config.is_default_branch(branch)
    }

    pub(crate) fn enforce_structural_eligibility(
        &mut self,
        branch: &str,
        config: &Config,
        hidden: bool,
    ) -> bool {
        if Self::structurally_eligible(branch, config, hidden) {
            return false;
        }
        self.clear_if_present()
    }

    pub(crate) fn clear_for_missing_github_remote(&mut self) -> bool {
        self.clear_if_present()
    }

    fn clear_if_present(&mut self) -> bool {
        let changed = self.summary.is_some() || self.details.is_some() || self.error.is_some();
        if changed {
            let started_at = Instant::now();
            self.begin_summary_poll(started_at);
            self.finish_summary_poll(started_at);
            self.record_summary_observation(None, timestamp_label());
        }
        changed
    }

    pub(crate) fn record_background_persistence_result(
        &mut self,
        details: bool,
        result: Result<(), String>,
    ) {
        match result {
            Ok(()) => {
                self.persistence_error = None;
                self.details_persistence_error = None;
            }
            Err(error) if details => self.details_persistence_error = Some(error),
            Err(error) => self.persistence_error = Some(error),
        }
        self.rebuild_error();
    }
}

pub(super) fn record_provider_summary_refresh(
    repo: &Repository,
    branch: &str,
    cache: &mut PrCache,
    observation: Result<Option<PrSummary>, String>,
) -> Result<(), String> {
    let started_at = Instant::now();
    cache.begin_summary_poll(started_at);
    match observation {
        Ok(summary) => {
            cache.finish_summary_poll(started_at);
            let mutation = cache.record_summary_observation(summary, timestamp_label());
            persist_pr_summary_mutation(repo, branch, cache, mutation);
        }
        Err(error) => {
            cache.finish_summary_poll(started_at);
            cache.record_summary_failure(error);
            persist_observation_errors(repo, branch, cache);
        }
    }
    cache.refresh_result()
}

pub(super) fn record_provider_details_refresh(
    cache: &mut PrCache,
    observation: Result<ProviderDetailsObservation, String>,
) {
    let Some(summary) = cache.summary.clone() else {
        cache.details = None;
        cache.details_association = None;
        cache.details_quality = PrObservationQuality::Unknown;
        return;
    };
    match observation {
        Ok(observation) => {
            cache.record_details_observation(PrDetailsObservation {
                association: PrDetailsAssociation::from_summary(&summary),
                comments: observation.comments,
                reviews: observation.reviews,
                review_comments: observation.review_comments,
                files: observation.files,
                failing_checks: observation.failing_checks,
                check_contexts: observation.check_contexts,
                ci_failures: observation.ci_failures,
                partial_errors: observation.partial_errors,
            });
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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RepoPolicyCache {
    pub repo_remote: String,
    pub provider: Option<crate::remote::ProviderKind>,
    pub canonical_host: Option<String>,
    pub project_path: Option<String>,
    pub target_branch: Option<String>,
    pub identity_complete: bool,
    pub default_branch: Option<String>,
    pub required_approvals: u64,
    pub require_conversation_resolution: bool,
    pub require_branch_up_to_date: bool,
    pub required_checks: Vec<String>,
    pub merge_queue_required: bool,
    pub refreshed_unix_ms: u64,
    pub error: Option<String>,
}

#[cfg(test)]
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
    pub(crate) change_request_identity: Option<crate::remote::CanonicalChangeRequestIdentity>,
    pub(crate) native_state_evidence: crate::remote::NativeStateEvidence,
    pub title: String,
    pub author: String,
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
    pub queue_state: String,
    pub comment_count: u64,
    pub merged: bool,
    pub draft: bool,
}

impl PrSummary {
    pub fn signature(&self) -> String {
        format!(
            "{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
            self.number,
            self.author,
            self.state,
            self.review_decision,
            self.requested_reviewers.join(","),
            self.body,
            self.head_sha,
            self.updated_at,
            self.check_status,
            self.merge_state_status,
            self.queue_state,
            self.comment_count
        )
    }

    pub fn check_state(&self) -> PrCheckState {
        PrCheckState::from_label(&self.check_status)
    }

    pub(crate) fn provider_noun(&self) -> &'static str {
        match self
            .change_request_identity
            .as_ref()
            .map(crate::remote::CanonicalChangeRequestIdentity::provider)
        {
            Some(crate::remote::ProviderKind::GitLab) => "MR",
            _ => "PR",
        }
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
    partial_errors: Vec<String>,
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
    #[serde(default)]
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
    #[serde(default, rename = "submittedAt", alias = "submitted_at")]
    submitted_at: String,
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

pub fn load_pr_cache(repo: &Repository, branch: &str) -> PrCache {
    let loaded = observability::with_writable_db(repo, |conn| {
        conn.query_row(
            "select
                number, title, author, body, url, state, review_decision, requested_reviewers,
                head_ref, base_ref, head_sha, updated_at, check_status, merge_state_status,
                queue_state, comment_count, merged, draft, last_refreshed, observation_error,
                provider, canonical_host, project_path, native_cr_id,
                source_provider, source_canonical_host, source_project_path,
                target_provider, target_canonical_host, target_project_path, identity_complete,
                native_state_evidence
              from pr_cache
              where branch = ?1",
            params![branch],
            |row| {
                Ok((
                    PrSummary {
                        number: row_u64(row, 0)?,
                        change_request_identity: row_change_request_identity(row, 20)?,
                        native_state_evidence: decode_native_state_evidence(
                            &row.get::<_, String>(31)?,
                        ),
                        title: row.get(1)?,
                        author: row.get(2)?,
                        body: row.get(3)?,
                        url: row.get(4)?,
                        state: row.get(5)?,
                        review_decision: row.get(6)?,
                        requested_reviewers: decode_requested_reviewers(&row.get::<_, String>(7)?),
                        head_ref: row.get(8)?,
                        base_ref: row.get(9)?,
                        head_sha: row.get(10)?,
                        updated_at: row.get(11)?,
                        check_status: row.get(12)?,
                        merge_state_status: row.get(13)?,
                        queue_state: row.get(14)?,
                        comment_count: row_u64(row, 15)?,
                        merged: row.get(16)?,
                        draft: row.get(17)?,
                    },
                    row.get::<_, String>(18)?,
                    row.get::<_, Option<String>>(19)?,
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
    let (details, details_association, details_errors, details_warnings) =
        match load_pr_details_cache_record(repo, branch) {
            Ok(Some(record)) => (
                Some(record.details),
                record.association,
                record.errors,
                record.warnings,
            ),
            Ok(None) => (None, None, Vec::new(), Vec::new()),
            Err(error) => (None, None, vec![error], Vec::new()),
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
        details_warnings,
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
    let result = fetch_pr_summary(path, branch, config).map(|observation| {
        let origin_push =
            super::discover_git_remote(path, config, "origin", super::RemoteUrlKind::Push)
                .ok()
                .map(|remote| remote.repository.id);
        let local_head = current_head_sha(path, config).ok();
        observation.filter(|(summary, _)| {
            pr_summary_matches_worktree(
                summary,
                branch,
                cache.summary.as_ref(),
                origin_push.as_ref(),
                local_head.as_deref(),
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
        save_pr_details_cache_for_association(
            repo,
            branch,
            details,
            &association,
            &cache.details_errors,
            &cache.details_warnings,
        )
    } else if !cache.details_errors.is_empty() || !cache.details_warnings.is_empty() {
        save_pr_details_cache_for_association(
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
    if !PrCacheEligibility::for_worktree(branch, path, config).can_observe() {
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
    let persistence = persist_pr_cache_snapshot(repo, branch, cache);
    cache.details_persistence_error = persistence.err();
    cache.rebuild_error();
    true
}

pub(crate) fn apply_pr_details_poll_result(cache: &mut PrCache, poll_result: PrCache) -> bool {
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
    cache.details_warnings = poll_result.details_warnings;
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

#[cfg(test)]
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
        let summary = resolve_pr_summary_for_session(session, managed.config, &summaries);
        let mutation = session
            .pr
            .record_summary_observation(summary, refreshed.clone());
        persist_pr_summary_mutation(managed.repo, &session.branch, &mut session.pr, mutation);
    }
}

fn pr_summary_matches_worktree(
    summary: &PrSummary,
    branch: &str,
    known_summary: Option<&PrSummary>,
    origin_push: Option<&crate::remote::RemoteRepositoryId>,
    local_head: Option<&str>,
) -> bool {
    let cached_canonical_association = known_summary.is_some_and(|known| {
        known.change_request_identity.is_some()
            && known.change_request_identity == summary.change_request_identity
    });
    let initial_canonical_association = summary.head_ref == branch
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

pub(crate) fn resolve_pr_summary_for_session(
    session: &Session,
    config: &Config,
    summaries: &[PrSummary],
) -> Option<PrSummary> {
    if !PrCacheEligibility::for_successful_index(session, config).can_observe() {
        return None;
    }
    let origin_push =
        super::discover_git_remote(&session.path, config, "origin", super::RemoteUrlKind::Push)
            .ok()
            .map(|remote| remote.repository.id);
    let local_head = current_head_sha(&session.path, config).ok();
    summaries
        .iter()
        .find(|summary| {
            pr_summary_matches_worktree(
                summary,
                &session.branch,
                session.pr.summary.as_ref(),
                origin_push.as_ref(),
                local_head.as_deref(),
            )
        })
        .cloned()
}

pub(crate) fn apply_pr_summary_poll_result(
    cache: &mut PrCache,
    poll_started_at: Instant,
    observation: Result<Option<PrSummary>, String>,
    refreshed: &str,
) -> bool {
    if !cache.finish_summary_poll(poll_started_at) {
        return false;
    }
    match observation {
        Ok(summary) => {
            cache.record_summary_observation(summary, refreshed.to_string());
        }
        Err(error) => cache.record_summary_failure(error),
    }
    true
}

pub fn pr_details_due(cache: &PrCache) -> bool {
    if cache.summary.is_none() {
        return false;
    }
    cache
        .details_last_polled
        .map(|last| last.elapsed() >= PR_DETAIL_POLL_INTERVAL)
        .unwrap_or(true)
}

pub(crate) fn pr_cache_pollable_for_session(session: &Session, config: &Config) -> bool {
    PrCache::structurally_eligible(&session.branch, config, session.hidden)
        && !session
            .pr
            .summary
            .as_ref()
            .is_some_and(|summary| summary.merged)
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
    crate::remote::discover_git_remote(path, config, "origin", crate::remote::RemoteUrlKind::Fetch)
        .is_ok()
}

pub(crate) fn persist_pr_cache_snapshot(
    repo: &Repository,
    branch: &str,
    cache: &PrCache,
) -> Result<(), String> {
    if cache.summary.is_none() {
        return remove_pr_cache(repo, branch);
    }
    save_pr_cache(repo, branch, cache)?;
    match (&cache.details, &cache.details_association) {
        (Some(details), Some(association)) => save_pr_details_cache_for_association(
            repo,
            branch,
            details,
            association,
            &cache.details_errors,
            &cache.details_warnings,
        ),
        (None, Some(association))
            if !cache.details_errors.is_empty() || !cache.details_warnings.is_empty() =>
        {
            save_pr_details_cache_for_association(
                repo,
                branch,
                &PrDetails::default(),
                association,
                &cache.details_errors,
                &cache.details_warnings,
            )
        }
        _ => remove_pr_details_cache(repo, branch),
    }
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
                    &cache.details_warnings,
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
                load_repo_policy_cache_for_identity(repo, repository, target_branch)
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
    save_repo_policy_cache(repo, &policy)?;
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

pub(crate) fn load_repo_policy_cache(
    repo: &Repository,
    repo_remote: &str,
) -> Option<RepoPolicyCache> {
    observability::with_writable_db(repo, |conn| {
        conn.query_row(
            "select repo_remote, default_branch, required_approvals,
                    require_conversation_resolution, require_branch_up_to_date,
                    required_checks, merge_queue_required, refreshed_unix_ms, error,
                    provider, canonical_host, project_path, target_branch, identity_complete
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
                    provider: row
                        .get::<_, Option<String>>(9)?
                        .as_deref()
                        .and_then(crate::remote::ProviderKind::parse),
                    canonical_host: row.get(10)?,
                    project_path: row.get(11)?,
                    target_branch: row.get(12)?,
                    identity_complete: row.get::<_, i64>(13)? != 0,
                })
            },
        )
        .optional()
        .map_err(|error| format!("read repo policy cache: {error}"))
    })
    .ok()
    .flatten()
}

pub(crate) fn load_repo_policy_cache_for_repository(
    repo: &Repository,
    repository: &crate::remote::RemoteRepositoryId,
) -> Option<RepoPolicyCache> {
    let latest = observability::with_writable_db(repo, |conn| {
        conn.query_row(
            "select repo_remote, default_branch, required_approvals,
                    require_conversation_resolution, require_branch_up_to_date,
                    required_checks, merge_queue_required, refreshed_unix_ms, error,
                    provider, canonical_host, project_path, target_branch
               from repo_policy_cache_v2
               where provider = ?1 and canonical_host = ?2
                 and project_path_key = ?3
              order by refreshed_unix_ms desc
              limit 1",
            params![
                repository.provider().config_label(),
                repository.host().to_string(),
                repo_policy_project_path_key(repository.provider(), repository.project_path()),
            ],
            repo_policy_from_v2_row,
        )
        .optional()
        .map_err(|error| format!("read identity-keyed repo policy cache: {error}"))
    })
    .ok()
    .flatten();
    if latest.is_some() {
        return latest;
    }
    let policy = load_repo_policy_cache(repo, repository.project_path())?;
    let expected_host = repository.host().to_string();
    (policy.identity_complete
        && policy.provider == Some(repository.provider())
        && policy.canonical_host.as_deref() == Some(expected_host.as_str())
        && policy
            .project_path
            .as_deref()
            .is_some_and(|path| repository.project_path_eq(path))
        && policy.target_branch.is_some()
        && policy.target_branch == policy.default_branch)
        .then_some(policy)
}

pub(crate) fn load_repo_policy_cache_for_identity(
    repo: &Repository,
    repository: &crate::remote::RemoteRepositoryId,
    target_branch: &str,
) -> Option<RepoPolicyCache> {
    observability::with_writable_db(repo, |conn| {
        conn.query_row(
            "select repo_remote, default_branch, required_approvals,
                    require_conversation_resolution, require_branch_up_to_date,
                    required_checks, merge_queue_required, refreshed_unix_ms, error,
                    provider, canonical_host, project_path, target_branch
               from repo_policy_cache_v2
               where provider = ?1 and canonical_host = ?2
                 and project_path_key = ?3
                and target_branch = ?4",
            params![
                repository.provider().config_label(),
                repository.host().to_string(),
                repo_policy_project_path_key(repository.provider(), repository.project_path()),
                target_branch,
            ],
            repo_policy_from_v2_row,
        )
        .optional()
        .map_err(|error| format!("read identity-keyed repo policy cache: {error}"))
    })
    .ok()
    .flatten()
    .or_else(|| {
        let policy = load_repo_policy_cache(repo, repository.project_path())?;
        let expected_host = repository.host().to_string();
        (policy.identity_complete
            && policy.provider == Some(repository.provider())
            && policy.canonical_host.as_deref() == Some(expected_host.as_str())
            && policy
                .project_path
                .as_deref()
                .is_some_and(|path| repository.project_path_eq(path))
            && policy.target_branch.as_deref() == Some(target_branch))
        .then_some(policy)
    })
}

fn repo_policy_from_v2_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RepoPolicyCache> {
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
        provider: row
            .get::<_, Option<String>>(9)?
            .as_deref()
            .and_then(crate::remote::ProviderKind::parse),
        canonical_host: row.get(10)?,
        project_path: row.get(11)?,
        target_branch: row.get(12)?,
        identity_complete: true,
    })
}

fn repo_policy_project_path_key(
    provider: crate::remote::ProviderKind,
    project_path: &str,
) -> String {
    match provider {
        crate::remote::ProviderKind::GitHub => project_path.to_ascii_lowercase(),
        crate::remote::ProviderKind::GitLab | crate::remote::ProviderKind::Forgejo => {
            project_path.to_string()
        }
    }
}

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

pub(crate) fn migrate_pr_cache_schema(conn: &rusqlite::Connection) -> Result<(), String> {
    conn.execute_batch(
        "
        create table if not exists pr_cache (
          branch text primary key,
          number integer not null,
          provider text,
          canonical_host text,
          project_path text,
          native_cr_id text,
          display_number integer,
          source_provider text,
          source_canonical_host text,
          source_project_path text,
          target_provider text,
          target_canonical_host text,
          target_project_path text,
          identity_complete integer not null default 0,
          title text not null,
          author text not null default '',
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
          queue_state text not null default '',
          comment_count integer not null default 0,
          merged integer not null,
          draft integer not null,
          last_refreshed text not null,
          refreshed_unix_ms integer not null,
          observation_error text,
          native_state_evidence text not null default '{}'
        );

        create table if not exists pr_details_cache (
          branch text primary key,
          pr_number integer,
          head_sha text,
          provider text,
          canonical_host text,
          project_path text,
          native_cr_id text,
          display_number integer,
          source_provider text,
          source_canonical_host text,
          source_project_path text,
          target_provider text,
          target_canonical_host text,
          target_project_path text,
          identity_complete integer not null default 0,
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
          provider text,
          canonical_host text,
          project_path text,
          target_branch text,
          identity_complete integer not null default 0,
          default_branch text,
          required_approvals integer not null default 0,
          require_conversation_resolution integer not null default 0,
          require_branch_up_to_date integer not null default 0,
          required_checks text not null default '[]',
          merge_queue_required integer not null default 0,
          refreshed_unix_ms integer not null,
          error text
        );

        create table if not exists repo_policy_cache_v2 (
          provider text not null,
          canonical_host text not null,
          project_path text not null,
          project_path_key text not null default '',
          target_branch text not null,
          repo_remote text not null,
          default_branch text,
          required_approvals integer not null default 0,
          require_conversation_resolution integer not null default 0,
          require_branch_up_to_date integer not null default 0,
          required_checks text not null default '[]',
          merge_queue_required integer not null default 0,
          refreshed_unix_ms integer not null,
          error text,
          primary key (provider, canonical_host, project_path, target_branch)
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
    if !table_has_column(conn, "pr_cache", "author")? {
        conn.execute(
            "alter table pr_cache add column author text not null default ''",
            [],
        )
        .map_err(|error| format!("migrate pr_cache author column: {error}"))?;
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
    if !table_has_column(conn, "pr_cache", "queue_state")? {
        conn.execute(
            "alter table pr_cache add column queue_state text not null default ''",
            [],
        )
        .map_err(|error| format!("migrate pr_cache queue_state column: {error}"))?;
    }
    if !table_has_column(conn, "pr_cache", "requested_reviewers")? {
        conn.execute(
            "alter table pr_cache add column requested_reviewers text not null default ''",
            [],
        )
        .map_err(|error| format!("migrate pr_cache requested_reviewers column: {error}"))?;
    }
    if !table_has_column(conn, "pr_cache", "native_state_evidence")? {
        conn.execute(
            "alter table pr_cache add column native_state_evidence text not null default '{}'",
            [],
        )
        .map_err(|error| format!("migrate pr_cache native_state_evidence column: {error}"))?;
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
    for (table, column, definition) in [
        ("pr_cache", "provider", "text"),
        ("pr_cache", "canonical_host", "text"),
        ("pr_cache", "project_path", "text"),
        ("pr_cache", "native_cr_id", "text"),
        ("pr_cache", "display_number", "integer"),
        ("pr_cache", "source_provider", "text"),
        ("pr_cache", "source_canonical_host", "text"),
        ("pr_cache", "source_project_path", "text"),
        ("pr_cache", "target_provider", "text"),
        ("pr_cache", "target_canonical_host", "text"),
        ("pr_cache", "target_project_path", "text"),
        (
            "pr_cache",
            "identity_complete",
            "integer not null default 0",
        ),
        ("pr_details_cache", "provider", "text"),
        ("pr_details_cache", "canonical_host", "text"),
        ("pr_details_cache", "project_path", "text"),
        ("pr_details_cache", "native_cr_id", "text"),
        ("pr_details_cache", "display_number", "integer"),
        ("pr_details_cache", "source_provider", "text"),
        ("pr_details_cache", "source_canonical_host", "text"),
        ("pr_details_cache", "source_project_path", "text"),
        ("pr_details_cache", "target_provider", "text"),
        ("pr_details_cache", "target_canonical_host", "text"),
        ("pr_details_cache", "target_project_path", "text"),
        (
            "pr_details_cache",
            "identity_complete",
            "integer not null default 0",
        ),
        ("repo_policy_cache", "provider", "text"),
        ("repo_policy_cache", "canonical_host", "text"),
        ("repo_policy_cache", "project_path", "text"),
        ("repo_policy_cache", "target_branch", "text"),
        (
            "repo_policy_cache",
            "identity_complete",
            "integer not null default 0",
        ),
    ] {
        if !table_has_column(conn, table, column)? {
            conn.execute(
                &format!("alter table {table} add column {column} {definition}"),
                [],
            )
            .map_err(|error| format!("migrate {table} {column} column: {error}"))?;
        }
    }
    if !table_has_column(conn, "repo_policy_cache_v2", "project_path_key")? {
        conn.execute(
            "alter table repo_policy_cache_v2 add column project_path_key text not null default ''",
            [],
        )
        .map_err(|error| format!("migrate repository policy project path key: {error}"))?;
    }
    conn.execute(
        "update repo_policy_cache_v2
            set project_path_key = case when provider = 'github' then lower(project_path) else project_path end
          where project_path_key = '' or project_path_key != case when provider = 'github' then lower(project_path) else project_path end",
        [],
    )
    .map_err(|error| format!("normalize repository policy project path keys: {error}"))?;
    conn.execute(
        "delete from repo_policy_cache_v2
          where rowid in (
            select rowid from (
              select rowid,
                     row_number() over (
                       partition by provider, canonical_host, project_path_key, target_branch
                       order by refreshed_unix_ms desc, rowid desc
                     ) as duplicate_rank
                from repo_policy_cache_v2
            ) where duplicate_rank > 1
          )",
        [],
    )
    .map_err(|error| format!("deduplicate repository policy project path keys: {error}"))?;
    conn.execute(
        "create unique index if not exists repo_policy_cache_v2_identity_key
             on repo_policy_cache_v2(provider, canonical_host, project_path_key, target_branch)",
        [],
    )
    .map_err(|error| format!("index repository policy project path keys: {error}"))?;
    backfill_legacy_remote_identity(conn)?;
    Ok(())
}

fn backfill_legacy_remote_identity(conn: &rusqlite::Connection) -> Result<(), String> {
    let rows = {
        let mut statement = conn
            .prepare("select branch, url, number from pr_cache where provider is null")
            .map_err(|error| format!("prepare legacy PR identity backfill: {error}"))?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .map_err(|error| format!("read legacy PR identities: {error}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("read legacy PR identity: {error}"))?
    };
    for (branch, url, number) in rows {
        let Some(project_path) = github_project_path_from_pr_url(&url) else {
            continue;
        };
        conn.execute(
            "update pr_cache
                set provider = 'github', canonical_host = 'github.com', project_path = ?2,
                    display_number = ?3, target_provider = 'github',
                    target_canonical_host = 'github.com', target_project_path = ?2
              where branch = ?1 and provider is null",
            params![branch, project_path, number],
        )
        .map_err(|error| format!("backfill legacy PR identity: {error}"))?;
    }
    conn.execute(
        "update pr_details_cache
            set pr_number = coalesce(pr_number, (select number from pr_cache where pr_cache.branch = pr_details_cache.branch)),
                provider = (select provider from pr_cache where pr_cache.branch = pr_details_cache.branch),
                canonical_host = (select canonical_host from pr_cache where pr_cache.branch = pr_details_cache.branch),
                project_path = (select project_path from pr_cache where pr_cache.branch = pr_details_cache.branch),
                display_number = coalesce(display_number, pr_number, (select number from pr_cache where pr_cache.branch = pr_details_cache.branch)),
                target_provider = (select target_provider from pr_cache where pr_cache.branch = pr_details_cache.branch),
                target_canonical_host = (select target_canonical_host from pr_cache where pr_cache.branch = pr_details_cache.branch),
                target_project_path = (select target_project_path from pr_cache where pr_cache.branch = pr_details_cache.branch)
          where provider is null
            and exists (select 1 from pr_cache where pr_cache.branch = pr_details_cache.branch and pr_cache.provider is not null)",
        [],
    )
    .map_err(|error| format!("backfill legacy PR details identity: {error}"))?;
    conn.execute(
        "update repo_policy_cache
            set provider = 'github', canonical_host = 'github.com', project_path = repo_remote,
                target_branch = default_branch,
                identity_complete = case when default_branch is not null and default_branch != '' then 1 else 0 end
          where provider is null and instr(repo_remote, '/') > 1",
        [],
    )
    .map_err(|error| format!("backfill legacy repository policy identity: {error}"))?;
    Ok(())
}

fn github_project_path_from_pr_url(url: &str) -> Option<String> {
    let remainder = url.strip_prefix("https://github.com/")?;
    let (project_path, number) = remainder.rsplit_once("/pull/")?;
    if project_path.split('/').count() != 2 || number.parse::<u64>().is_err() {
        return None;
    }
    crate::remote::RemoteRepositoryId::new(
        crate::remote::ProviderKind::GitHub,
        crate::remote::HostIdentity::new("github.com", None).ok()?,
        project_path,
    )
    .ok()
    .map(|repository| repository.project_path().to_string())
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
                    check_contexts, pr_number, head_sha, observation_error,
                    provider, canonical_host, project_path, native_cr_id,
                    source_provider, source_canonical_host, source_project_path,
                    target_provider, target_canonical_host, target_project_path, identity_complete
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
                            change_request_identity: row_change_request_identity(row, 10)?,
                        })
                    }
                    _ => None,
                };
                let observation_messages = row
                    .get::<_, Option<String>>(9)?
                    .filter(|error| !error.is_empty())
                    .unwrap_or_default();
                let mut errors = Vec::new();
                let mut warnings = Vec::new();
                for message in observation_messages.lines() {
                    if let Some(warning) = message.strip_prefix("warning:") {
                        warnings.push(warning.to_string());
                    } else if !message.is_empty() {
                        errors.push(message.to_string());
                    }
                }
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
                    warnings,
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
            "select number, head_sha, provider, canonical_host, project_path, native_cr_id,
                    source_provider, source_canonical_host, source_project_path,
                    target_provider, target_canonical_host, target_project_path, identity_complete
               from pr_cache where branch = ?1",
            params![branch],
            |row| {
                Ok(PrDetailsAssociation {
                    pr_number: row_u64(row, 0)?,
                    head_sha: row.get(1)?,
                    change_request_identity: row_change_request_identity(row, 2)?,
                })
            },
        )
        .map_err(|error| format!("read PR summary association: {error}"))
    })?;
    save_pr_details_cache_for_association(repo, branch, details, &association, &[], &[])
}

fn save_pr_details_cache_for_association(
    repo: &Repository,
    branch: &str,
    details: &PrDetails,
    association: &PrDetailsAssociation,
    errors: &[String],
    warnings: &[String],
) -> Result<(), String> {
    observability::with_writable_db(repo, |conn| {
        let identity = association.change_request_identity.as_ref();
        conn.execute(
            "insert into pr_details_cache (
                branch, pr_number, head_sha, provider, canonical_host, project_path, native_cr_id,
                display_number, source_provider, source_canonical_host, source_project_path,
                target_provider, target_canonical_host, target_project_path, identity_complete,
                comments, reviews, review_comments, files, failing_checks, ci_failures,
                check_contexts, refreshed_unix_ms, observation_error
             ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24)
              on conflict(branch) do update set
                pr_number = excluded.pr_number,
                head_sha = excluded.head_sha,
                provider = excluded.provider,
                canonical_host = excluded.canonical_host,
                project_path = excluded.project_path,
                native_cr_id = excluded.native_cr_id,
                display_number = excluded.display_number,
                source_provider = excluded.source_provider,
                source_canonical_host = excluded.source_canonical_host,
                source_project_path = excluded.source_project_path,
                target_provider = excluded.target_provider,
                target_canonical_host = excluded.target_canonical_host,
                target_project_path = excluded.target_project_path,
                identity_complete = excluded.identity_complete,
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
                identity.map(|identity| identity.provider().config_label()),
                identity.map(|identity| identity.canonical_host()),
                identity.map(|identity| identity.project_path()),
                identity.map(|identity| identity.native_id()),
                sqlite_i64(association.pr_number, "PR display number")?,
                identity.map(|identity| identity.source_provider().config_label()),
                identity.map(|identity| identity.source_canonical_host()),
                identity.map(|identity| identity.source_project_path()),
                identity.map(|identity| identity.target_provider().config_label()),
                identity.map(|identity| identity.target_canonical_host()),
                identity.map(|identity| identity.target_project_path()),
                identity.is_some(),
                encode_pr_comments(&details.comments),
                encode_pr_reviews(&details.reviews),
                encode_pr_review_comments(&details.review_comments),
                encode_string_values(&details.files),
                encode_string_values(&details.failing_checks),
                encode_ci_failures(&details.ci_failures),
                encode_check_contexts(&details.check_contexts),
                unix_seconds(),
                (!errors.is_empty() || !warnings.is_empty()).then(|| {
                    errors
                        .iter()
                        .cloned()
                        .chain(warnings.iter().map(|warning| format!("warning:{warning}")))
                        .collect::<Vec<_>>()
                        .join("\n")
                }),
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
    let identity = summary.change_request_identity.as_ref();
    observability::with_writable_db(repo, |conn| {
        conn.execute(
            "insert into pr_cache (
                branch, number, provider, canonical_host, project_path, native_cr_id,
                display_number, source_provider, source_canonical_host, source_project_path,
                target_provider, target_canonical_host, target_project_path, identity_complete,
                title, author, body, url, state, review_decision, requested_reviewers,
                head_ref, base_ref, head_sha, updated_at, check_status, merge_state_status,
                queue_state, comment_count, merged, draft, last_refreshed, refreshed_unix_ms,
                observation_error, native_state_evidence
             ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34, ?35)
              on conflict(branch) do update set
                number = excluded.number,
                provider = excluded.provider,
                canonical_host = excluded.canonical_host,
                project_path = excluded.project_path,
                native_cr_id = excluded.native_cr_id,
                display_number = excluded.display_number,
                source_provider = excluded.source_provider,
                source_canonical_host = excluded.source_canonical_host,
                source_project_path = excluded.source_project_path,
                target_provider = excluded.target_provider,
                target_canonical_host = excluded.target_canonical_host,
                target_project_path = excluded.target_project_path,
                identity_complete = excluded.identity_complete,
                title = excluded.title,
                author = excluded.author,
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
                queue_state = excluded.queue_state,
                comment_count = excluded.comment_count,
                merged = excluded.merged,
                draft = excluded.draft,
                last_refreshed = excluded.last_refreshed,
                refreshed_unix_ms = excluded.refreshed_unix_ms,
                observation_error = excluded.observation_error,
                native_state_evidence = excluded.native_state_evidence",
            params![
                branch,
                number,
                identity.map(|identity| identity.provider().config_label()),
                identity.map(|identity| identity.canonical_host()),
                identity.map(|identity| identity.project_path()),
                identity.map(|identity| identity.native_id()),
                number,
                identity.map(|identity| identity.source_provider().config_label()),
                identity.map(|identity| identity.source_canonical_host()),
                identity.map(|identity| identity.source_project_path()),
                identity.map(|identity| identity.target_provider().config_label()),
                identity.map(|identity| identity.target_canonical_host()),
                identity.map(|identity| identity.target_project_path()),
                identity.is_some(),
                summary.title.as_str(),
                summary.author.as_str(),
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
                summary.queue_state.as_str(),
                comment_count,
                summary.merged,
                summary.draft,
                cache.last_refreshed.as_deref().unwrap_or(""),
                unix_seconds(),
                cache.summary_error.as_deref(),
                encode_native_state_evidence(&summary.native_state_evidence),
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
        if policy.identity_complete
            && let (Some(provider), Some(canonical_host), Some(project_path), Some(target_branch)) = (
                policy.provider,
                policy.canonical_host.as_deref(),
                policy.project_path.as_deref(),
                policy.target_branch.as_deref(),
            )
        {
            conn.execute(
                "insert into repo_policy_cache_v2 (
                    provider, canonical_host, project_path, project_path_key, target_branch, repo_remote,
                    default_branch, required_approvals, require_conversation_resolution,
                    require_branch_up_to_date, required_checks, merge_queue_required,
                    refreshed_unix_ms, error
                 ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
                  on conflict(provider, canonical_host, project_path_key, target_branch) do update set
                    project_path = excluded.project_path,
                    repo_remote = excluded.repo_remote,
                    default_branch = excluded.default_branch,
                    required_approvals = excluded.required_approvals,
                    require_conversation_resolution = excluded.require_conversation_resolution,
                    require_branch_up_to_date = excluded.require_branch_up_to_date,
                    required_checks = excluded.required_checks,
                    merge_queue_required = excluded.merge_queue_required,
                    refreshed_unix_ms = excluded.refreshed_unix_ms,
                    error = excluded.error",
                params![
                    provider.config_label(),
                    canonical_host,
                    project_path,
                    repo_policy_project_path_key(provider, project_path),
                    target_branch,
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
            .map_err(|error| format!("write identity-keyed repo policy cache: {error}"))?;
        }
        conn.execute(
            "insert into repo_policy_cache (
                repo_remote, provider, canonical_host, project_path, target_branch,
                identity_complete, default_branch, required_approvals,
                require_conversation_resolution, require_branch_up_to_date,
                required_checks, merge_queue_required, refreshed_unix_ms, error
             ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
              on conflict(repo_remote) do update set
                provider = excluded.provider,
                canonical_host = excluded.canonical_host,
                project_path = excluded.project_path,
                target_branch = excluded.target_branch,
                identity_complete = excluded.identity_complete,
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
                policy.provider.map(|provider| provider.config_label()),
                policy.canonical_host.as_deref(),
                policy.project_path.as_deref(),
                policy.target_branch.as_deref(),
                policy.identity_complete,
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

fn encode_native_state_evidence(evidence: &crate::remote::NativeStateEvidence) -> String {
    serde_json::to_string(evidence).unwrap_or_else(|_| "{}".to_string())
}

fn decode_native_state_evidence(raw: &str) -> crate::remote::NativeStateEvidence {
    serde_json::from_str(raw).unwrap_or_default()
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

fn row_change_request_identity(
    row: &rusqlite::Row<'_>,
    start: usize,
) -> rusqlite::Result<Option<crate::remote::CanonicalChangeRequestIdentity>> {
    if row.get::<_, i64>(start + 10)? == 0 {
        return Ok(None);
    }
    let Some(provider) = row
        .get::<_, Option<String>>(start)?
        .as_deref()
        .and_then(crate::remote::ProviderKind::parse)
    else {
        return Ok(None);
    };
    let Some(source_provider) = row
        .get::<_, Option<String>>(start + 4)?
        .as_deref()
        .and_then(crate::remote::ProviderKind::parse)
    else {
        return Ok(None);
    };
    let Some(target_provider) = row
        .get::<_, Option<String>>(start + 7)?
        .as_deref()
        .and_then(crate::remote::ProviderKind::parse)
    else {
        return Ok(None);
    };
    let values = (
        row.get::<_, Option<String>>(start + 1)?,
        row.get::<_, Option<String>>(start + 2)?,
        row.get::<_, Option<String>>(start + 3)?,
        row.get::<_, Option<String>>(start + 5)?,
        row.get::<_, Option<String>>(start + 6)?,
        row.get::<_, Option<String>>(start + 8)?,
        row.get::<_, Option<String>>(start + 9)?,
    );
    let (
        Some(host),
        Some(project_path),
        Some(native_id),
        Some(source_host),
        Some(source_project_path),
        Some(target_host),
        Some(target_project_path),
    ) = values
    else {
        return Ok(None);
    };
    let Some((repository, native_id, source, target)) = (|| {
        Some((
            crate::remote::RemoteRepositoryId::new(
                provider,
                crate::remote::HostIdentity::parse(&host).ok()?,
                project_path,
            )
            .ok()?,
            crate::remote::NativeChangeRequestId::new(native_id).ok()?,
            crate::remote::RemoteRepositoryId::new(
                source_provider,
                crate::remote::HostIdentity::parse(&source_host).ok()?,
                source_project_path,
            )
            .ok()?,
            crate::remote::RemoteRepositoryId::new(
                target_provider,
                crate::remote::HostIdentity::parse(&target_host).ok()?,
                target_project_path,
            )
            .ok()?,
        ))
    })() else {
        return Ok(None);
    };
    Ok(Some(crate::remote::CanonicalChangeRequestIdentity::new(
        &repository,
        &native_id,
        &source,
        &target,
    )))
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
            create table repo_policy_cache (
              repo_remote text primary key, default_branch text,
              required_approvals integer not null default 0,
              require_conversation_resolution integer not null default 0,
              require_branch_up_to_date integer not null default 0,
              required_checks text not null default '[]',
              merge_queue_required integer not null default 0,
              refreshed_unix_ms integer not null, error text
            );
            insert into pr_cache values (
              'feature', 42, 'Old row', 'https://example.test/42', 'OPEN', '',
              'feature', 'main', 'head-a', '2026-01-01', 'pending', 0, 0,
              'before migration', 123
            );
            insert into pr_details_cache values (
              'feature', '[]', '[]', '[]', '[\"src/lib.rs\"]', '[]', 123
            );
            insert into pr_cache values (
              'github-feature', 43, 'GitHub row', 'https://github.com/acme/widgets/pull/43',
              'OPEN', '', 'github-feature', 'main', 'head-b', '2026-01-02', 'pending',
              0, 0, 'before migration', 124
            );
            insert into pr_details_cache values (
              'github-feature', '[]', '[]', '[]', '[\"src/main.rs\"]', '[]', 124
            );
            insert into repo_policy_cache values (
              'acme/widgets', 'main', 1, 1, 1, '[\"ci\"]', 0, 125, null
            );
            ",
        )
        .unwrap();

        migrate_pr_cache_schema(&conn).unwrap();
        migrate_pr_cache_schema(&conn).unwrap();

        assert!(table_has_column(&conn, "pr_cache", "body").unwrap());
        assert!(table_has_column(&conn, "pr_cache", "observation_error").unwrap());
        assert!(table_has_column(&conn, "pr_details_cache", "pr_number").unwrap());
        assert!(table_has_column(&conn, "pr_details_cache", "head_sha").unwrap());
        assert!(table_has_column(&conn, "pr_cache", "native_cr_id").unwrap());
        assert!(table_has_column(&conn, "pr_cache", "identity_complete").unwrap());
        assert!(table_has_column(&conn, "pr_cache", "native_state_evidence").unwrap());
        assert!(table_has_column(&conn, "pr_details_cache", "target_project_path").unwrap());
        assert!(table_has_column(&conn, "repo_policy_cache", "target_branch").unwrap());
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
        assert_eq!(
            conn.query_row(
                "select native_state_evidence from pr_cache where branch = 'feature'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
            "{}"
        );
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
        let github_identity = conn
            .query_row(
                "select provider, canonical_host, project_path, native_cr_id, display_number,
                        source_project_path, target_project_path, identity_complete
                   from pr_cache where branch = 'github-feature'",
                [],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, i64>(7)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            github_identity,
            (
                Some("github".to_string()),
                Some("github.com".to_string()),
                Some("acme/widgets".to_string()),
                None,
                Some(43),
                None,
                Some("acme/widgets".to_string()),
                0,
            )
        );
        let details_identity = conn
            .query_row(
                "select provider, target_project_path, identity_complete
                   from pr_details_cache where branch = 'github-feature'",
                [],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            details_identity,
            (
                Some("github".to_string()),
                Some("acme/widgets".to_string()),
                0,
            )
        );
        let policy_identity = conn
            .query_row(
                "select provider, canonical_host, project_path, target_branch, identity_complete
                   from repo_policy_cache where repo_remote = 'acme/widgets'",
                [],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            policy_identity,
            (
                Some("github".to_string()),
                Some("github.com".to_string()),
                Some("acme/widgets".to_string()),
                Some("main".to_string()),
                1,
            )
        );
    }

    #[test]
    fn migration_normalizes_and_deduplicates_github_policy_identity_keys_only() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            create table repo_policy_cache_v2 (
              provider text not null,
              canonical_host text not null,
              project_path text not null,
              target_branch text not null,
              repo_remote text not null,
              default_branch text,
              required_approvals integer not null default 0,
              require_conversation_resolution integer not null default 0,
              require_branch_up_to_date integer not null default 0,
              required_checks text not null default '[]',
              merge_queue_required integer not null default 0,
              refreshed_unix_ms integer not null,
              error text,
              primary key (provider, canonical_host, project_path, target_branch)
            );
            insert into repo_policy_cache_v2 values
              ('github', 'github.com', 'acme/widget', 'main', 'acme/widget', 'main', 1, 0, 0, '[]', 0, 10, null),
              ('github', 'github.com', 'Acme/Widget', 'main', 'Acme/Widget', 'main', 2, 0, 0, '[]', 0, 20, null),
              ('gitlab', 'gitlab.com', 'acme/widget', 'main', 'acme/widget', 'main', 3, 0, 0, '[]', 0, 30, null),
              ('gitlab', 'gitlab.com', 'Acme/Widget', 'main', 'Acme/Widget', 'main', 4, 0, 0, '[]', 0, 40, null);
            ",
        )
        .unwrap();

        migrate_pr_cache_schema(&conn).unwrap();
        migrate_pr_cache_schema(&conn).unwrap();

        let github = conn
            .query_row(
                "select count(*), project_path, project_path_key, required_approvals
                   from repo_policy_cache_v2 where provider = 'github'",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            github,
            (1, "Acme/Widget".to_string(), "acme/widget".to_string(), 2)
        );
        assert_eq!(
            conn.query_row(
                "select count(*) from repo_policy_cache_v2 where provider = 'gitlab'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            2
        );
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
        let identity = test_identity(
            crate::remote::ProviderKind::GitHub,
            "github.com",
            "example/repo",
            "PR_equivalent",
        );
        let old_summary = PrSummary {
            change_request_identity: Some(identity.clone()),
            ..test_summary("feature", "head-a", 1)
        };
        let new_summary = PrSummary {
            change_request_identity: Some(identity),
            ..test_summary("feature", "head-a", 2)
        };
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
    fn canonical_change_request_or_head_changes_invalidate_cached_details() {
        let base_summary = PrSummary {
            change_request_identity: Some(test_identity(
                crate::remote::ProviderKind::GitHub,
                "github.com",
                "example/repo",
                "PR_one",
            )),
            ..test_summary("feature", "head-a", 1)
        };
        let details = PrDetails {
            comments: vec![PrComment {
                body: "cached".to_string(),
                ..PrComment::default()
            }],
            ..PrDetails::default()
        };
        let changed_identities = [
            test_identity(
                crate::remote::ProviderKind::GitLab,
                "github.com",
                "example/repo",
                "PR_one",
            ),
            test_identity(
                crate::remote::ProviderKind::GitHub,
                "github.example.com",
                "example/repo",
                "PR_one",
            ),
            test_identity(
                crate::remote::ProviderKind::GitHub,
                "github.com",
                "example/other",
                "PR_one",
            ),
            test_identity(
                crate::remote::ProviderKind::GitHub,
                "github.com",
                "example/repo",
                "PR_two",
            ),
        ];

        for identity in changed_identities {
            let mut cache = PrCache::observed(base_summary.clone(), Some(details.clone()));
            cache.record_summary_observation(
                Some(PrSummary {
                    change_request_identity: Some(identity),
                    ..base_summary.clone()
                }),
                "now".to_string(),
            );
            assert!(cache.details().is_none());
        }

        let mut cache = PrCache::observed(base_summary.clone(), Some(details));
        cache.record_summary_observation(
            Some(PrSummary {
                head_sha: "head-b".to_string(),
                ..base_summary
            }),
            "now".to_string(),
        );
        assert!(cache.details().is_none());
    }

    #[test]
    fn create_pr_uses_fill_with_explicit_empty_body_and_default_base_when_configured() {
        assert_eq!(
            create_pr_args(Some("main"), "", None, None),
            vec!["pr", "create", "--fill", "--body", "", "--base", "main"]
        );
        assert_eq!(
            create_pr_args(None, "manual description", None, None),
            vec!["pr", "create", "--fill", "--body", "manual description"]
        );
        assert_eq!(
            create_pr_args(
                Some("main"),
                "manual description",
                Some("owner/repo"),
                Some("contributor:topic"),
            ),
            vec![
                "pr",
                "create",
                "--fill",
                "--body",
                "manual description",
                "--repo",
                "owner/repo",
                "--base",
                "main",
                "--head",
                "contributor:topic"
            ]
        );
    }

    #[test]
    fn merge_pr_args_use_configured_method() {
        assert_eq!(
            merge_pr_args("42", MergeMethod::Squash, "abc123", None),
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
            merge_pr_args("42", MergeMethod::Merge, "abc123", None),
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
            merge_pr_args("42", MergeMethod::Rebase, "abc123", None),
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

        merge_pull_request(&config, &worktree, 42, "abc123", None).unwrap();

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
    fn empty_check_rollup_is_authoritative_no_ci_but_missing_rollup_is_unknown() {
        for rollup in ["[]", "null", r#"{"contexts":{"nodes":[]}}"#] {
            let raw = format!(
                r#"{{
                    "number": 42,
                    "state": "OPEN",
                    "statusCheckRollup": {rollup}
                }}"#
            );
            let node = serde_json::from_str::<GithubPullRequest>(&raw).unwrap();
            let summary = pr_summary_from_node(&node, None).unwrap();

            assert_eq!(summary.check_state(), PrCheckState::Success);
        }

        let node =
            serde_json::from_str::<GithubPullRequest>(r#"{"number":42,"state":"OPEN"}"#).unwrap();
        let summary = pr_summary_from_node(&node, None).unwrap();
        assert_eq!(summary.check_state(), PrCheckState::Unknown);
    }

    #[test]
    fn malformed_or_truncated_check_rollup_is_unknown_evidence() {
        for raw in [
            r#"{"number":42,"state":"OPEN","statusCheckRollup":{}}"#,
            r#"{"number":42,"state":"OPEN","statusCheckRollup":{"contexts":{}}}"#,
            r#"{"number":42,"state":"OPEN","statusCheckRollup":{"contexts":{"pageInfo":{"hasNextPage":true},"nodes":[]}}}"#,
        ] {
            assert!(serde_json::from_str::<GithubPullRequest>(raw).is_err());
        }
        let capped = serde_json::json!({
            "number": 42,
            "state": "OPEN",
            "statusCheckRollup": vec![serde_json::json!({"name": "check"}); 100]
        });
        assert!(serde_json::from_value::<GithubPullRequest>(capped).is_err());
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
    fn rest_check_failures_are_detected_case_insensitively() {
        let contexts = vec![
            GithubStatusContext {
                name: Some("check-run".to_string()),
                conclusion: Some("failure".to_string()),
                ..GithubStatusContext::default()
            },
            GithubStatusContext {
                context: Some("commit-status".to_string()),
                state: Some("error".to_string()),
                ..GithubStatusContext::default()
            },
        ];

        assert_eq!(
            collect_failing_checks_from_contexts(&contexts),
            ["check-run", "commit-status"]
        );
    }

    #[test]
    fn resolve_review_thread_args_target_exact_thread_id() {
        let host = crate::remote::HostIdentity::new("github.example.com", None).unwrap();
        let config = crate::test_support::test_config();
        let args = resolve_review_thread_args(&config, &host, "PRRT_thread_1");

        assert_eq!(args[0], "api");
        assert_eq!(args[1], "graphql");
        assert!(
            args.windows(2).any(|pair| {
                pair == ["--hostname".to_string(), "github.example.com".to_string()]
            })
        );
        assert!(args.contains(&"thread=PRRT_thread_1".to_string()));
        assert!(args
            .iter()
            .any(|arg| arg.contains("resolveReviewThread") && arg.contains("threadId: $thread")));
    }

    #[test]
    fn configured_api_override_uses_full_rest_and_graphql_endpoints_with_canonical_host() {
        let host = crate::remote::HostIdentity::new("github.example.com", None).unwrap();
        let mut config = crate::test_support::test_config();
        config.remote_hosts.insert(
            "github.example.com".to_string(),
            crate::config::RemoteHostConfig {
                provider: crate::remote::ProviderKind::GitHub,
                web_url: None,
                api_url: Some("https://broker.example.com/github/api/v3".to_string()),
                credential_env: None,
                allow_http: false,
            },
        );

        let graphql = github_graphql_api_args(&config, &host);
        let rest = github_api_endpoint(&config, &host, "/repos/Acme/Widget");

        assert_eq!(graphql[1], "https://broker.example.com/github/api/graphql");
        assert!(
            graphql.windows(2).any(|pair| {
                pair == ["--hostname".to_string(), "github.example.com".to_string()]
            })
        );
        assert_eq!(
            rest,
            "https://broker.example.com/github/api/v3/repos/Acme/Widget"
        );
    }

    #[cfg(unix)]
    #[test]
    fn ghes_summary_graphql_uses_the_canonical_hostname() {
        let temp = unique_temp_dir("prism-ghes-summary-host");
        fs::create_dir_all(&temp).unwrap();
        let gh = temp.join("gh");
        let log = temp.join("gh.log");
        write_executable(
            &gh,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" > '{}'\nprintf '%s\\n' '{{\"data\":{{\"repository\":{{\"pullRequests\":{{\"nodes\":[],\"pageInfo\":{{\"hasNextPage\":false}}}}}}}}}}'\n",
                log.display()
            ),
        );
        let mut config = test_config();
        config
            .tools
            .insert("gh".to_string(), gh.display().to_string());
        let repository = crate::remote::RemoteRepositoryId::new(
            crate::remote::ProviderKind::GitHub,
            crate::remote::HostIdentity::new("github.example.com", None).unwrap(),
            "Acme/Widget",
        )
        .unwrap();

        assert!(
            fetch_pr_summary_index_for_repository(&temp, &config, &repository)
                .unwrap()
                .is_empty()
        );
        let command = fs::read_to_string(log).unwrap();
        assert!(command.contains("api graphql --hostname github.example.com"));
        assert!(command.contains("owner=Acme"));
        assert!(command.contains("name=Widget"));
        assert!(command.contains("states: OPEN"));

        fs::remove_dir_all(temp).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn exact_summary_observation_queries_only_the_requested_number() {
        let temp = unique_temp_dir("prism-github-exact-summary");
        fs::create_dir_all(&temp).unwrap();
        let gh = temp.join("gh");
        let log = temp.join("gh.log");
        write_executable(
            &gh,
            &format!(
                r#"#!/bin/sh
printf '%s\n' "$*" > '{}'
printf '%s\n' '{{"data":{{"repository":{{"pullRequest":{{"id":"PR_42","number":42,"state":"MERGED","headRefName":"feature","baseRefName":"main","headRefOid":"head","headRepository":{{"nameWithOwner":"Acme/Widget"}},"baseRepository":{{"nameWithOwner":"Acme/Widget"}},"merged":true}}}}}}}}'
"#,
                log.display()
            ),
        );
        let mut config = test_config();
        config
            .tools
            .insert("gh".to_string(), gh.display().to_string());
        let repository = crate::remote::RemoteRepositoryId::new(
            crate::remote::ProviderKind::GitHub,
            crate::remote::HostIdentity::new("github.com", None).unwrap(),
            "Acme/Widget",
        )
        .unwrap();

        let summary = fetch_pr_summary_for_repository_number(&temp, &config, &repository, 42)
            .unwrap()
            .unwrap();

        assert_eq!(summary.number, 42);
        assert!(summary.merged);
        let command = fs::read_to_string(log).unwrap();
        assert!(command.contains("number=42"));
        assert!(command.contains("pullRequest(number: $number)"));
        assert!(!command.contains("pullRequests("));
        fs::remove_dir_all(temp).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn ghes_review_thread_mutation_uses_the_canonical_hostname() {
        let temp = unique_temp_dir("prism-ghes-thread-host");
        fs::create_dir_all(&temp).unwrap();
        let gh = temp.join("gh");
        let log = temp.join("gh.log");
        write_executable(
            &gh,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" > '{}'\nprintf '%s\\n' '{{\"data\":{{\"resolveReviewThread\":{{\"thread\":{{\"id\":\"PRRT_1\",\"isResolved\":true}}}}}}}}'\n",
                log.display()
            ),
        );
        let mut config = test_config();
        config
            .tools
            .insert("gh".to_string(), gh.display().to_string());
        let host = crate::remote::HostIdentity::new("github.example.com", None).unwrap();

        resolve_review_thread(&temp, &config, &host, "PRRT_1").unwrap();

        assert!(
            fs::read_to_string(log)
                .unwrap()
                .contains("api graphql --hostname github.example.com")
        );
        fs::remove_dir_all(temp).unwrap();
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
            change_request_identity: Some(crate::remote::test_change_request_identity()),
            native_state_evidence: crate::remote::NativeStateEvidence {
                lifecycle: vec!["OPEN".to_string()],
                review: vec!["CHANGES_REQUESTED".to_string()],
                mergeability: vec!["CLEAN".to_string()],
                check: vec!["COMPLETED".to_string(), "FAILURE".to_string()],
                queue: vec!["PREPARING".to_string()],
            },
            title: "Fix review".to_string(),
            author: "author".to_string(),
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
            queue_state: "preparing_merged_result".to_string(),
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
        assert_eq!(
            loaded.summary().unwrap().queue_state,
            "preparing_merged_result"
        );
        assert_eq!(
            loaded.summary().unwrap().native_state_evidence,
            cache.summary().unwrap().native_state_evidence
        );
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
            &[],
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
            partial_errors: Vec::new(),
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
    fn unavailable_ci_logs_preserve_previous_logs_without_poisoning_other_details() {
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
        assert!(cache.trusted_details().is_ok());
        assert!(
            cache
                .display_error()
                .is_some_and(|error| error.contains("CI logs unavailable: logs unavailable"))
        );
        let loaded = load_pr_cache(&repo, "feature");
        assert_eq!(loaded.details().unwrap().ci_failures[0].run_id, "old-run");
        assert_eq!(loaded.details().unwrap().ci_failures[0].log_tail, "");
        assert!(
            loaded
                .display_error()
                .is_some_and(|error| error.contains("CI logs unavailable: logs unavailable"))
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
        assert!(!pr_cache_pollable_for_session(&merged, &test_config()));
    }

    #[test]
    fn missing_pr_details_obey_poll_interval_after_an_attempt_starts() {
        let mut cache = PrCache::observed(test_summary("feature", "abc123", 0), None);

        assert!(pr_details_due(&cache));
        let _poll = cache.begin_details_poll();

        assert!(!pr_details_due(&cache));
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
    fn canonical_cached_pr_survives_restart_on_a_synthetic_local_branch() {
        let temp = unique_temp_dir("prism-synthetic-canonical-pr-test");
        fs::create_dir_all(&temp).unwrap();
        let git = temp.join("git");
        write_executable(
            &git,
            "#!/bin/sh\ncase \"$*\" in *\"remote get-url origin\"*) printf 'https://github.com/example/repo.git\\n' ;; *) printf 'local-head\\n' ;; esac\n",
        );
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let mut config = test_config();
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        let mut summary = test_summary("provider-topic", "remote-head", 1);
        summary.change_request_identity = Some(test_identity(
            crate::remote::ProviderKind::GitHub,
            "github.com",
            "example/repo",
            "PR_canonical",
        ));
        let details = PrDetails {
            comments: vec![PrComment {
                body: "persisted association".to_string(),
                ..PrComment::default()
            }],
            ..PrDetails::default()
        };
        let cache = PrCache::observed(summary.clone(), Some(details));
        persist_pr_cache_snapshot(&repo, "pr-42", &cache).unwrap();

        let loaded = load_pr_cache_for_branch(&repo, &config, "pr-42", &temp);

        assert_eq!(loaded.summary(), Some(&summary));
        assert_eq!(
            loaded.details().unwrap().comments[0].body,
            "persisted association"
        );
        let mut session = test_session("pr-42", loaded);
        session.path = temp.clone();
        assert_eq!(
            resolve_pr_summary_for_session(&session, &config, &[summary.clone()]),
            Some(summary)
        );

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
        let summary = PrSummary {
            change_request_identity: Some(test_identity(
                crate::remote::ProviderKind::GitHub,
                "github.com",
                "example/repo",
                "PR_local_repair",
            )),
            ..test_summary("feature", "remote-pr-head", 0)
        };
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
                "pageInfo": {"hasNextPage": false},
                "nodes": [
                  {
                    "number": 9,
                    "title": "Batch polling",
                    "author": {"login": "octocat"},
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
                                "pageInfo": {"hasNextPage": false},
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
        assert_eq!(summaries[0].author, "octocat");
        assert_eq!(summaries[0].head_ref, "feature");
        assert_eq!(summaries[0].review_decision, "UNKNOWN");
        assert_eq!(summaries[0].requested_reviewers, vec!["alice", "backend"]);
        assert_eq!(summaries[0].comment_count, 5);
        assert_eq!(summaries[0].check_status, "passed");
        assert_eq!(summaries[0].merge_state_status, "DIRTY");
    }

    #[test]
    fn graphql_queue_state_distinguishes_native_entry_absence_and_unobserved() {
        let queued = try_parse_pr_summary_index(
            r#"{"data":{"repository":{"pullRequests":{"nodes":[{"number":42,"mergeQueueEntry":{"state":"AWAITING_CHECKS"}}],"pageInfo":{"hasNextPage":false}}}}}"#,
        )
        .unwrap();
        let not_queued = try_parse_pr_summary_index(
            r#"{"data":{"repository":{"pullRequests":{"nodes":[{"number":42,"mergeQueueEntry":null}],"pageInfo":{"hasNextPage":false}}}}}"#,
        )
        .unwrap();
        let direct: GithubPullRequest = serde_json::from_str(r#"{"number":42}"#).unwrap();

        assert_eq!(queued[0].queue_state, "AWAITING_CHECKS");
        assert_eq!(not_queued[0].queue_state, "not_queued");
        assert_eq!(
            pr_summary_from_node(&direct, None).unwrap().queue_state,
            "unknown"
        );
    }

    #[test]
    fn graphql_summary_index_preserves_unknown_lifecycle_without_dropping_other_items() {
        let raw = r#"{
          "data": {"repository": {"pullRequests": {
            "pageInfo": {"hasNextPage": false},
            "nodes": [
              {"number": 9, "state": "OPEN"},
              {"number": 10, "state": "SUPERSEDED_BY_TRAIN"}
            ]
          }}}
        }"#;

        let summaries = try_parse_pr_summary_index(raw).unwrap();

        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].state, "OPEN");
        assert_eq!(summaries[1].state, "SUPERSEDED_BY_TRAIN");
    }

    #[test]
    fn incomplete_graphql_summary_index_is_an_observation_failure() {
        let raw = r#"{"data":{"repository":{}}}"#;

        assert!(try_parse_pr_summary_index(raw).is_err());
    }

    #[test]
    fn incomplete_graphql_summary_pagination_is_a_reported_failure() {
        let raw = include_str!("../../tests/fixtures/remote/github/summary-truncated.json");
        let error = try_parse_pr_summary_index(raw).unwrap_err();
        let summary = test_summary("feature", "abc123", 0);
        let mut cache = PrCache::observed(summary.clone(), None);
        let poll_started_at = Instant::now();
        cache.begin_summary_poll(poll_started_at);

        assert!(apply_pr_summary_poll_result(
            &mut cache,
            poll_started_at,
            Err(error.clone()),
            "not refreshed",
        ));

        assert!(error.contains("pagination is incomplete"));
        assert_eq!(cache.summary(), Some(&summary));
        assert_eq!(
            cache.summary_observation_quality(),
            PrObservationQuality::PreservedStale
        );
        assert_eq!(cache.display_error(), Some(error.as_str()));
        assert!(cache.trusted_summary().is_err());
    }

    #[test]
    fn paginated_graphql_summary_index_combines_every_page() {
        let raw = r#"[
          {"data":{"repository":{"pullRequests":{
            "pageInfo":{"hasNextPage":true,"endCursor":"page-1"},
            "nodes":[{"number":107,"state":"OPEN","headRefName":"feat/remote-adapters"}]
          }}}},
          {"data":{"repository":{"pullRequests":{
            "pageInfo":{"hasNextPage":false,"endCursor":"page-2"},
            "nodes":[{"number":108,"state":"OPEN","headRefName":"feat/tmux-name-convention"}]
          }}}}
        ]"#;

        let summaries = try_parse_pr_summary_index(raw).unwrap();

        assert_eq!(
            summaries
                .iter()
                .map(|summary| (summary.number, summary.head_ref.as_str()))
                .collect::<Vec<_>>(),
            [
                (107, "feat/remote-adapters"),
                (108, "feat/tmux-name-convention")
            ]
        );
    }

    #[test]
    fn exact_graphql_summary_distinguishes_absence_from_query_errors() {
        let repository = crate::remote::RemoteRepositoryId::new(
            crate::remote::ProviderKind::GitHub,
            crate::remote::HostIdentity::parse("github.com").unwrap(),
            "example/repo",
        )
        .unwrap();
        let absent = r#"{"data":{"repository":{"pullRequest":null}}}"#;
        let failed = r#"{
          "data":{"repository":{"pullRequest":null}},
          "errors":[{"message":"temporary failure"}]
        }"#;

        assert_eq!(
            try_parse_pr_summary_for_repository(absent, &repository).unwrap(),
            None
        );
        assert!(
            try_parse_pr_summary_for_repository(failed, &repository)
                .unwrap_err()
                .contains("GraphQL errors")
        );
    }

    #[test]
    fn incomplete_or_truncated_graphql_check_rollup_is_rejected() {
        let response = |page_info: &str| {
            format!(
                r#"{{"data":{{"repository":{{"pullRequests":{{
                    "pageInfo":{{"hasNextPage":false}},
                    "nodes":[{{"number":1,"state":"OPEN","commits":{{"nodes":[{{"commit":{{
                        "statusCheckRollup":{{"contexts":{{{page_info}"nodes":[]}}}}
                    }}}}]}}}}]
                }}}}}}}}"#
            )
        };

        assert!(
            try_parse_pr_summary_index(&response(""))
                .unwrap_err()
                .contains("missing check rollup")
        );
        assert!(
            try_parse_pr_summary_index(&response(r#""pageInfo":{"hasNextPage":true},"#))
                .unwrap_err()
                .contains("first 50")
        );
    }

    #[test]
    fn parses_classic_branch_protection_without_discarding_checks_shape() {
        let facts = parse_classic_branch_protection(
            r#"{
                "url":"https://api.github.com/repos/owner/repo/branches/main/protection",
                "required_pull_request_reviews":{"required_approving_review_count":0,"require_code_owner_reviews":true},
                "required_status_checks":{
                    "strict":true,
                    "contexts":["ci", " lint ", ""],
                    "checks":[{"context":"ci"}, {"context":"build"}]
                },
                "required_conversation_resolution":{"enabled":true}
            }"#,
        )
        .unwrap();

        assert_eq!(facts.required_approvals, 1);
        assert!(facts.require_conversation_resolution);
        assert!(facts.require_branch_up_to_date);
        assert_eq!(facts.required_checks, ["ci", "lint", "build"]);
        assert!(!facts.merge_queue_required);
        assert!(parse_classic_branch_protection("{}").is_err());
    }

    #[test]
    fn fetches_and_combines_exact_branch_classic_and_evaluated_ruleset_policy() {
        let temp = unique_temp_dir("prism-github-exact-policy");
        fs::create_dir_all(&temp).unwrap();
        let gh = temp.join("gh");
        let log = temp.join("gh.log");
        write_executable(
            &gh,
            &format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$*" in
  *'/repos/owner/repo/branches/release%2Fnext/protection'*)
    printf '%s\n' '{{"url":"https://api.github.com/repos/owner/repo/branches/release%2Fnext/protection","required_pull_request_reviews":{{"required_approving_review_count":1}},"required_status_checks":{{"strict":false,"contexts":["classic-ci"]}},"required_conversation_resolution":{{"enabled":true}}}}'
    ;;
  *'/repos/owner/repo/rules/branches/release%2Fnext?per_page=100'*)
    printf '%s\n' '[[{{"type":"pull_request","parameters":{{"required_approving_review_count":2,"required_review_thread_resolution":false,"require_code_owner_review":false,"require_last_push_approval":false}}}},{{"type":"required_status_checks","parameters":{{"strict_required_status_checks_policy":true,"required_status_checks":[{{"context":"ruleset-ci"}}]}}}},{{"type":"merge_queue","parameters":{{"check_response_timeout_minutes":60,"grouping_strategy":"ALLGREEN","max_entries_to_build":5,"max_entries_to_merge":5,"merge_method":"SQUASH","min_entries_to_merge":1,"min_entries_to_merge_wait_minutes":0}}}}]]'
    ;;
  *)
    printf '%s\n' 'unexpected gh command' >&2
    exit 1
    ;;
esac
"#,
                log.display()
            ),
        );
        let mut config = test_config();
        config
            .tools
            .insert("gh".to_string(), gh.display().to_string());
        let repository = crate::remote::RemoteRepositoryId::new(
            crate::remote::ProviderKind::GitHub,
            crate::remote::HostIdentity::new("github.com", None).unwrap(),
            "owner/repo",
        )
        .unwrap();

        let policy = fetch_repo_policy(&temp, &config, &repository, "release/next").unwrap();

        assert_eq!(policy.target_branch.as_deref(), Some("release/next"));
        assert_eq!(policy.required_approvals, 2);
        assert!(policy.require_conversation_resolution);
        assert!(policy.require_branch_up_to_date);
        assert_eq!(policy.required_checks, ["classic-ci", "ruleset-ci"]);
        assert!(policy.merge_queue_required);
        assert!(policy.error.is_none());
        let commands = fs::read_to_string(&log).unwrap();
        assert!(commands.contains("--paginate --slurp"));

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn authoritative_unprotected_and_empty_rulesets_produce_known_empty_policy() {
        let temp = unique_temp_dir("prism-github-empty-policy");
        fs::create_dir_all(&temp).unwrap();
        let gh = temp.join("gh");
        write_executable(
            &gh,
            r#"#!/bin/sh
case "$*" in
  *'/protection'*)
    printf '%s\n' 'gh: Branch not protected (HTTP 404)' >&2
    exit 1
    ;;
  *'/rules/branches/'*)
    printf '%s\n' '[[]]'
    ;;
  *) exit 1 ;;
esac
"#,
        );
        let mut config = test_config();
        config
            .tools
            .insert("gh".to_string(), gh.display().to_string());
        let repository = crate::remote::RemoteRepositoryId::new(
            crate::remote::ProviderKind::GitHub,
            crate::remote::HostIdentity::new("github.com", None).unwrap(),
            "owner/repo",
        )
        .unwrap();

        let policy = fetch_repo_policy(&temp, &config, &repository, "main").unwrap();

        assert_eq!(policy.required_approvals, 0);
        assert!(policy.required_checks.is_empty());
        assert!(!policy.merge_queue_required);
        assert!(policy.error.is_none());

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn evaluated_rules_require_paginated_envelope_and_complete_parameters() {
        assert!(parse_evaluated_branch_rules("[]").is_err());
        assert!(parse_evaluated_branch_rules(r#"[{"type":"merge_queue"}]"#).is_err());
        assert!(parse_evaluated_branch_rules(r#"[[{"type":"merge_queue"}]]"#).is_err());
        assert!(
            parse_evaluated_branch_rules(r#"[[{"type":"required_status_checks"}]]"#)
                .unwrap_err()
                .contains("missing parameters")
        );
        assert!(
            parse_evaluated_branch_rules(
                r#"[[{"type":"pull_request","parameters":{"required_approving_review_count":"one"}}]]"#,
            )
            .unwrap_err()
            .contains("malformed pull_request")
        );
    }

    #[test]
    fn evaluated_rules_ignore_known_non_merge_constraints() {
        let facts = parse_evaluated_branch_rules(
            r#"[[
                {"type":"required_linear_history"},
                {"type":"required_signatures"},
                {"type":"commit_message_pattern","parameters":{"operator":"starts_with"}},
                {"type":"copilot_code_review","parameters":{"review_on_push":false}}
            ]]"#,
        )
        .unwrap();

        assert_eq!(facts.required_approvals, 0);
        assert!(facts.required_checks.is_empty());
        assert!(!facts.merge_queue_required);
    }

    #[test]
    fn safety_relevant_and_unknown_rules_produce_unknown_policy_evidence() {
        for rule_type in [
            "workflows",
            "required_deployments",
            "code_scanning",
            "future_rule",
        ] {
            let raw = format!(r#"[[{{"type":"{rule_type}"}}]]"#);
            let error = parse_evaluated_branch_rules(&raw).unwrap_err();

            assert!(error.contains("policy evidence is unknown"));
            assert!(error.contains(rule_type));
            assert!(!error.contains("malformed"));
        }
    }

    #[test]
    fn only_explicit_unprotected_404_is_authoritative_classic_absence() {
        assert!(is_unprotected_branch_response(
            "gh: Branch not protected (HTTP 404)"
        ));
        assert!(!is_unprotected_branch_response(
            "gh: Branch not found (HTTP 404)"
        ));
        assert!(!is_unprotected_branch_response(
            "gh: Resource not accessible by integration (HTTP 403)"
        ));
    }

    #[test]
    fn failed_policy_refresh_preserves_identity_matched_stale_facts() {
        let temp = unique_temp_dir("prism-github-stale-policy-refresh");
        fs::create_dir_all(&temp).unwrap();
        let gh = temp.join("gh");
        write_executable(&gh, "#!/bin/sh\necho 'policy unavailable' >&2\nexit 1\n");
        let git = temp.join("git");
        write_executable(
            &git,
            "#!/bin/sh\nprintf 'https://github.com/owner/repo.git\\n'\n",
        );
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let mut config = test_config();
        config
            .tools
            .insert("gh".to_string(), gh.display().to_string());
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        let stale = RepoPolicyCache {
            repo_remote: "owner/repo".to_string(),
            provider: Some(crate::remote::ProviderKind::GitHub),
            canonical_host: Some("github.com".to_string()),
            project_path: Some("owner/repo".to_string()),
            target_branch: Some("main".to_string()),
            identity_complete: true,
            default_branch: Some("main".to_string()),
            required_approvals: 2,
            require_conversation_resolution: true,
            require_branch_up_to_date: true,
            required_checks: vec!["ci".to_string()],
            merge_queue_required: true,
            refreshed_unix_ms: 123,
            error: None,
        };
        save_repo_policy_cache(&repo, &stale).unwrap();

        let refreshed = refresh_repo_policy_cache(&repo, &temp, &config).unwrap();

        assert_eq!(refreshed.required_approvals, stale.required_approvals);
        assert_eq!(refreshed.required_checks, stale.required_checks);
        assert_eq!(
            refreshed.require_conversation_resolution,
            stale.require_conversation_resolution
        );
        assert_eq!(
            refreshed.require_branch_up_to_date,
            stale.require_branch_up_to_date
        );
        assert_eq!(refreshed.merge_queue_required, stale.merge_queue_required);
        assert_eq!(refreshed.refreshed_unix_ms, stale.refreshed_unix_ms);
        assert!(
            refreshed
                .error
                .as_deref()
                .is_some_and(|error| error.contains("policy unavailable"))
        );
        assert_eq!(load_repo_policy_cache(&repo, "owner/repo"), Some(refreshed));

        let _ = fs::remove_dir_all(repo.prism_dir());
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn repo_policy_cache_round_trips_success_and_error() {
        let temp = unique_temp_dir("prism-repo-policy-cache-test");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let policy = RepoPolicyCache {
            repo_remote: "owner/repo".to_string(),
            provider: Some(crate::remote::ProviderKind::GitHub),
            canonical_host: Some("github.com".to_string()),
            project_path: Some("owner/repo".to_string()),
            target_branch: Some("main".to_string()),
            identity_complete: true,
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
        let github_repository = crate::remote::RemoteRepositoryId::new(
            crate::remote::ProviderKind::GitHub,
            crate::remote::HostIdentity::new("github.com", None).unwrap(),
            "owner/repo",
        )
        .unwrap();
        assert_eq!(
            load_repo_policy_cache_for_repository(&repo, &github_repository),
            Some(policy.clone())
        );
        let enterprise_repository = crate::remote::RemoteRepositoryId::new(
            crate::remote::ProviderKind::GitHub,
            crate::remote::HostIdentity::new("github.example.com", None).unwrap(),
            "owner/repo",
        )
        .unwrap();
        assert!(load_repo_policy_cache_for_repository(&repo, &enterprise_repository).is_none());

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
    fn repo_policy_cache_keeps_distinct_target_branches_under_one_identity() {
        let temp = unique_temp_dir("prism-repo-policy-identity-test");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let repository = crate::remote::RemoteRepositoryId::new(
            crate::remote::ProviderKind::GitLab,
            crate::remote::HostIdentity::new("gitlab.example.com", Some(8443)).unwrap(),
            "owner/repo",
        )
        .unwrap();
        let policy = |target: &str, approvals: u64| RepoPolicyCache {
            repo_remote: "owner/repo".to_string(),
            provider: Some(crate::remote::ProviderKind::GitLab),
            canonical_host: Some("gitlab.example.com:8443".to_string()),
            project_path: Some("owner/repo".to_string()),
            target_branch: Some(target.to_string()),
            identity_complete: true,
            default_branch: Some("main".to_string()),
            required_approvals: approvals,
            refreshed_unix_ms: approvals,
            ..RepoPolicyCache::default()
        };

        save_repo_policy_cache(&repo, &policy("main", 1)).unwrap();
        save_repo_policy_cache(&repo, &policy("release/next", 2)).unwrap();

        assert_eq!(
            load_repo_policy_cache_for_identity(&repo, &repository, "main")
                .unwrap()
                .required_approvals,
            1
        );
        assert_eq!(
            load_repo_policy_cache_for_identity(&repo, &repository, "release/next")
                .unwrap()
                .required_approvals,
            2
        );

        let _ = fs::remove_dir_all(repo.prism_dir());
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn github_policy_identity_queries_and_upserts_use_normalized_path_keys() {
        let temp = unique_temp_dir("prism-github-policy-case-key-test");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let policy = |project_path: &str, approvals: u64| RepoPolicyCache {
            repo_remote: project_path.to_string(),
            provider: Some(crate::remote::ProviderKind::GitHub),
            canonical_host: Some("github.com".to_string()),
            project_path: Some(project_path.to_string()),
            target_branch: Some("main".to_string()),
            identity_complete: true,
            default_branch: Some("main".to_string()),
            required_approvals: approvals,
            refreshed_unix_ms: approvals,
            ..RepoPolicyCache::default()
        };
        save_repo_policy_cache(&repo, &policy("Acme/Widget", 1)).unwrap();
        save_repo_policy_cache(&repo, &policy("acme/widget", 2)).unwrap();
        let lowercase = crate::remote::RemoteRepositoryId::new(
            crate::remote::ProviderKind::GitHub,
            crate::remote::HostIdentity::new("github.com", None).unwrap(),
            "ACME/WIDGET",
        )
        .unwrap();

        let loaded = load_repo_policy_cache_for_identity(&repo, &lowercase, "main").unwrap();
        let count = observability::with_writable_db(&repo, |conn| {
            conn.query_row(
                "select count(*) from repo_policy_cache_v2 where provider = 'github'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|error| error.to_string())
        })
        .unwrap();

        assert_eq!(loaded.required_approvals, 2);
        assert_eq!(loaded.project_path.as_deref(), Some("acme/widget"));
        assert_eq!(count, 1);
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
                    "totalCount": 2,
                    "pageInfo": {"hasNextPage": false},
                    "nodes": [
                    {
                      "id": "PRRT_kw123",
                      "isResolved": true,
                      "comments": {
                        "totalCount": 1,
                        "pageInfo": {"hasNextPage": false},
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
                        "totalCount": 1,
                        "pageInfo": {"hasNextPage": false},
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
    fn rejects_truncated_review_threads() {
        let raw = r#"{
          "data": {
            "repository": {
              "pullRequest": {
                "reviewThreads": {
                  "totalCount": 2,
                  "pageInfo": {"hasNextPage": false},
                  "nodes": [
                    {
                      "id": "PRRT_kw123",
                      "isResolved": false,
                      "comments": {"totalCount": 0, "pageInfo": {"hasNextPage": false}, "nodes": []}
                    }
                  ]
                }
              }
            }
          }
        }"#;

        assert_eq!(
            try_parse_review_thread_comments(raw).unwrap_err(),
            "GitHub returned only 1 of 2 review threads"
        );
    }

    #[test]
    fn combines_paginated_review_threads() {
        let raw = r#"[
          {
            "data": {"repository": {"pullRequest": {"reviewThreads": {
              "totalCount": 2,
              "pageInfo": {"hasNextPage": true},
              "nodes": [{
                "id": "PRRT_1",
                "isResolved": false,
                "comments": {"totalCount": 1, "pageInfo": {"hasNextPage": false}, "nodes": [{"id": "C1", "body": "one"}]}
              }]
            }}}}
          },
          {
            "data": {"repository": {"pullRequest": {"reviewThreads": {
              "totalCount": 2,
              "pageInfo": {"hasNextPage": false},
              "nodes": [{
                "id": "PRRT_2",
                "isResolved": false,
                "comments": {"totalCount": 1, "pageInfo": {"hasNextPage": false}, "nodes": [{"id": "C2", "body": "two"}]}
              }]
            }}}}
          }
        ]"#;

        let comments = try_parse_review_thread_comments(raw).unwrap();

        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].thread_id, "PRRT_1");
        assert_eq!(comments[1].thread_id, "PRRT_2");
    }

    #[test]
    fn rejects_truncated_comments_inside_a_review_thread() {
        let raw = r#"[{
          "data": {"repository": {"pullRequest": {"reviewThreads": {
            "totalCount": 1,
            "pageInfo": {"hasNextPage": false},
            "nodes": [{
              "id": "PRRT_1",
              "isResolved": false,
              "comments": {
                "totalCount": 101,
                "pageInfo": {"hasNextPage": true},
                "nodes": [{"id": "C1", "body": "one"}]
              }
            }]
          }}}}
        }]"#;

        let error = try_parse_review_thread_comments(raw).unwrap_err();

        assert!(error.contains("only 1 of 101 comments"));
    }

    #[test]
    fn canonical_target_number_details_use_complete_paginated_endpoints() {
        let temp = unique_temp_dir("prism-github-paginated-details");
        fs::create_dir_all(&temp).unwrap();
        let gh = temp.join("gh");
        let log = temp.join("gh.log");
        write_executable(
            &gh,
            &format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$*" in
  *'/repos/target/repo/issues/42/comments?per_page=100'*)
    printf '%s\n' '[[{{"id":"C1","body":"one","user":{{"login":"alice"}}}}],[{{"id":"C2","body":"two","user":{{"login":"bob"}}}}]]'
    ;;
  *'/repos/target/repo/pulls/42/reviews?per_page=100'*)
    printf '%s\n' '[[{{"id":"R1","state":"APPROVED","user":{{"login":"reviewer"}}}}]]'
    ;;
  *'/repos/target/repo/pulls/42/files?per_page=100'*)
    printf '%s\n' '[[{{"filename":"src/one.rs"}}],[{{"filename":"src/two.rs"}}]]'
    ;;
  *'/repos/target/repo/commits/head-sha/check-runs?per_page=100'*)
    printf '%s\n' '[{{"total_count":1,"check_runs":[{{"name":"build","status":"completed","conclusion":"success"}}]}}]'
    ;;
  *'/repos/target/repo/commits/head-sha/statuses?per_page=100'*)
    printf '%s\n' '[[{{"context":"legacy-ci","state":"success"}}]]'
    ;;
  *'api graphql'*)
    printf '%s\n' '[{{"data":{{"repository":{{"pullRequest":{{"reviewThreads":{{"totalCount":0,"pageInfo":{{"hasNextPage":false}},"nodes":[]}}}}}}}}}}]'
    ;;
  *)
    printf '%s\n' 'unexpected gh command' >&2
    exit 1
    ;;
esac
"#,
                log.display()
            ),
        );
        let mut config = test_config();
        config
            .tools
            .insert("gh".to_string(), gh.display().to_string());
        let repository = crate::remote::RemoteRepositoryId::new(
            crate::remote::ProviderKind::GitHub,
            crate::remote::HostIdentity::new("github.com", None).unwrap(),
            "target/repo",
        )
        .unwrap();

        let details = fetch_pr_details_for_repository_number(
            &temp,
            &config,
            &repository,
            42,
            "synthetic-local-branch",
            "head-sha",
        )
        .unwrap();

        assert_eq!(details.comments.unwrap().len(), 2);
        assert_eq!(details.reviews.unwrap().len(), 1);
        assert_eq!(details.files.unwrap(), ["src/one.rs", "src/two.rs"]);
        assert!(details.review_comments.unwrap().is_empty());
        assert!(details.failing_checks.unwrap().is_empty());
        assert_eq!(details.check_contexts.unwrap().len(), 2);
        let commands = fs::read_to_string(log).unwrap();
        assert_eq!(commands.matches("--paginate --slurp").count(), 6);
        assert!(!commands.contains("synthetic-local-branch"));

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn fetch_pr_summary_uses_merged_at_instead_of_removed_merged_field() {
        let temp = unique_temp_dir("prism-gh-summary-test");
        let bin = temp.join("bin");
        let repo = temp.join("repo");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&repo).unwrap();
        let gh = bin.join("gh");
        let git = bin.join("git");
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
  "id": "PR_test",
  "title": "Test PR",
  "url": "https://github.com/example/repo/pull/7",
  "state": "CLOSED",
  "reviewDecision": null,
  "headRefName": "feature",
  "baseRefName": "main",
  "headRefOid": "abc123",
  "headRepository": {"nameWithOwner": "example/repo"},
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
        write_executable(
            &git,
            "#!/bin/sh\nprintf 'https://github.com/example/repo.git\\n'\n",
        );

        let mut config = test_config();
        config
            .tools
            .insert("gh".to_string(), gh.display().to_string());
        config
            .tools
            .insert("git".to_string(), git.display().to_string());

        let summary = fetch_pr_summary(&repo, "feature", &config)
            .unwrap()
            .unwrap()
            .0;

        assert_eq!(summary.number, 7);
        assert_eq!(summary.review_decision, "UNKNOWN");
        assert!(summary.merged);
        assert_eq!(
            summary
                .change_request_identity
                .as_ref()
                .map(|identity| identity.native_id()),
            Some("PR_test")
        );

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn fetch_pr_summary_preserves_unknown_native_lifecycle() {
        let temp = unique_temp_dir("prism-gh-unknown-summary-test");
        let bin = temp.join("bin");
        let repo = temp.join("repo");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&repo).unwrap();
        let gh = bin.join("gh");
        let git = bin.join("git");
        write_executable(
            &gh,
            r#"#!/bin/sh
cat <<'JSON'
{
  "number": 7,
  "id": "PR_test",
  "title": "Test PR",
  "state": "SUPERSEDED_BY_TRAIN",
  "headRefName": "feature",
  "baseRefName": "main",
  "headRefOid": "abc123",
  "headRepository": {"nameWithOwner": "example/repo"},
  "statusCheckRollup": []
}
JSON
"#,
        );
        write_executable(
            &git,
            "#!/bin/sh\nprintf 'https://github.com/example/repo.git\\n'\n",
        );
        let mut config = test_config();
        config
            .tools
            .insert("gh".to_string(), gh.display().to_string());
        config
            .tools
            .insert("git".to_string(), git.display().to_string());

        let summary = fetch_pr_summary(&repo, "feature", &config)
            .unwrap()
            .unwrap()
            .0;

        assert_eq!(summary.state, "SUPERSEDED_BY_TRAIN");
        assert!(!summary.merged);
        assert_eq!(
            summary.native_state_evidence.lifecycle,
            ["SUPERSEDED_BY_TRAIN"]
        );

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn github_summary_retains_known_lossy_and_future_native_states() {
        let node: GithubPullRequest = serde_json::from_str(
            r#"{
                "number":42,
                "state":"OPEN",
                "reviewDecision":"REVIEW_REQUIRED",
                "mergeStateStatus":"HAS_HOOKS",
                "statusCheckRollup":[
                    {"name":"build","status":"COMPLETED","conclusion":"NEUTRAL"},
                    {"context":"future","state":"NEW_CHECK_STATE"}
                ],
                "mergeQueueEntry":{"state":"AWAITING_CHECKS"}
            }"#,
        )
        .unwrap();

        let summary = pr_summary_from_node(&node, None).unwrap();

        assert_eq!(summary.native_state_evidence.lifecycle, ["OPEN"]);
        assert_eq!(summary.native_state_evidence.review, ["REVIEW_REQUIRED"]);
        assert_eq!(summary.native_state_evidence.mergeability, ["HAS_HOOKS"]);
        assert_eq!(
            summary.native_state_evidence.check,
            ["COMPLETED", "NEUTRAL", "NEW_CHECK_STATE"]
        );
        assert_eq!(summary.native_state_evidence.queue, ["AWAITING_CHECKS"]);
    }

    #[test]
    fn closed_unmerged_request_does_not_match_a_worktree() {
        let mut summary = test_summary("feature", "head123", 0);
        summary.state = "CLOSED".to_string();

        assert!(!pr_summary_matches_worktree(
            &summary,
            "feature",
            Some(&summary),
            None,
            None,
        ));
    }

    #[test]
    fn initial_association_requires_origin_push_source_and_exact_local_head() {
        let origin_push = crate::remote::RemoteRepositoryId::new(
            crate::remote::ProviderKind::GitHub,
            crate::remote::HostIdentity::new("github.example.com", None).unwrap(),
            "Contributor/Widget",
        )
        .unwrap();
        let target = crate::remote::RemoteRepositoryId::new(
            crate::remote::ProviderKind::GitHub,
            crate::remote::HostIdentity::new("github.example.com", None).unwrap(),
            "Acme/Widget",
        )
        .unwrap();
        let unrelated = crate::remote::RemoteRepositoryId::new(
            crate::remote::ProviderKind::GitHub,
            crate::remote::HostIdentity::new("github.example.com", None).unwrap(),
            "Other/Widget",
        )
        .unwrap();
        let native = crate::remote::NativeChangeRequestId::new("PR_42").unwrap();
        let mut summary = test_summary("topic", "head-42", 0);
        summary.change_request_identity = Some(crate::remote::CanonicalChangeRequestIdentity::new(
            &target, &native, &unrelated, &target,
        ));

        assert!(!pr_summary_matches_worktree(
            &summary,
            "topic",
            None,
            Some(&origin_push),
            Some("head-42"),
        ));

        summary.change_request_identity = Some(crate::remote::CanonicalChangeRequestIdentity::new(
            &target,
            &native,
            &crate::remote::RemoteRepositoryId::new(
                crate::remote::ProviderKind::GitHub,
                crate::remote::HostIdentity::new("github.example.com", None).unwrap(),
                "contributor/widget",
            )
            .unwrap(),
            &target,
        ));
        assert!(!pr_summary_matches_worktree(
            &summary,
            "topic",
            None,
            Some(&origin_push),
            Some("different-head"),
        ));
        assert!(pr_summary_matches_worktree(
            &summary,
            "topic",
            None,
            Some(&origin_push),
            Some("head-42"),
        ));
    }

    #[test]
    fn explicit_cached_target_pr_preserves_maintainer_fork_association() {
        let host = crate::remote::HostIdentity::new("github.com", None).unwrap();
        let source = crate::remote::RemoteRepositoryId::new(
            crate::remote::ProviderKind::GitHub,
            host.clone(),
            "contributor/widget",
        )
        .unwrap();
        let target = crate::remote::RemoteRepositoryId::new(
            crate::remote::ProviderKind::GitHub,
            host,
            "acme/widget",
        )
        .unwrap();
        let identity = crate::remote::CanonicalChangeRequestIdentity::new(
            &target,
            &crate::remote::NativeChangeRequestId::new("PR_fork").unwrap(),
            &source,
            &target,
        );
        let known = PrSummary {
            change_request_identity: Some(identity.clone()),
            ..test_summary("contributor-topic", "remote-head", 0)
        };
        let observed = PrSummary {
            head_sha: "advanced-remote-head".to_string(),
            ..known.clone()
        };
        let maintainer_origin = crate::remote::RemoteRepositoryId::new(
            crate::remote::ProviderKind::GitHub,
            crate::remote::HostIdentity::new("github.com", None).unwrap(),
            "acme/widget",
        )
        .unwrap();

        assert!(pr_summary_matches_worktree(
            &observed,
            "pr/42",
            Some(&known),
            Some(&maintainer_origin),
            Some("local-repair"),
        ));
    }

    #[test]
    fn unknown_lifecycle_poll_preserves_matching_canonical_session_association() {
        let identity = test_identity(
            crate::remote::ProviderKind::GitHub,
            "github.com",
            "example/repo",
            "PR_42",
        );
        let mut known = test_summary("provider-feature", "head123", 0);
        known.change_request_identity = Some(identity.clone());
        let mut observed = known.clone();
        observed.state = "SUPERSEDED_BY_TRAIN".to_string();
        let mut session = test_session(
            "pr/42",
            PrCache::observed(known, Some(PrDetails::default())),
        );

        let resolved = resolve_pr_summary_for_session(
            &session,
            &test_config(),
            std::slice::from_ref(&observed),
        );
        let poll_started_at = Instant::now();
        session.pr.begin_summary_poll(poll_started_at);
        assert!(apply_pr_summary_poll_result(
            &mut session.pr,
            poll_started_at,
            Ok(resolved),
            "now",
        ));

        assert_eq!(session.pr.summary(), Some(&observed));
        assert_eq!(
            session.pr.summary_observation_quality(),
            PrObservationQuality::Fresh
        );
        assert!(session.pr.trusted_summary().is_ok());
        assert_eq!(
            session
                .pr
                .summary()
                .and_then(|summary| summary.change_request_identity.as_ref()),
            Some(&identity)
        );
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
            change_request_identity: None,
            native_state_evidence: crate::remote::NativeStateEvidence::default(),
            title: "Fix review".to_string(),
            author: "author".to_string(),
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
            queue_state: "not_queued".to_string(),
            comment_count,
            merged: false,
            draft: false,
        }
    }

    fn test_identity(
        provider: crate::remote::ProviderKind,
        host: &str,
        project_path: &str,
        native_id: &str,
    ) -> crate::remote::CanonicalChangeRequestIdentity {
        let repository = crate::remote::RemoteRepositoryId::new(
            provider,
            crate::remote::HostIdentity::new(host, None).unwrap(),
            project_path,
        )
        .unwrap();
        crate::remote::CanonicalChangeRequestIdentity::new(
            &repository,
            &crate::remote::NativeChangeRequestId::new(native_id).unwrap(),
            &repository,
            &repository,
        )
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
