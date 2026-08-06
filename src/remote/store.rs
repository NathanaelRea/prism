use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::observability;
use crate::persistence::remote::{self as persistence, DetailsRecord, PolicyRecord, SummaryRecord};
use crate::repo::Repository;
use crate::util::timestamp_label;

use super::cache::{
    CiFailure, PrCache, PrDetails, PrDetailsAssociation, PrObservationQuality, PrSummary,
    RepoPolicyCache,
};
use super::{
    CanonicalChangeRequestIdentity, HostIdentity, NativeChangeRequestId, ProviderKind,
    RemoteRepositoryId,
};

struct PersistedPrDetails {
    details: PrDetails,
    association: PrDetailsAssociation,
    errors: Vec<String>,
    warnings: Vec<String>,
}

pub(crate) fn load_pr_cache(repo: &Repository, branch: &str) -> PrCache {
    match load_pr_cache_result(repo, branch) {
        Ok(cache) => cache,
        Err(error) => {
            let mut cache = PrCache::default();
            cache.record_summary_failure(error);
            cache
        }
    }
}

fn load_pr_cache_result(repo: &Repository, branch: &str) -> Result<PrCache, String> {
    let (summary, details) = persistence::load_snapshot(&observability::db_path(repo), branch)
        .map_err(|error| format!("read PR cache snapshot: {error}"))?;
    let Some(summary) = summary else {
        if details.is_some() {
            return Err("PR details cache exists without an associated summary".to_string());
        }
        return Ok(PrCache::default());
    };
    let (summary, last_refreshed, summary_error) = decode_summary(summary)?;
    let persisted_details = details.map(decode_details).transpose()?;
    let (details, association, details_errors, details_warnings) = match persisted_details {
        Some(details) => {
            if !details.association.matches(&summary) {
                return Err("PR details cache identity does not match its summary".to_string());
            }
            (
                Some(details.details),
                Some(details.association),
                details.errors,
                details.warnings,
            )
        }
        None => (None, None, Vec::new(), Vec::new()),
    };
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
        summary_quality: PrObservationQuality::PreservedStale,
        details_quality,
        details_association: association,
        summary_error,
        details_errors,
        details_warnings,
        ..PrCache::default()
    };
    cache.rebuild_error();
    Ok(cache)
}

