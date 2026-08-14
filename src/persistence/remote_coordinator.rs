use std::path::Path;

use crate::remote::request_coordinator::{
    PersistedRemoteLane, PersistedRemoteMutation, PersistedRemoteMutationState,
    RemoteCoordinatorError, RemoteCoordinatorStore, RemoteFuture, RemoteLaneKey,
};

#[derive(Clone)]
pub struct SqliteRemoteCoordinatorStore {
    pool: sqlx::SqlitePool,
}

impl SqliteRemoteCoordinatorStore {
    pub async fn open(path: &Path) -> Result<Self, RemoteCoordinatorError> {
        // The numbered prompt-workflow migrator is the sole production schema authority. Opening
        // this adapter standalone first initializes the shared Workflow database, so a later
        // Workflow store open preserves the same ledger and constraints.
        let workflow = super::workflow_kernel::DurableWorkflowRunStore::open(path)
            .await
            .map_err(|error| RemoteCoordinatorError::Persistence(error.to_string()))?;
        workflow.close().await;
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                super::pools::options(path, true, false)
                    .map_err(|error| RemoteCoordinatorError::Persistence(error.to_string()))?,
            )
            .await
            .map_err(persistence)?;
        Ok(Self { pool })
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }
}

impl RemoteCoordinatorStore for SqliteRemoteCoordinatorStore {
    fn load_lanes<'a>(&'a self) -> RemoteFuture<'a, Vec<PersistedRemoteLane>> {
        Box::pin(async move {
            let rows = sqlx::query_as::<_, (String, String, i64, i64, i64)>(
                "select canonical_host, credential_profile, next_request_unix_ms, retry_count, updated_unix_ms from remote_lane_cooldown",
            )
            .fetch_all(&self.pool)
            .await
            .map_err(persistence)?;
            rows.into_iter()
                .map(
                    |(host, profile, next_request_unix_ms, retry_count, updated_unix_ms)| {
                        Ok(PersistedRemoteLane {
                            key: RemoteLaneKey::new(host, profile)?,
                            next_request_unix_ms,
                            retry_count: u32::try_from(retry_count).map_err(|_| {
                                RemoteCoordinatorError::Persistence(
                                    "remote lane retry count is out of range".into(),
                                )
                            })?,
                            updated_unix_ms,
                        })
                    },
                )
                .collect()
        })
    }

    fn save_lane<'a>(&'a self, lane: &'a PersistedRemoteLane) -> RemoteFuture<'a, ()> {
        Box::pin(async move {
            sqlx::query("insert into remote_lane_cooldown(canonical_host, credential_profile, next_request_unix_ms, retry_count, updated_unix_ms) values(?,?,?,?,?) on conflict(canonical_host, credential_profile) do update set next_request_unix_ms=excluded.next_request_unix_ms, retry_count=excluded.retry_count, updated_unix_ms=excluded.updated_unix_ms")
                .bind(&lane.key.canonical_host)
                .bind(&lane.key.credential_profile)
                .bind(lane.next_request_unix_ms)
                .bind(i64::from(lane.retry_count))
                .bind(lane.updated_unix_ms)
                .execute(&self.pool)
                .await
                .map_err(persistence)?;
            Ok(())
        })
    }

    fn load_mutation<'a>(
        &'a self,
        lane: &'a RemoteLaneKey,
        request_id: &'a str,
    ) -> RemoteFuture<'a, Option<PersistedRemoteMutation>> {
        Box::pin(async move {
            let row = sqlx::query_as::<_, (String, String, Option<String>, Option<String>, i64)>(
                "select request_fingerprint, state, outcome_json, reason, updated_unix_ms from remote_mutation_ledger where canonical_host=? and credential_profile=? and request_id=?",
            )
            .bind(&lane.canonical_host)
            .bind(&lane.credential_profile)
            .bind(request_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(persistence)?;
            row.map(|(fingerprint, state, outcome, reason, updated)| {
                decode_mutation(
                    lane,
                    request_id,
                    fingerprint,
                    state,
                    outcome,
                    reason,
                    updated,
                )
            })
            .transpose()
        })
    }

    fn claim_mutation<'a>(
        &'a self,
        mutation: &'a PersistedRemoteMutation,
    ) -> RemoteFuture<'a, Option<PersistedRemoteMutation>> {
        Box::pin(async move {
            let changed = sqlx::query("insert into remote_mutation_ledger(canonical_host, credential_profile, request_id, request_fingerprint, state, outcome_json, reason, updated_unix_ms) values(?,?,?,?, 'claimed', null, null, ?) on conflict(canonical_host, credential_profile, request_id) do nothing")
                .bind(&mutation.lane.canonical_host)
                .bind(&mutation.lane.credential_profile)
                .bind(&mutation.request_id)
                .bind(&mutation.request_fingerprint)
                .bind(mutation.updated_unix_ms)
                .execute(&self.pool)
                .await
                .map_err(persistence)?
                .rows_affected();
            if changed == 1 {
                Ok(None)
            } else {
                self.load_mutation(&mutation.lane, &mutation.request_id)
                    .await
            }
        })
    }

    fn save_mutation<'a>(&'a self, mutation: &'a PersistedRemoteMutation) -> RemoteFuture<'a, ()> {
        Box::pin(async move {
            let (state, outcome, reason) = encode_mutation_state(&mutation.state)?;
            let allowed_previous = match mutation.state {
                PersistedRemoteMutationState::Uncertain { .. } => ["claimed", "uncertain"],
                PersistedRemoteMutationState::Applied { .. }
                | PersistedRemoteMutationState::Failed { .. } => ["uncertain", "uncertain"],
                PersistedRemoteMutationState::Claimed => ["", ""],
            };
            let changed = sqlx::query("update remote_mutation_ledger set state=?, outcome_json=?, reason=?, updated_unix_ms=? where canonical_host=? and credential_profile=? and request_id=? and request_fingerprint=? and state in (?,?)")
                .bind(state)
                .bind(outcome)
                .bind(reason)
                .bind(mutation.updated_unix_ms)
                .bind(&mutation.lane.canonical_host)
                .bind(&mutation.lane.credential_profile)
                .bind(&mutation.request_id)
                .bind(&mutation.request_fingerprint)
                .bind(allowed_previous[0])
                .bind(allowed_previous[1])
                .execute(&self.pool)
                .await
                .map_err(persistence)?
                .rows_affected();
            if changed != 1 {
                return Err(RemoteCoordinatorError::Persistence(
                    "remote mutation terminal state did not match its durable claim".into(),
                ));
            }
            Ok(())
        })
    }
}

