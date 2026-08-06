use sqlx::FromRow;

use super::error::DatabaseError;
use super::pools::WorkflowDatabase;
use super::run_ledger::AttemptLease;

#[derive(Clone)]
pub(crate) struct EffectBroker {
    database: WorkflowDatabase,
}

pub(crate) struct PrepareEffect<'a> {
    pub id: &'a str,
    pub lease: &'a AttemptLease,
    pub kind: &'a str,
    pub authority_scope: &'a str,
    pub idempotency_key: &'a str,
    pub request_json: &'a str,
    pub now_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub(crate) struct ReconciliationIntent {
    pub id: String,
    pub effect_kind: String,
    pub idempotency_key: String,
    pub request_json: String,
    pub result_json: Option<String>,
}

impl EffectBroker {
    pub(crate) fn new(database: WorkflowDatabase) -> Self {
        Self { database }
    }

    pub(crate) async fn grant_authority(
        &self,
        id: &str,
        run_id: &str,
        scope: &str,
        granted_by: &str,
        now_unix_ms: i64,
        expires_unix_ms: Option<i64>,
    ) -> Result<(), DatabaseError> {
        let (id, run_id, scope, granted_by) = (
            id.to_string(),
            run_id.to_string(),
            scope.to_string(),
            granted_by.to_string(),
        );
        self.database.write_immediate(|connection| Box::pin(async move {
            sqlx::query("insert into authority_grant (id, run_id, scope, granted_by, granted_unix_ms, expires_unix_ms) values (?, ?, ?, ?, ?, ?)")
                .bind(id).bind(run_id).bind(scope).bind(granted_by).bind(now_unix_ms).bind(expires_unix_ms)
                .execute(connection).await.map_err(DatabaseError::Query)?;
            Ok(())
        })).await
    }

    pub(crate) async fn prepare(
        &self,
        command: PrepareEffect<'_>,
    ) -> Result<String, DatabaseError> {
        let id = command.id.to_string();
        let lease = command.lease.clone();
        let kind = command.kind.to_string();
        let scope = command.authority_scope.to_string();
        let key = command.idempotency_key.to_string();
        let request = command.request_json.to_string();
        let now = command.now_unix_ms;
        self.database
            .write_immediate(|connection| {
                Box::pin(async move {
                    let inserted: Option<String> = sqlx::query_scalar(include_str!(
                        "../../sql/workflow_ledger/prepare_effect.sql"
                    ))
                    .bind(&id)
                    .bind(&kind)
                    .bind(&key)
                    .bind(&request)
                    .bind(now)
                    .bind(now)
                    .bind(&lease.attempt_id)
                    .bind(&lease.worker_id)
                    .bind(&lease.target_id)
                    .bind(lease.fencing_token)
                    .bind(now)
                    .bind(&scope)
                    .bind(now)
                    .fetch_optional(&mut *connection)
                    .await
                    .map_err(DatabaseError::Query)?;
                    if let Some(id) = inserted {
                        return Ok(id);
                    }
                    let existing: Option<(String, String, String)> = sqlx::query_as(
                "select id, effect_kind, request_json from effect_intent where idempotency_key = ?",
            ).bind(&key).fetch_optional(&mut *connection).await.map_err(DatabaseError::Query)?;
                    match existing {
                        Some((id, existing_kind, existing_request))
                            if existing_kind == kind && existing_request == request =>
                        {
                            Ok(id)
                        }
                        Some(_) => Err(DatabaseError::Conflict {
                            operation: "prepare effect idempotency",
                        }),
                        None => Err(DatabaseError::StaleClaim),
                    }
                })
            })
            .await
    }

    pub(crate) async fn mark_dispatching(
        &self,
        effect_id: &str,
        lease: &AttemptLease,
        now_unix_ms: i64,
    ) -> Result<(), DatabaseError> {
        let id = effect_id.to_string();
        let lease = lease.clone();
        self.database.write_immediate(|connection| Box::pin(async move {
            let changed = sqlx::query("update effect_intent set status = 'dispatching', updated_unix_ms = ? where id = ? and status = 'prepared' and exists (select 1 from step_attempt attempt where attempt.id = effect_intent.attempt_id and attempt.status = 'claimed' and attempt.worker_id = ? and attempt.target_id = ? and attempt.fencing_token = ? and attempt.lease_expires_unix_ms > ?)")
                .bind(now_unix_ms).bind(id).bind(lease.worker_id).bind(lease.target_id)
                .bind(lease.fencing_token).bind(now_unix_ms).execute(connection).await
                .map_err(DatabaseError::Query)?.rows_affected();
            if changed == 1 { Ok(()) } else { Err(DatabaseError::StaleClaim) }
        })).await
    }

    /// Records an authoritative result only under the current lease. A late result is made
    /// explicitly indeterminate so reconciliation can recover it.
    pub(crate) async fn record_result(
        &self,
        effect_id: &str,
        lease: &AttemptLease,
        succeeded: bool,
        result_json: &str,
        now_unix_ms: i64,
    ) -> Result<bool, DatabaseError> {
        let id = effect_id.to_string();
        let lease = lease.clone();
        let result = result_json.to_string();
        self.database.write_immediate(|connection| Box::pin(async move {
            let status = if succeeded { "succeeded" } else { "failed" };
            let changed = sqlx::query(include_str!("../../sql/workflow_ledger/record_effect_result.sql"))
                .bind(status).bind(&result).bind(now_unix_ms).bind(&id).bind(&lease.worker_id)
                .bind(&lease.target_id).bind(lease.fencing_token).bind(now_unix_ms)
                .execute(&mut *connection).await.map_err(DatabaseError::Query)?.rows_affected();
            if changed == 1 { return Ok(true); }
            let indeterminate = sqlx::query("update effect_intent set status = 'indeterminate', result_json = ?, updated_unix_ms = ? where id = ? and status in ('prepared','dispatching')")
                .bind(&result).bind(now_unix_ms).bind(&id).execute(connection).await
                .map_err(DatabaseError::Query)?.rows_affected();
            if indeterminate == 1 { Ok(false) } else { Err(DatabaseError::Conflict { operation: "record effect result" }) }
        })).await
    }

