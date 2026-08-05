use std::path::Path;
use std::str::FromStr;

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{Connection, FromRow, SqliteConnection};

use super::error::DatabaseError;
use super::pools::WorkflowDatabase;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImportSummary {
    pub imported: usize,
    pub already_imported: usize,
}

#[derive(Debug, FromRow)]
struct LegacyRun {
    kind: String,
    id: String,
    repository: String,
    status: String,
    created_unix_ms: i64,
    updated_unix_ms: i64,
}

pub(crate) async fn import_legacy_repository(
    database: &WorkflowDatabase,
    source_path: &Path,
    importer_revision: &str,
    now_unix_ms: i64,
) -> Result<ImportSummary, DatabaseError> {
    let canonical =
        std::fs::canonicalize(source_path).map_err(|source| DatabaseError::Connect {
            path: source_path.to_path_buf(),
            source: sqlx::Error::Io(source),
        })?;
    let options = SqliteConnectOptions::from_str(&canonical.to_string_lossy())
        .map_err(|source| DatabaseError::Connect {
            path: canonical.clone(),
            source,
        })?
        .read_only(true)
        .create_if_missing(false)
        .foreign_keys(true);
    let mut source = SqliteConnection::connect_with(&options)
        .await
        .map_err(|source| DatabaseError::Connect {
            path: canonical.clone(),
            source,
        })?;
    sqlx::query("pragma query_only = on")
        .execute(&mut source)
        .await
        .map_err(DatabaseError::Query)?;
    validate_source(&mut source).await?;

    let protected: i64 = sqlx::query_scalar(
        "select (select count(*) from workflow_execution where dispatch_state in ('queued','claimed','recovery_pending')) + (select count(*) from plan_run where status in ('queued','running','paused')) + (select count(*) from auto_run where status in ('queued','running','paused','waiting'))",
    )
    .fetch_one(&mut source)
    .await
    .map_err(DatabaseError::Query)?;
    if protected != 0 {
        return Err(DatabaseError::Conflict {
            operation: "import active legacy execution",
        });
    }

    let source_schema_version: i64 = sqlx::query_scalar(
        "select coalesce(max(version), 0) from _sqlx_migrations where success = 1",
    )
    .fetch_one(&mut source)
    .await
    .map_err(DatabaseError::Query)?;
    let runs = sqlx::query_as::<_, LegacyRun>(
        "select 'plan' kind, id, repo_root repository, status, created_unix_ms, updated_unix_ms from plan_run union all select 'auto' kind, id, repo_root repository, status, created_unix_ms, updated_unix_ms from auto_run order by kind, id",
    )
    .fetch_all(&mut source)
    .await
    .map_err(DatabaseError::Query)?;
    source.close().await.map_err(DatabaseError::Query)?;

    install_legacy_definitions(database, now_unix_ms).await?;
    let source_identity = canonical.to_string_lossy().into_owned();
    let source_key = format!("{:016x}", crate::util::stable_hash(&canonical));
    let importer_revision = importer_revision.to_string();
    let mut summary = ImportSummary {
        imported: 0,
        already_imported: 0,
    };
    for run in runs {
        let identity = format!("{}:{}", run.kind, run.id);
        let imported_run_id = format!("legacy:{source_key}:{}:{}", run.kind, run.id);
        let source_identity = source_identity.clone();
        let importer_revision = importer_revision.clone();
        let inserted = database
            .write_immediate(|connection| {
                Box::pin(async move {
                    let journaled: i64 = sqlx::query_scalar(
                        "select exists(select 1 from import_journal where source_database_identity = ? and source_schema_version = ? and legacy_run_identity = ? and importer_revision = ? and status = 'completed')",
                    )
                    .bind(&source_identity)
                    .bind(source_schema_version)
                    .bind(&identity)
                    .bind(&importer_revision)
                    .fetch_one(&mut *connection)
                    .await
                    .map_err(DatabaseError::Query)?;
                    if journaled == 1 {
                        return Ok(false);
                    }
                    let definition_id = match run.kind.as_str() {
                        "plan" => "legacy-plan-v1",
                        "auto" => "legacy-auto-v1",
                        _ => {
                            return Err(DatabaseError::InvalidValue {
                                field: "legacy run kind",
                                value: run.kind,
                            });
                        }
                    };
                    let (status, completed_unix_ms) = imported_status(&run.status, run.updated_unix_ms);
                    sqlx::query("insert into workflow_run (id, definition_snapshot_id, repository, status, created_unix_ms, updated_unix_ms, completed_unix_ms) values (?, ?, ?, ?, ?, ?, ?) on conflict(id) do nothing")
                        .bind(&imported_run_id)
                        .bind(definition_id)
                        .bind(&run.repository)
                        .bind(status)
                        .bind(run.created_unix_ms)
                        .bind(run.updated_unix_ms)
                        .bind(completed_unix_ms)
                        .execute(&mut *connection)
                        .await
                        .map_err(DatabaseError::Query)?;
                    sqlx::query("insert into audit_event (run_id, sequence, kind, time_unix_ms, data_json) select ?, 1, 'legacy_imported', ?, ? where not exists (select 1 from audit_event where run_id = ?)")
                        .bind(&imported_run_id)
                        .bind(now_unix_ms)
                        .bind(serde_json::json!({
                            "source_database_identity": source_identity,
                            "source_schema_version": source_schema_version,
                            "legacy_run_identity": identity,
                            "legacy_status": run.status,
                            "importer_revision": importer_revision,
                        }).to_string())
                        .bind(&imported_run_id)
                        .execute(&mut *connection)
                        .await
                        .map_err(DatabaseError::Query)?;
                    sqlx::query("insert into import_journal (source_database_identity, source_schema_version, legacy_run_identity, importer_revision, status, imported_run_id, updated_unix_ms) values (?, ?, ?, ?, 'completed', ?, ?) on conflict(source_database_identity, source_schema_version, legacy_run_identity, importer_revision) do update set status = 'completed', imported_run_id = excluded.imported_run_id, updated_unix_ms = excluded.updated_unix_ms")
                        .bind(&source_identity)
                        .bind(source_schema_version)
                        .bind(&identity)
                        .bind(&importer_revision)
                        .bind(&imported_run_id)
                        .bind(now_unix_ms)
                        .execute(connection)
                        .await
                        .map_err(DatabaseError::Query)?;
                    Ok(true)
                })
            })
            .await?;
        if inserted {
            summary.imported += 1;
        } else {
            summary.already_imported += 1;
        }
    }
    Ok(summary)
}