fn encode_mutation_state(
    state: &PersistedRemoteMutationState,
) -> Result<(&'static str, Option<String>, Option<String>), RemoteCoordinatorError> {
    match state {
        PersistedRemoteMutationState::Claimed => Ok(("claimed", None, None)),
        PersistedRemoteMutationState::Uncertain { reason } => {
            Ok(("uncertain", None, Some(reason.clone())))
        }
        PersistedRemoteMutationState::Applied { value } => Ok((
            "applied",
            Some(
                serde_json::to_string(value)
                    .map_err(|error| RemoteCoordinatorError::Persistence(error.to_string()))?,
            ),
            None,
        )),
        PersistedRemoteMutationState::Failed { reason } => {
            Ok(("failed", None, Some(reason.clone())))
        }
    }
}

fn decode_mutation(
    lane: &RemoteLaneKey,
    request_id: &str,
    request_fingerprint: String,
    state: String,
    outcome: Option<String>,
    reason: Option<String>,
    updated_unix_ms: i64,
) -> Result<PersistedRemoteMutation, RemoteCoordinatorError> {
    let state = match state.as_str() {
        "claimed" => PersistedRemoteMutationState::Claimed,
        "uncertain" => PersistedRemoteMutationState::Uncertain {
            reason: reason
                .ok_or_else(|| invalid_mutation_row("uncertain mutation has no reason"))?,
        },
        "applied" => PersistedRemoteMutationState::Applied {
            value: serde_json::from_str(
                outcome
                    .as_deref()
                    .ok_or_else(|| invalid_mutation_row("applied mutation has no outcome"))?,
            )
            .map_err(|error| invalid_mutation_row(&error.to_string()))?,
        },
        "failed" => PersistedRemoteMutationState::Failed {
            reason: reason.ok_or_else(|| invalid_mutation_row("failed mutation has no reason"))?,
        },
        other => {
            return Err(invalid_mutation_row(&format!(
                "unknown mutation state {other}"
            )));
        }
    };
    Ok(PersistedRemoteMutation {
        lane: lane.clone(),
        request_id: request_id.to_string(),
        request_fingerprint,
        state,
        updated_unix_ms,
    })
}

fn invalid_mutation_row(reason: &str) -> RemoteCoordinatorError {
    RemoteCoordinatorError::Persistence(format!("invalid remote mutation ledger row: {reason}"))
}

fn persistence(error: sqlx::Error) -> RemoteCoordinatorError {
    RemoteCoordinatorError::Persistence(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mutation_ledger_survives_store_restart() {
        let root = std::env::temp_dir().join(format!(
            "prism-remote-ledger-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = root.join("workflow.db");
        let lane = RemoteLaneKey::new("github.com", "default").unwrap();
        let claim = PersistedRemoteMutation {
            lane: lane.clone(),
            request_id: "request".into(),
            request_fingerprint: "sha256:test".into(),
            state: PersistedRemoteMutationState::Claimed,
            updated_unix_ms: 1,
        };
        let first = SqliteRemoteCoordinatorStore::open(&path).await.unwrap();
        assert!(first.claim_mutation(&claim).await.unwrap().is_none());
        let uncertain = PersistedRemoteMutation {
            state: PersistedRemoteMutationState::Uncertain {
                reason: "dispatched".into(),
            },
            updated_unix_ms: 2,
            ..claim.clone()
        };
        first.save_mutation(&uncertain).await.unwrap();
        let applied = PersistedRemoteMutation {
            state: PersistedRemoteMutationState::Applied {
                value: serde_json::json!({"merged": true}),
            },
            updated_unix_ms: 3,
            ..claim.clone()
        };
        first.save_mutation(&applied).await.unwrap();
        first.close().await;

        let workflow = super::super::workflow_kernel::DurableWorkflowRunStore::open(&path)
            .await
            .unwrap();
        workflow.close().await;
        let second = SqliteRemoteCoordinatorStore::open(&path).await.unwrap();
        assert_eq!(
            second.load_mutation(&lane, "request").await.unwrap(),
            Some(applied)
        );
        second.close().await;
        let _ = std::fs::remove_dir_all(root);
    }
}