pub(super) fn record_provider_summary_refresh(
    repo: &Repository,
    branch: &str,
    cache: &mut PrCache,
    observation: Result<Option<PrSummary>, String>,
) -> Result<(), String> {
    let started_at = std::time::Instant::now();
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

pub(crate) fn record_pr_summary(
    repo: &Repository,
    branch: &str,
    cache: &mut PrCache,
    summary: PrSummary,
) {
    let started_at = std::time::Instant::now();
    cache.begin_summary_poll(started_at);
    cache.finish_summary_poll(started_at);
    let mutation = cache.record_summary_observation(Some(summary), timestamp_label());
    persist_pr_summary_mutation(repo, branch, cache, mutation);
}

pub(crate) fn persist_pr_cache_snapshot(
    repo: &Repository,
    branch: &str,
    cache: &PrCache,
) -> Result<(), String> {
    let Some(summary) = &cache.summary else {
        return remove_pr_cache(repo, branch);
    };
    let summary = encode_summary(branch, summary, cache)?;
    let details = match (&cache.details, &cache.details_association) {
        (Some(details), Some(association)) => Some(encode_details(
            branch,
            details,
            association,
            &cache.details_errors,
            &cache.details_warnings,
        )?),
        (None, Some(association))
            if !cache.details_errors.is_empty() || !cache.details_warnings.is_empty() =>
        {
            Some(encode_details(
                branch,
                &PrDetails::default(),
                association,
                &cache.details_errors,
                &cache.details_warnings,
            )?)
        }
        _ => None,
    };
    persistence::save_snapshot(
        &observability::db_path(repo),
        &summary,
        details.as_ref(),
        unix_seconds(),
    )
    .map_err(|error| format!("write PR cache snapshot: {error}"))
}

pub(super) fn persist_pr_summary_mutation(
    repo: &Repository,
    branch: &str,
    cache: &mut PrCache,
    mutation: super::cache::PrCacheSummaryMutation,
) {
    let result = match mutation {
        super::cache::PrCacheSummaryMutation::SaveSummary => {
            persist_pr_cache_snapshot(repo, branch, cache)
        }
        super::cache::PrCacheSummaryMutation::RemoveSummary => remove_pr_cache(repo, branch),
    };
    cache.record_persistence_result(result);
}

pub(super) fn remove_pr_cache(repo: &Repository, branch: &str) -> Result<(), String> {
    persistence::remove_snapshot(&observability::db_path(repo), branch)
        .map_err(|error| format!("remove PR cache snapshot: {error}"))
}

#[cfg(test)]
pub(crate) fn save_pr_details_cache(
    repo: &Repository,
    branch: &str,
    details: &PrDetails,
) -> Result<(), String> {
    let cache = load_pr_cache_result(repo, branch)?;
    let association = cache
        .summary_identity()
        .ok_or_else(|| "cannot save PR details without a canonical summary identity".to_string())?;
    save_pr_details_cache_for_association(repo, branch, details, &association, &[], &[])
}

pub(super) fn save_pr_details_cache_for_association(
    repo: &Repository,
    branch: &str,
    details: &PrDetails,
    association: &PrDetailsAssociation,
    errors: &[String],
    warnings: &[String],
) -> Result<(), String> {
    let record = encode_details(branch, details, association, errors, warnings)?;
    persistence::save_details(&observability::db_path(repo), &record, unix_seconds())
        .map_err(|error| format!("write PR details cache: {error}"))
}

pub(super) fn persist_observation_errors(repo: &Repository, branch: &str, cache: &mut PrCache) {
    if let Err(error) = persistence::update_summary_error(
        &observability::db_path(repo),
        branch,
        cache.summary_error.as_deref(),
    ) {
        cache.persistence_error = Some(format!("write PR observation error: {error}"));
        cache.rebuild_error();
    }
}

pub fn save_pr_cache(repo: &Repository, branch: &str, cache: &PrCache) -> Result<(), String> {
    let Some(summary) = &cache.summary else {
        return Ok(());
    };
    let record = encode_summary(branch, summary, cache)?;
    persistence::save_summary(&observability::db_path(repo), &record, unix_seconds())
        .map_err(|error| format!("write PR cache: {error}"))
}

#[cfg(test)]
pub(super) fn load_pr_details_cache(repo: &Repository, branch: &str) -> Option<PrDetails> {
    load_pr_cache_result(repo, branch)
        .ok()
        .and_then(|cache| cache.details)
}

pub(crate) fn load_repo_policy_cache_for_identity(
    repo: &Repository,
    repository: &RemoteRepositoryId,
    target_branch: &str,
) -> Option<RepoPolicyCache> {
    let provider = repository.provider();
    persistence::load_policy(
        &observability::db_path(repo),
        provider.config_label(),
        &repository.host().to_string(),
        &repo_policy_project_path_key(provider, repository.project_path()),
        target_branch,
    )
    .ok()
    .flatten()
    .and_then(|record| decode_policy(record).ok())
}

pub(crate) fn save_repo_policy_cache(
    repo: &Repository,
    policy: &RepoPolicyCache,
) -> Result<(), String> {
    let provider = policy
        .provider
        .ok_or_else(|| "repository policy has no provider identity".to_string())?;
    let canonical_host = required_policy_field(policy.canonical_host.as_deref(), "host")?;
    let project_path = required_policy_field(policy.project_path.as_deref(), "project path")?;
    let target_branch = required_policy_field(policy.target_branch.as_deref(), "target branch")?;
    if !policy.identity_complete {
        return Err("repository policy identity is incomplete".to_string());
    }
    let record = PolicyRecord {
        provider: provider.config_label().to_string(),
        canonical_host: canonical_host.to_string(),
        project_path: project_path.to_string(),
        project_path_key: repo_policy_project_path_key(provider, project_path),
        target_branch: target_branch.to_string(),
        default_branch: policy.default_branch.clone(),
        required_approvals: sqlite_i64(policy.required_approvals, "required approvals")?,
        require_conversation_resolution: bool_integer(policy.require_conversation_resolution),
        require_branch_up_to_date: bool_integer(policy.require_branch_up_to_date),
        required_checks: encode_json(&policy.required_checks, "repository policy checks")?,
        merge_queue_required: bool_integer(policy.merge_queue_required),
        refreshed_unix_ms: sqlite_i64(policy.refreshed_unix_ms, "policy refresh time")?,
        error: policy.error.clone(),
    };
    persistence::save_policy(&observability::db_path(repo), &record)
        .map_err(|error| format!("write repository policy cache: {error}"))
}

fn required_policy_field<'a>(value: Option<&'a str>, name: &str) -> Result<&'a str, String> {
    value
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("repository policy has no {name} identity"))
}