async fn validate_source(connection: &mut SqliteConnection) -> Result<(), DatabaseError> {
    let quick: Vec<String> = sqlx::query_scalar("pragma quick_check")
        .fetch_all(&mut *connection)
        .await
        .map_err(DatabaseError::Query)?;
    if quick.as_slice() != ["ok"] {
        return Err(DatabaseError::Integrity {
            check: "quick_check",
            details: quick.join("; "),
        });
    }
    let foreign_keys: Vec<(String, i64, String, i64)> = sqlx::query_as("pragma foreign_key_check")
        .fetch_all(&mut *connection)
        .await
        .map_err(DatabaseError::Query)?;
    if !foreign_keys.is_empty() {
        return Err(DatabaseError::Integrity {
            check: "foreign_key_check",
            details: format!("{} violation(s)", foreign_keys.len()),
        });
    }
    Ok(())
}

async fn install_legacy_definitions(
    database: &WorkflowDatabase,
    now_unix_ms: i64,
) -> Result<(), DatabaseError> {
    database
        .write_immediate(|connection| {
            Box::pin(async move {
                for (id, name, digest) in [
                    ("legacy-plan-v1", "legacy-plan-history", "legacy-plan-v1"),
                    ("legacy-auto-v1", "legacy-auto-history", "legacy-auto-v1"),
                ] {
                    sqlx::query("insert into definition_snapshot (id, definition_name, revision, source, trusted, body_json, digest, created_unix_ms) values (?, ?, '1', 'legacy-import', 1, '{\"steps\":[]}', ?, ?) on conflict(id) do nothing")
                        .bind(id)
                        .bind(name)
                        .bind(digest)
                        .bind(now_unix_ms)
                        .execute(&mut *connection)
                        .await
                        .map_err(DatabaseError::Query)?;
                }
                Ok(())
            })
        })
        .await
}

fn imported_status(status: &str, updated_unix_ms: i64) -> (&'static str, Option<i64>) {
    match status {
        "completed" | "succeeded" | "merged" => ("succeeded", Some(updated_unix_ms)),
        "failed" | "error" => ("failed", Some(updated_unix_ms)),
        "cancelled" | "canceled" | "aborted" => ("cancelled", Some(updated_unix_ms)),
        "paused" => ("paused", None),
        _ => ("recovery_required", None),
    }
}
