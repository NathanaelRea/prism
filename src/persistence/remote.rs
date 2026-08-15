use std::path::Path;

use sqlx::SqliteConnection;

use super::database::{block_on, finish_connection, open_writable};
use super::error::DatabaseError;

#[derive(Clone, Debug)]
pub(crate) struct SummaryRecord {
    pub branch: String,
    pub number: i64,
    pub provider: String,
    pub canonical_host: String,
    pub project_path: String,
    pub native_cr_id: String,
    pub display_number: i64,
    pub source_provider: String,
    pub source_canonical_host: String,
    pub source_project_path: String,
    pub target_provider: String,
    pub target_canonical_host: String,
    pub target_project_path: String,
    pub title: String,
    pub author: String,
    pub body: String,
    pub url: String,
    pub state: String,
    pub review_decision: String,
    pub requested_reviewers: String,
    pub head_ref: String,
    pub base_ref: String,
    pub head_sha: String,
    pub updated_at: String,
    pub check_status: String,
    pub merge_state_status: String,
    pub queue_state: String,
    pub comment_count: i64,
    pub merged: i64,
    pub draft: i64,
    pub last_refreshed: String,
    pub observation_error: Option<String>,
    pub native_state_evidence: String,
}

#[derive(Clone, Debug)]
pub(crate) struct DetailsRecord {
    pub branch: String,
    pub pr_number: i64,
    pub head_sha: String,
    pub provider: String,
    pub canonical_host: String,
    pub project_path: String,
    pub native_cr_id: String,
    pub display_number: i64,
    pub source_provider: String,
    pub source_canonical_host: String,
    pub source_project_path: String,
    pub target_provider: String,
    pub target_canonical_host: String,
    pub target_project_path: String,
    pub comments: String,
    pub reviews: String,
    pub review_comments: String,
    pub files: String,
    pub failing_checks: String,
    pub check_contexts: String,
    pub ci_failures: String,
    pub observation_error: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct PolicyRecord {
    pub provider: String,
    pub canonical_host: String,
    pub project_path: String,
    pub project_path_key: String,
    pub target_branch: String,
    pub default_branch: Option<String>,
    pub required_approvals: i64,
    pub require_conversation_resolution: i64,
    pub require_branch_up_to_date: i64,
    pub required_checks: String,
    pub merge_queue_required: i64,
    pub refreshed_unix_ms: i64,
    pub error: Option<String>,
}

async fn rollback(connection: &mut SqliteConnection) {
    let _ = super::database::rollback_query().execute(connection).await;
}

fn with_connection<T>(
    path: &Path,
    operation: impl FnOnce(&mut SqliteConnection) -> Result<T, DatabaseError>,
) -> Result<T, DatabaseError> {
    let mut connection = open_writable(path)?;
    let result = operation(&mut connection);
    finish_connection(connection, result)
}

pub(crate) fn load_snapshot(
    path: &Path,
    branch: &str,
) -> Result<(Option<SummaryRecord>, Option<DetailsRecord>), DatabaseError> {
    with_connection(path, |connection| {
        block_on(async {
            super::database::begin_immediate_query()
                .execute(&mut *connection)
                .await?;
            let result = async {
                let summary =
                    sqlx::query_file_as!(SummaryRecord, "sql/remote/load_pr_summary.sql", branch)
                        .fetch_optional(&mut *connection)
                        .await?;
                let details =
                    sqlx::query_file_as!(DetailsRecord, "sql/remote/load_pr_details.sql", branch)
                        .fetch_optional(&mut *connection)
                        .await?;
                super::database::commit_query()
                    .execute(&mut *connection)
                    .await?;
                Ok((summary, details))
            }
            .await;
            if result.is_err() {
                rollback(connection).await;
            }
            result
        })
    })
}

async fn upsert_summary(
    connection: &mut SqliteConnection,
    record: &SummaryRecord,
    refreshed_unix_ms: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query_file!(
        "sql/remote/upsert_pr_summary.sql",
        record.branch,
        record.number,
        record.provider,
        record.canonical_host,
        record.project_path,
        record.native_cr_id,
        record.display_number,
        record.source_provider,
        record.source_canonical_host,
        record.source_project_path,
        record.target_provider,
        record.target_canonical_host,
        record.target_project_path,
        record.title,
        record.author,
        record.body,
        record.url,
        record.state,
        record.review_decision,
        record.requested_reviewers,
        record.head_ref,
        record.base_ref,
        record.head_sha,
        record.updated_at,
        record.check_status,
        record.merge_state_status,
        record.queue_state,
        record.comment_count,
        record.merged,
        record.draft,
        record.last_refreshed,
        refreshed_unix_ms,
        record.observation_error,
        record.native_state_evidence,
    )
    .execute(connection)
    .await?;
    Ok(())
}

async fn upsert_details(
    connection: &mut SqliteConnection,
    record: &DetailsRecord,
    refreshed_unix_ms: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query_file!(
        "sql/remote/upsert_pr_details.sql",
        record.branch,
        record.pr_number,
        record.head_sha,
        record.provider,
        record.canonical_host,
        record.project_path,
        record.native_cr_id,
        record.display_number,
        record.source_provider,
        record.source_canonical_host,
        record.source_project_path,
        record.target_provider,
        record.target_canonical_host,
        record.target_project_path,
        record.comments,
        record.reviews,
        record.review_comments,
        record.files,
        record.failing_checks,
        record.check_contexts,
        record.ci_failures,
        refreshed_unix_ms,
        record.observation_error,
    )
    .execute(connection)
    .await?;
    Ok(())
}

pub(crate) fn save_snapshot(
    path: &Path,
    summary: &SummaryRecord,
    details: Option<&DetailsRecord>,
    refreshed_unix_ms: i64,
) -> Result<(), DatabaseError> {
    with_connection(path, |connection| {
        block_on(async {
            super::database::begin_immediate_query()
                .execute(&mut *connection)
                .await?;
            let result = async {
                upsert_summary(connection, summary, refreshed_unix_ms).await?;
                if let Some(details) = details {
                    upsert_details(connection, details, refreshed_unix_ms).await?;
                } else {
                    sqlx::query_file!("sql/remote/delete_pr_details.sql", summary.branch)
                        .execute(&mut *connection)
                        .await?;
                }
                super::database::commit_query()
                    .execute(&mut *connection)
                    .await?;
                Ok(())
            }
            .await;
            if result.is_err() {
                rollback(connection).await;
            }
            result
        })
    })
}

pub(crate) fn save_summary(
    path: &Path,
    summary: &SummaryRecord,
    refreshed_unix_ms: i64,
) -> Result<(), DatabaseError> {
    with_connection(path, |connection| {
        block_on(upsert_summary(connection, summary, refreshed_unix_ms))
    })
}

pub(crate) fn save_details(
    path: &Path,
    details: &DetailsRecord,
    refreshed_unix_ms: i64,
) -> Result<(), DatabaseError> {
    with_connection(path, |connection| {
        block_on(upsert_details(connection, details, refreshed_unix_ms))
    })
}

pub(crate) fn remove_snapshot(path: &Path, branch: &str) -> Result<(), DatabaseError> {
    with_connection(path, |connection| {
        block_on(async {
            super::database::begin_immediate_query()
                .execute(&mut *connection)
                .await?;
            let result = async {
                sqlx::query_file!("sql/remote/delete_pr_details.sql", branch)
                    .execute(&mut *connection)
                    .await?;
                sqlx::query_file!("sql/remote/delete_pr_summary.sql", branch)
                    .execute(&mut *connection)
                    .await?;
                super::database::commit_query()
                    .execute(&mut *connection)
                    .await?;
                Ok(())
            }
            .await;
            if result.is_err() {
                rollback(connection).await;
            }
            result
        })
    })
}

pub(crate) fn update_summary_error(
    path: &Path,
    branch: &str,
    error: Option<&str>,
) -> Result<(), DatabaseError> {
    with_connection(path, |connection| {
        block_on(async {
            sqlx::query_file!("sql/remote/update_pr_observation_error.sql", error, branch)
                .execute(&mut *connection)
                .await?;
            Ok(())
        })
    })
}

pub(crate) fn load_policy(
    path: &Path,
    provider: &str,
    canonical_host: &str,
    project_path_key: &str,
    target_branch: &str,
) -> Result<Option<PolicyRecord>, DatabaseError> {
    with_connection(path, |connection| {
        block_on(async {
            sqlx::query_file_as!(
                PolicyRecord,
                "sql/remote/load_policy.sql",
                provider,
                canonical_host,
                project_path_key,
                target_branch,
            )
            .fetch_optional(&mut *connection)
            .await
        })
    })
}

pub(crate) fn save_policy(path: &Path, policy: &PolicyRecord) -> Result<(), DatabaseError> {
    with_connection(path, |connection| {
        block_on(async {
            sqlx::query_file!(
                "sql/remote/upsert_policy.sql",
                policy.provider,
                policy.canonical_host,
                policy.project_path,
                policy.project_path_key,
                policy.target_branch,
                policy.default_branch,
                policy.required_approvals,
                policy.require_conversation_resolution,
                policy.require_branch_up_to_date,
                policy.required_checks,
                policy.merge_queue_required,
                policy.refreshed_unix_ms,
                policy.error,
            )
            .execute(&mut *connection)
            .await?;
            Ok(())
        })
    })
}
