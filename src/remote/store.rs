use rusqlite::{OptionalExtension, params};

use crate::observability;
use crate::repo::Repository;
use crate::util::timestamp_label;

use super::cache::{
    CiFailure, PrCache, PrCheckContext, PrComment, PrDetails, PrDetailsAssociation,
    PrObservationQuality, PrReview, PrReviewComment, PrSummary, RepoPolicyCache,
};

struct PersistedPrDetails {
    details: PrDetails,
    association: Option<PrDetailsAssociation>,
    errors: Vec<String>,
    warnings: Vec<String>,
}

pub(crate) fn load_pr_cache(repo: &Repository, branch: &str) -> PrCache {
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

pub(super) fn persist_pr_summary_mutation(
    repo: &Repository,
    branch: &str,
    cache: &mut PrCache,
    mutation: super::cache::PrCacheSummaryMutation,
) {
    let result = match mutation {
        super::cache::PrCacheSummaryMutation::SaveSummary => save_pr_cache(repo, branch, cache)
            .and_then(|()| {
                if let (Some(details), Some(association)) =
                    (&cache.details, &cache.details_association)
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
        super::cache::PrCacheSummaryMutation::RemoveSummary => remove_pr_cache(repo, branch),
    };
    cache.record_persistence_result(result);
}

pub(super) fn remove_pr_cache(repo: &Repository, branch: &str) -> Result<(), String> {
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

pub(super) fn save_pr_details_cache_for_association(
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

pub(super) fn persist_observation_errors(repo: &Repository, branch: &str, cache: &mut PrCache) {
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
pub(super) fn load_pr_details_cache(repo: &Repository, branch: &str) -> Option<PrDetails> {
    load_pr_details_cache_record(repo, branch)
        .ok()
        .flatten()
        .map(|record| record.details)
}

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

pub(super) fn repo_policy_project_path_key(
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

fn encode_string_values(values: &[String]) -> String {
    serde_json::to_string(values).unwrap_or_else(|_| "[]".to_string())
}

fn decode_string_values(raw: &str) -> Vec<String> {
    serde_json::from_str(raw).unwrap_or_default()
}

fn decode_native_state_evidence(raw: &str) -> crate::remote::NativeStateEvidence {
    serde_json::from_str(raw).unwrap_or_default()
}

fn encode_native_state_evidence(evidence: &crate::remote::NativeStateEvidence) -> String {
    serde_json::to_string(evidence).unwrap_or_else(|_| "{}".to_string())
}

fn encode_requested_reviewers(reviewers: &[String]) -> String {
    reviewers.join("\n")
}

fn encode_pr_comments(comments: &[PrComment]) -> String {
    serde_json::to_string(comments).unwrap_or_else(|_| "[]".to_string())
}

fn encode_pr_reviews(reviews: &[PrReview]) -> String {
    serde_json::to_string(reviews).unwrap_or_else(|_| "[]".to_string())
}

fn encode_pr_review_comments(comments: &[PrReviewComment]) -> String {
    serde_json::to_string(comments).unwrap_or_else(|_| "[]".to_string())
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

fn encode_check_contexts(contexts: &[PrCheckContext]) -> String {
    serde_json::to_string(contexts).unwrap_or_else(|_| "[]".to_string())
}

fn decode_requested_reviewers(value: &str) -> Vec<String> {
    value
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

fn decode_pr_comments(raw: &str) -> Vec<PrComment> {
    serde_json::from_str(raw).unwrap_or_default()
}

fn decode_pr_reviews(raw: &str) -> Vec<PrReview> {
    serde_json::from_str(raw).unwrap_or_default()
}

fn decode_pr_review_comments(raw: &str) -> Vec<PrReviewComment> {
    serde_json::from_str(raw).unwrap_or_default()
}

fn decode_ci_failures(raw: &str) -> Vec<CiFailure> {
    serde_json::from_str(raw).unwrap_or_default()
}

fn decode_check_contexts(raw: &str) -> Vec<PrCheckContext> {
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