    pub(crate) async fn record_reconciliation(
        &self,
        effect_id: &str,
        succeeded: bool,
        result_json: &str,
        now_unix_ms: i64,
    ) -> Result<(), DatabaseError> {
        let id = effect_id.to_string();
        let result = result_json.to_string();
        self.database
            .write_immediate(|connection| {
                Box::pin(async move {
                    let status = if succeeded { "succeeded" } else { "failed" };
                    let run_id: Option<String> = sqlx::query_scalar(
                        "update effect_intent set status = ?, result_json = ?, updated_unix_ms = ? where id = ? and status = 'indeterminate' returning run_id",
                    )
                    .bind(status)
                    .bind(&result)
                    .bind(now_unix_ms)
                    .bind(&id)
                    .fetch_optional(&mut *connection)
                    .await
                    .map_err(DatabaseError::Query)?;
                    let Some(run_id) = run_id else {
                        return Err(DatabaseError::Conflict {
                            operation: "reconcile effect",
                        });
                    };
                    sqlx::query("insert into audit_event (run_id, sequence, kind, time_unix_ms, data_json) select ?, coalesce(max(sequence), 0) + 1, 'effect_reconciled', ?, ? from audit_event where run_id = ?")
                        .bind(&run_id)
                        .bind(now_unix_ms)
                        .bind(serde_json::json!({"effect_id": id, "status": status}).to_string())
                        .bind(&run_id)
                        .execute(connection)
                        .await
                        .map_err(DatabaseError::Query)?;
                    Ok(())
                })
            })
            .await
    }

    pub(crate) async fn reconciliation_required(
        &self,
        limit: usize,
    ) -> Result<Vec<ReconciliationIntent>, DatabaseError> {
        let limit = i64::try_from(limit).map_err(|_| DatabaseError::InvalidValue {
            field: "effect reconciliation limit",
            value: limit.to_string(),
        })?;
        sqlx::query_as("select id, effect_kind, idempotency_key, request_json, result_json from effect_intent where status = 'indeterminate' order by updated_unix_ms, id limit ?")
            .bind(limit).fetch_all(self.database.readers()).await.map_err(DatabaseError::Query)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::persistence::run_ledger::{ClaimRequest, Coordinator};

    static SEQUENCE: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn effect_results_require_authority_and_current_fence() {
        let path = std::env::temp_dir().join(format!(
            "prism-effects-{}-{}.db",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap().block_on(async {
            let database = WorkflowDatabase::open(&path).await.unwrap();
            database.write_immediate(|connection| Box::pin(async move {
                sqlx::query("insert into definition_snapshot (id, definition_name, revision, source, trusted, body_json, digest, created_unix_ms) values ('definition', 'test', '1', 'test', 1, '{}', 'digest', 1)").execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                sqlx::query("insert into workflow_run (id, definition_snapshot_id, status, created_unix_ms, updated_unix_ms) values ('run', 'definition', 'runnable', 1, 1)").execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                sqlx::query("insert into workflow_step (id, run_id, step_key, implementation, target_id, status, available_unix_ms, input_json) values ('step', 'run', 'step', 'fake', 'local', 'runnable', 1, '{}')").execute(connection).await.map_err(DatabaseError::Query)?;
                Ok(())
            })).await.unwrap();
            let lease = Coordinator::new(database.clone()).claim(ClaimRequest {
                attempt_id: "attempt", step_id: "step", worker_id: "worker", now_unix_ms: 2, lease_expires_unix_ms: 5,
            }).await.unwrap().unwrap();
            let broker = EffectBroker::new(database.clone());
            assert!(matches!(broker.prepare(PrepareEffect {
                id: "effect", lease: &lease, kind: "remote", authority_scope: "remote:write",
                idempotency_key: "key", request_json: "{}", now_unix_ms: 3,
            }).await, Err(DatabaseError::StaleClaim)));
            broker.grant_authority("grant", "run", "remote:write", "user", 3, None).await.unwrap();
            assert_eq!(broker.prepare(PrepareEffect {
                id: "effect", lease: &lease, kind: "remote", authority_scope: "remote:write",
                idempotency_key: "key", request_json: "{}", now_unix_ms: 3,
            }).await.unwrap(), "effect");
            broker.mark_dispatching("effect", &lease, 4).await.unwrap();
            assert!(!broker.record_result("effect", &lease, true, "{}", 6).await.unwrap());
            assert_eq!(broker.reconciliation_required(10).await.unwrap().len(), 1);
            broker.record_reconciliation("effect", true, r#"{"remote":"confirmed"}"#, 7).await.unwrap();
            assert!(broker.reconciliation_required(10).await.unwrap().is_empty());
            database.close().await;
        });
        let _ = std::fs::remove_file(path);
    }
}