fn decode_policy(record: PolicyRecord) -> Result<RepoPolicyCache, String> {
    let provider = parse_provider(&record.provider, "repo_policy_cache.provider")?;
    let expected_key = repo_policy_project_path_key(provider, &record.project_path);
    if record.project_path_key != expected_key {
        return Err(
            "repo_policy_cache.project_path_key does not match its provider identity".into(),
        );
    }
    Ok(RepoPolicyCache {
        repo_remote: record.project_path.clone(),
        default_branch: record.default_branch,
        required_approvals: checked_u64(
            "repo_policy_cache.required_approvals",
            record.required_approvals,
        )?,
        require_conversation_resolution: checked_bool(
            "repo_policy_cache.require_conversation_resolution",
            record.require_conversation_resolution,
        )?,
        require_branch_up_to_date: checked_bool(
            "repo_policy_cache.require_branch_up_to_date",
            record.require_branch_up_to_date,
        )?,
        required_checks: decode_json(&record.required_checks, "repo_policy_cache.required_checks")?,
        merge_queue_required: checked_bool(
            "repo_policy_cache.merge_queue_required",
            record.merge_queue_required,
        )?,
        refreshed_unix_ms: checked_u64(
            "repo_policy_cache.refreshed_unix_ms",
            record.refreshed_unix_ms,
        )?,
        error: record.error,
        provider: Some(provider),
        canonical_host: Some(record.canonical_host),
        project_path: Some(record.project_path),
        target_branch: Some(record.target_branch),
        identity_complete: true,
    })
}

pub(super) fn repo_policy_project_path_key(provider: ProviderKind, project_path: &str) -> String {
    match provider {
        ProviderKind::GitHub => project_path.to_ascii_lowercase(),
        ProviderKind::GitLab | ProviderKind::Forgejo => project_path.to_string(),
    }
}

fn encode_summary(
    branch: &str,
    summary: &PrSummary,
    cache: &PrCache,
) -> Result<SummaryRecord, String> {
    let identity = summary
        .change_request_identity
        .as_ref()
        .ok_or_else(|| "cannot persist a change request without canonical identity".to_string())?;
    Ok(SummaryRecord {
        branch: branch.to_string(),
        number: sqlite_i64(summary.number, "PR number")?,
        provider: identity.provider().config_label().to_string(),
        canonical_host: identity.canonical_host().to_string(),
        project_path: identity.project_path().to_string(),
        native_cr_id: identity.native_id().to_string(),
        display_number: sqlite_i64(summary.number, "PR display number")?,
        source_provider: identity.source_provider().config_label().to_string(),
        source_canonical_host: identity.source_canonical_host().to_string(),
        source_project_path: identity.source_project_path().to_string(),
        target_provider: identity.target_provider().config_label().to_string(),
        target_canonical_host: identity.target_canonical_host().to_string(),
        target_project_path: identity.target_project_path().to_string(),
        title: summary.title.clone(),
        author: summary.author.clone(),
        body: summary.body.clone(),
        url: summary.url.clone(),
        state: summary.state.clone(),
        review_decision: summary.review_decision.clone(),
        requested_reviewers: encode_json(&summary.requested_reviewers, "PR requested reviewers")?,
        head_ref: summary.head_ref.clone(),
        base_ref: summary.base_ref.clone(),
        head_sha: summary.head_sha.clone(),
        updated_at: summary.updated_at.clone(),
        check_status: summary.check_status.clone(),
        merge_state_status: summary.merge_state_status.clone(),
        queue_state: summary.queue_state.clone(),
        comment_count: sqlite_i64(summary.comment_count, "PR comment count")?,
        merged: bool_integer(summary.merged),
        draft: bool_integer(summary.draft),
        last_refreshed: cache.last_refreshed.clone().unwrap_or_default(),
        observation_error: cache.summary_error.clone(),
        native_state_evidence: encode_json(
            &summary.native_state_evidence,
            "PR native state evidence",
        )?,
    })
}

