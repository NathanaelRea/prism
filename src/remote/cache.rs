use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::session::Session;

pub const PR_SUMMARY_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);
pub(super) const PR_DETAIL_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

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
    pub(super) pr_number: u64,
    pub(super) head_sha: String,
    pub(super) change_request_identity: Option<crate::remote::CanonicalChangeRequestIdentity>,
}

impl PrDetailsAssociation {
    pub(super) fn from_summary(summary: &PrSummary) -> Self {
        Self {
            pr_number: summary.number,
            head_sha: summary.head_sha.clone(),
            change_request_identity: summary.change_request_identity.clone(),
        }
    }

    pub(super) fn matches(&self, summary: &PrSummary) -> bool {
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
    pub(super) summary: Option<PrSummary>,
    pub(super) details: Option<PrDetails>,
    pub(super) last_polled: Option<Instant>,
    pub(super) details_last_polled: Option<Instant>,
    pub(super) last_refreshed: Option<String>,
    pub(super) signature: Option<String>,
    pub(super) error: Option<String>,
    pub(super) summary_quality: PrObservationQuality,
    pub(super) details_quality: PrObservationQuality,
    pub(super) details_association: Option<PrDetailsAssociation>,
    pub(super) summary_error: Option<String>,
    pub(super) details_errors: Vec<String>,
    pub(super) details_warnings: Vec<String>,
    pub(super) persistence_error: Option<String>,
    pub(super) details_persistence_error: Option<String>,
    pub(super) next_generation: u64,
    pub(super) pending_summary: Option<(u64, Instant)>,
    pub(super) pending_details: Option<u64>,
    pub(super) summary_observed_in_process: bool,
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
    pub(crate) fn replace_details_for_test(&mut self, details: PrDetails) {
        self.details = Some(details);
        self.details_association = self.summary_identity();
        self.details_quality = PrObservationQuality::Fresh;
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

    pub(crate) fn authorize_guarded_refresh(
        &mut self,
        identity: Option<&crate::remote::CanonicalChangeRequestIdentity>,
        head_sha: Option<&str>,
    ) {
        let (Some(identity), Some(head_sha)) = (identity, head_sha) else {
            return;
        };
        self.reauthorize_guarded_summary(identity, head_sha);
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

    pub(super) fn summary_identity(&self) -> Option<PrDetailsAssociation> {
        self.summary
            .as_ref()
            .map(PrDetailsAssociation::from_summary)
    }

    pub(crate) fn reauthorize_guarded_summary(
        &mut self,
        expected_identity: &crate::remote::CanonicalChangeRequestIdentity,
        expected_head_sha: &str,
    ) {
        if self.summary.as_ref().is_some_and(|summary| {
            summary.change_request_identity.as_ref() == Some(expected_identity)
                && summary.head_sha == expected_head_sha
        }) {
            self.summary_observed_in_process = true;
        }
    }

    pub(crate) fn reauthorize_persisted_run_summary(
        &mut self,
        expected_number: u64,
        expected_url: &str,
        expected_head_sha: &str,
    ) {
        if self.summary.as_ref().is_some_and(|summary| {
            summary.number == expected_number
                && summary.url == expected_url
                && summary.head_sha == expected_head_sha
                && summary.change_request_identity.is_some()
        }) {
            self.summary_observed_in_process = true;
        }
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

    pub(super) fn finish_summary_poll(&mut self, started_at: Instant) -> bool {
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

    pub(super) fn accepts_details_poll(&self, result: &Self) -> bool {
        self.pending_details.is_some() && self.pending_details == result.pending_details
    }

    fn details_are_associated(&self) -> bool {
        self.summary.as_ref().is_some_and(|summary| {
            self.details_association
                .as_ref()
                .is_some_and(|association| association.matches(summary))
        })
    }

    pub(super) fn rebuild_error(&mut self) {
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

    pub(super) fn record_persistence_result(&mut self, result: Result<(), String>) {
        match result {
            Ok(()) => self.persistence_error = None,
            Err(error) => self.persistence_error = Some(error),
        }
        self.rebuild_error();
    }

    pub(super) fn refresh_result(&self) -> Result<(), String> {
        self.summary_error
            .as_ref()
            .or_else(|| self.details_errors.first())
            .or(self.persistence_error.as_ref())
            .or(self.details_persistence_error.as_ref())
            .map_or(Ok(()), |error| Err(error.clone()))
    }

    pub(super) fn record_summary_failure(&mut self, error: String) {
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

    pub(super) fn record_summary_observation(
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

    pub(super) fn record_details_observation(&mut self, observation: PrDetailsObservation) -> bool {
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
            Err(error) => warnings.push(format!("CI logs unavailable: {error}")),
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
            self.record_summary_observation(None, crate::util::timestamp_label());
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PrCacheSummaryMutation {
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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
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
pub(super) struct PrDetailsObservation {
    pub(super) association: PrDetailsAssociation,
    pub(super) comments: Result<Vec<PrComment>, String>,
    pub(super) reviews: Result<Vec<PrReview>, String>,
    pub(super) review_comments: Result<Vec<PrReviewComment>, String>,
    pub(super) files: Result<Vec<String>, String>,
    pub(super) failing_checks: Result<Vec<String>, String>,
    pub(super) check_contexts: Result<Vec<PrCheckContext>, String>,
    pub(super) ci_failures: Result<Vec<CiFailure>, String>,
    pub(super) partial_errors: Vec<String>,
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

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct PrComment {
    #[serde(default)]
    pub id: String,
    pub author: String,
    pub body: String,
    #[serde(default)]
    pub created_at: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct PrReview {
    #[serde(default)]
    pub id: String,
    pub author: String,
    pub state: String,
    pub body: String,
    #[serde(default)]
    pub submitted_at: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
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

pub(super) fn pr_details_due(cache: &PrCache) -> bool {
    if cache.summary.is_none() {
        return false;
    }
    cache
        .details_last_polled
        .map(|last| last.elapsed() >= PR_DETAIL_POLL_INTERVAL)
        .unwrap_or(true)
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

#[cfg(test)]
pub(crate) fn pr_cache_has_comments(cache: &PrCache) -> bool {
    pr_cache_comment_count(cache) > 0
}
