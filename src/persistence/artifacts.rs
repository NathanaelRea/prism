use super::error::DatabaseError;
use super::pools::WorkflowDatabase;
use super::run_ledger::AttemptLease;

const MAX_INLINE_ARTIFACT_BYTES: usize = 64 * 1024;

#[derive(Clone)]
pub(crate) struct ArtifactStore {
    database: WorkflowDatabase,
}

pub(crate) enum ArtifactBody<'a> {
    Inline(&'a [u8]),
    ContentAddressedFile(&'a str),
}

pub(crate) struct PublishArtifact<'a> {
    pub id: &'a str,
    pub lease: &'a AttemptLease,
    pub revision: i64,
    pub digest: &'a str,
    pub size_bytes: u64,
    pub sensitivity: &'a str,
    pub body: ArtifactBody<'a>,
    pub parents: &'a [String],
    pub now_unix_ms: i64,
}

impl ArtifactStore {
    pub(crate) fn new(database: WorkflowDatabase) -> Self {
        Self { database }
    }

    pub(crate) async fn publish(&self, command: PublishArtifact<'_>) -> Result<(), DatabaseError> {
        if command.revision <= 0 {
            return Err(DatabaseError::InvalidValue {
                field: "artifact revision",
                value: command.revision.to_string(),
            });
        }
        if command.sensitivity.eq_ignore_ascii_case("secret") {
            return Err(DatabaseError::InvalidValue {
                field: "artifact sensitivity",
                value: "secrets cannot be persisted as artifacts".into(),
            });
        }
        let size_bytes =
            i64::try_from(command.size_bytes).map_err(|_| DatabaseError::InvalidValue {
                field: "artifact size",
                value: command.size_bytes.to_string(),
            })?;
        let (inline_body, file_path) = match command.body {
            ArtifactBody::Inline(body) => {
                if body.len() > MAX_INLINE_ARTIFACT_BYTES || body.len() as u64 != command.size_bytes
                {
                    return Err(DatabaseError::InvalidValue {
                        field: "inline artifact body",
                        value: format!(
                            "{} bytes for declared size {} (maximum {})",
                            body.len(),
                            command.size_bytes,
                            MAX_INLINE_ARTIFACT_BYTES
                        ),
                    });
                }
                (Some(body.to_vec()), None)
            }
            ArtifactBody::ContentAddressedFile(path) => {
                if path.is_empty() {
                    return Err(DatabaseError::InvalidValue {
                        field: "artifact file path",
                        value: "empty".into(),
                    });
                }
                (None, Some(path.to_string()))
            }
        };
        let id = command.id.to_string();
        let lease = command.lease.clone();
        let revision = command.revision;
        let digest = command.digest.to_string();
        let sensitivity = command.sensitivity.to_string();
        let parents = command.parents.to_vec();
        let now = command.now_unix_ms;
        self.database
            .write_immediate(|connection| {
                Box::pin(async move {
                    let changed = sqlx::query(
                        "insert into artifact (id, run_id, producing_attempt_id, revision, digest, size_bytes, sensitivity, inline_body, file_path, created_unix_ms) select ?, step.run_id, attempt.id, ?, ?, ?, ?, ?, ?, ? from step_attempt attempt join workflow_step step on step.id = attempt.step_id where attempt.id = ? and attempt.status = 'claimed' and attempt.worker_id = ? and attempt.target_id = ? and attempt.fencing_token = ? and attempt.lease_expires_unix_ms > ?",
                    )
                    .bind(&id)
                    .bind(revision)
                    .bind(&digest)
                    .bind(size_bytes)
                    .bind(&sensitivity)
                    .bind(inline_body)
                    .bind(file_path)
                    .bind(now)
                    .bind(&lease.attempt_id)
                    .bind(&lease.worker_id)
                    .bind(&lease.target_id)
                    .bind(lease.fencing_token)
                    .bind(now)
                    .execute(&mut *connection)
                    .await
                    .map_err(DatabaseError::Query)?
                    .rows_affected();
                    if changed != 1 {
                        return Err(DatabaseError::StaleClaim);
                    }
                    for parent in parents {
                        sqlx::query("insert into artifact_lineage (artifact_id, parent_artifact_id) values (?, ?)")
                            .bind(&id)
                            .bind(parent)
                            .execute(&mut *connection)
                            .await
                            .map_err(DatabaseError::Query)?;
                    }
                    let event_changed = sqlx::query(include_str!(
                        "../../sql/workflow_ledger/append_fenced_event.sql"
                    ))
                    .bind("artifact_published")
                    .bind(now)
                    .bind(serde_json::json!({"artifact_id": id, "digest": digest, "size_bytes": size_bytes}).to_string())
                    .bind(&lease.attempt_id)
                    .bind(&lease.worker_id)
                    .bind(&lease.target_id)
                    .bind(lease.fencing_token)
                    .bind(now)
                    .execute(connection)
                    .await
                    .map_err(DatabaseError::Query)?
                    .rows_affected();
                    if event_changed == 1 {
                        Ok(())
                    } else {
                        Err(DatabaseError::StaleClaim)
                    }
                })
            })
            .await
    }
}