fn decode_summary(record: SummaryRecord) -> Result<(PrSummary, String, Option<String>), String> {
    let identity = decode_identity(
        &record.provider,
        &record.canonical_host,
        &record.project_path,
        &record.native_cr_id,
        &record.source_provider,
        &record.source_canonical_host,
        &record.source_project_path,
        &record.target_provider,
        &record.target_canonical_host,
        &record.target_project_path,
    )?;
    let number = checked_u64("pr_cache.number", record.number)?;
    if checked_u64("pr_cache.display_number", record.display_number)? != number {
        return Err("pr_cache.display_number does not match pr_cache.number".to_string());
    }
    Ok((
        PrSummary {
            number,
            change_request_identity: Some(identity),
            native_state_evidence: decode_json(
                &record.native_state_evidence,
                "pr_cache.native_state_evidence",
            )?,
            title: record.title,
            author: record.author,
            body: record.body,
            url: record.url,
            state: record.state,
            review_decision: record.review_decision,
            requested_reviewers: decode_json(
                &record.requested_reviewers,
                "pr_cache.requested_reviewers",
            )?,
            head_ref: record.head_ref,
            base_ref: record.base_ref,
            head_sha: record.head_sha,
            updated_at: record.updated_at,
            check_status: record.check_status,
            merge_state_status: record.merge_state_status,
            queue_state: record.queue_state,
            comment_count: checked_u64("pr_cache.comment_count", record.comment_count)?,
            merged: checked_bool("pr_cache.merged", record.merged)?,
            draft: checked_bool("pr_cache.draft", record.draft)?,
        },
        record.last_refreshed,
        record.observation_error,
    ))
}

fn encode_details(
    branch: &str,
    details: &PrDetails,
    association: &PrDetailsAssociation,
    errors: &[String],
    warnings: &[String],
) -> Result<DetailsRecord, String> {
    let identity = association
        .change_request_identity
        .as_ref()
        .ok_or_else(|| {
            "cannot persist PR details without canonical summary identity".to_string()
        })?;
    let failures_without_logs: Vec<CiFailure> = details
        .ci_failures
        .iter()
        .cloned()
        .map(|mut failure| {
            failure.log_tail.clear();
            failure
        })
        .collect();
    Ok(DetailsRecord {
        branch: branch.to_string(),
        pr_number: sqlite_i64(association.pr_number, "PR number")?,
        head_sha: association.head_sha.clone(),
        provider: identity.provider().config_label().to_string(),
        canonical_host: identity.canonical_host().to_string(),
        project_path: identity.project_path().to_string(),
        native_cr_id: identity.native_id().to_string(),
        display_number: sqlite_i64(association.pr_number, "PR display number")?,
        source_provider: identity.source_provider().config_label().to_string(),
        source_canonical_host: identity.source_canonical_host().to_string(),
        source_project_path: identity.source_project_path().to_string(),
        target_provider: identity.target_provider().config_label().to_string(),
        target_canonical_host: identity.target_canonical_host().to_string(),
        target_project_path: identity.target_project_path().to_string(),
        comments: encode_json(&details.comments, "PR comments")?,
        reviews: encode_json(&details.reviews, "PR reviews")?,
        review_comments: encode_json(&details.review_comments, "PR review comments")?,
        files: encode_json(&details.files, "PR files")?,
        failing_checks: encode_json(&details.failing_checks, "PR failing checks")?,
        check_contexts: encode_json(&details.check_contexts, "PR check contexts")?,
        ci_failures: encode_json(&failures_without_logs, "PR CI failures")?,
        observation_error: (!errors.is_empty() || !warnings.is_empty()).then(|| {
            errors
                .iter()
                .cloned()
                .chain(warnings.iter().map(|warning| format!("warning:{warning}")))
                .collect::<Vec<_>>()
                .join("\n")
        }),
    })
}

fn decode_details(record: DetailsRecord) -> Result<PersistedPrDetails, String> {
    let identity = decode_identity(
        &record.provider,
        &record.canonical_host,
        &record.project_path,
        &record.native_cr_id,
        &record.source_provider,
        &record.source_canonical_host,
        &record.source_project_path,
        &record.target_provider,
        &record.target_canonical_host,
        &record.target_project_path,
    )?;
    let pr_number = checked_u64("pr_details_cache.pr_number", record.pr_number)?;
    if checked_u64("pr_details_cache.display_number", record.display_number)? != pr_number {
        return Err(
            "pr_details_cache.display_number does not match pr_details_cache.pr_number".into(),
        );
    }
    if record.head_sha.is_empty() {
        return Err("pr_details_cache.head_sha is empty".to_string());
    }
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    for message in record.observation_error.unwrap_or_default().lines() {
        if let Some(warning) = message.strip_prefix("warning:") {
            warnings.push(warning.to_string());
        } else if !message.is_empty() {
            errors.push(message.to_string());
        }
    }
    Ok(PersistedPrDetails {
        details: PrDetails {
            comments: decode_json(&record.comments, "pr_details_cache.comments")?,
            reviews: decode_json(&record.reviews, "pr_details_cache.reviews")?,
            review_comments: decode_json(
                &record.review_comments,
                "pr_details_cache.review_comments",
            )?,
            files: decode_json(&record.files, "pr_details_cache.files")?,
            failing_checks: decode_json(&record.failing_checks, "pr_details_cache.failing_checks")?,
            ci_failures: decode_json(&record.ci_failures, "pr_details_cache.ci_failures")?,
            check_contexts: decode_json(&record.check_contexts, "pr_details_cache.check_contexts")?,
        },
        association: PrDetailsAssociation {
            pr_number,
            head_sha: record.head_sha,
            change_request_identity: Some(identity),
        },
        errors,
        warnings,
    })
}

#[allow(clippy::too_many_arguments)]
fn decode_identity(
    provider: &str,
    canonical_host: &str,
    project_path: &str,
    native_cr_id: &str,
    source_provider: &str,
    source_canonical_host: &str,
    source_project_path: &str,
    target_provider: &str,
    target_canonical_host: &str,
    target_project_path: &str,
) -> Result<CanonicalChangeRequestIdentity, String> {
    let repository = decode_repository(provider, canonical_host, project_path, "repository")?;
    let source = decode_repository(
        source_provider,
        source_canonical_host,
        source_project_path,
        "source repository",
    )?;
    let target = decode_repository(
        target_provider,
        target_canonical_host,
        target_project_path,
        "target repository",
    )?;
    let native_id = NativeChangeRequestId::new(native_cr_id)
        .map_err(|error| format!("invalid persisted change request native ID: {error}"))?;
    Ok(CanonicalChangeRequestIdentity::new(
        &repository,
        &native_id,
        &source,
        &target,
    ))
}

fn decode_repository(
    provider: &str,
    host: &str,
    project_path: &str,
    label: &str,
) -> Result<RemoteRepositoryId, String> {
    let provider = parse_provider(provider, "persisted provider")?;
    let host = HostIdentity::parse(host)
        .map_err(|error| format!("invalid persisted {label} host: {error}"))?;
    RemoteRepositoryId::new(provider, host, project_path)
        .map_err(|error| format!("invalid persisted {label}: {error}"))
}

fn parse_provider(value: &str, field: &str) -> Result<ProviderKind, String> {
    ProviderKind::parse(value)
        .ok_or_else(|| format!("invalid persisted value for {field}: {value}"))
}

fn encode_json<T: Serialize>(value: &T, field: &str) -> Result<String, String> {
    serde_json::to_string(value).map_err(|error| format!("encode {field}: {error}"))
}

fn decode_json<T: DeserializeOwned>(raw: &str, field: &str) -> Result<T, String> {
    serde_json::from_str(raw).map_err(|error| format!("decode {field}: {error}"))
}

fn checked_u64(field: &str, value: i64) -> Result<u64, String> {
    u64::try_from(value).map_err(|_| format!("invalid persisted value for {field}: {value}"))
}

fn checked_bool(field: &str, value: i64) -> Result<bool, String> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(format!("invalid persisted value for {field}: {value}")),
    }
}

fn bool_integer(value: bool) -> i64 {
    i64::from(value)
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
