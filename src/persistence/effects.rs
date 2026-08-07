use prism_extension_protocol::{BrokeredEffectRequest, ProtocolError};
use sqlx::FromRow;

use super::error::DatabaseError;
use super::pools::WorkflowDatabase;
use super::run_ledger::AttemptLease;

#[derive(Clone)]
pub(crate) struct EffectBroker {
    database: WorkflowDatabase,
}

/// Durable production adapter used by the public Standard host-operation broker.
#[derive(Clone)]
pub(crate) struct WorkflowEffectLedger {
    database: WorkflowDatabase,
    broker: EffectBroker,
}

impl WorkflowEffectLedger {
    pub(crate) fn new(database: WorkflowDatabase) -> Self {
        Self {
            broker: EffectBroker::new(database.clone()),
            database,
        }
    }

    async fn attempt_lease(
        &self,
        attempt_id: &str,
        generation: u64,
    ) -> Result<AttemptLease, ProtocolError> {
        let generation = i64::try_from(generation)
            .map_err(|_| ProtocolError::new("invalid_generation", "generation is too large"))?;
        let row: Option<(String, String, String, i64, i64, String)> = sqlx::query_as(
            "select attempt.step_id, attempt.worker_id, attempt.target_id, attempt.fencing_token, attempt.lease_expires_unix_ms, step.class from step_attempt attempt join workflow_step step on step.id = attempt.step_id where attempt.id = ? and attempt.status = 'claimed'",
        )
        .bind(attempt_id)
        .fetch_optional(self.database.readers())
        .await
        .map_err(protocol_database_error)?;
        let Some((step_id, worker_id, target_id, fencing_token, lease_expires_unix_ms, class)) =
            row
        else {
            return Err(ProtocolError::new(
                "stale_attempt",
                "Attempt is not actively claimed",
            ));
        };
        if class != "action" {
            return Err(ProtocolError::new(
                "class_forbidden",
                format!("{class} Steps cannot invoke protected mutation host operations"),
            ));
        }
        if fencing_token != generation {
            return Err(ProtocolError::new(
                "stale_generation",
                "Attempt generation does not match its fencing token",
            ));
        }
        Ok(AttemptLease {
            attempt_id: attempt_id.into(),
            step_id,
            worker_id,
            target_id,
            fencing_token,
            lease_expires_unix_ms,
        })
    }

    async fn effect_lease(&self, effect_id: &str) -> Result<AttemptLease, ProtocolError> {
        let row: Option<(String, String, String, String, i64, i64)> = sqlx::query_as(
            "select attempt.id, attempt.step_id, attempt.worker_id, attempt.target_id, attempt.fencing_token, attempt.lease_expires_unix_ms from effect_intent effect join step_attempt attempt on attempt.id = effect.attempt_id where effect.id = ?",
        )
        .bind(effect_id)
        .fetch_optional(self.database.readers())
        .await
        .map_err(protocol_database_error)?;
        row.map(
            |(attempt_id, step_id, worker_id, target_id, fencing_token, lease_expires_unix_ms)| {
                AttemptLease {
                    attempt_id,
                    step_id,
                    worker_id,
                    target_id,
                    fencing_token,
                    lease_expires_unix_ms,
                }
            },
        )
        .ok_or_else(|| ProtocolError::new("unknown_effect", "effect intent does not exist"))
    }
}

impl crate::extension::EffectLedger for WorkflowEffectLedger {
    fn prepare<'a>(
        &'a self,
        attempt_id: &'a str,
        generation: u64,
        kind: crate::workflow::effect::ProtectedEffectKind,
        request: &'a BrokeredEffectRequest,
    ) -> crate::extension::BrokerFuture<'a, crate::extension::PreparedEffect> {
        Box::pin(async move {
            let lease = self.attempt_lease(attempt_id, generation).await?;
            let request_json = serde_json::to_string(request).map_err(|error| {
                ProtocolError::new(
                    "invalid_effect",
                    format!("serialize effect intent: {error}"),
                )
            })?;
            let token = self
                .broker
                .prepare(PrepareEffect {
                    id: &request.effect_id,
                    lease: &lease,
                    kind: kind.label(),
                    authority_scope: &request.authority_scope,
                    idempotency_key: &request.idempotency_key,
                    request_json: &request_json,
                    now_unix_ms: effect_unix_ms(),
                })
                .await
                .map_err(protocol_persistence_error)?;
            let (status, result_json): (String, Option<String>) =
                sqlx::query_as("select status, result_json from effect_intent where id = ?")
                    .bind(&token)
                    .fetch_one(self.database.readers())
                    .await
                    .map_err(protocol_database_error)?;
            let prior_result = match status.as_str() {
                "prepared" => None,
                "succeeded" => Some(
                    serde_json::from_str(result_json.as_deref().unwrap_or("null")).map_err(
                        |error| ProtocolError::new("invalid_effect_result", error.to_string()),
                    ),
                ),
                "failed" => Some(Err(ProtocolError::new(
                    "prior_effect_failed",
                    result_json.unwrap_or_else(|| "protected effect previously failed".into()),
                ))),
                "dispatching" | "indeterminate" => {
                    return Err(ProtocolError::new(
                        "reconciliation_required",
                        format!("effect '{token}' must reconcile before replay"),
                    ));
                }
                status => {
                    return Err(ProtocolError::new(
                        "invalid_effect_status",
                        format!("effect '{token}' has unexpected status '{status}'"),
                    ));
                }
            };
            Ok(crate::extension::PreparedEffect {
                token,
                prior_result,
            })
        })
    }

    fn mark_dispatching<'a>(
        &'a self,
        effect_token: &'a str,
    ) -> crate::extension::BrokerFuture<'a, ()> {
        Box::pin(async move {
            let lease = self.effect_lease(effect_token).await?;
            self.broker
                .mark_dispatching(effect_token, &lease, effect_unix_ms())
                .await
                .map_err(protocol_persistence_error)
        })
    }

    fn record_result<'a>(
        &'a self,
        effect_token: &'a str,
        result: &'a Result<serde_json::Value, ProtocolError>,
    ) -> crate::extension::BrokerFuture<'a, bool> {
        Box::pin(async move {
            let lease = self.effect_lease(effect_token).await?;
            let (succeeded, result_json) = match result {
                Ok(value) => (true, value.to_string()),
                Err(error) => (
                    false,
                    serde_json::json!({"code":error.code,"message":error.message}).to_string(),
                ),
            };
            self.broker
                .record_result(
                    effect_token,
                    &lease,
                    succeeded,
                    &result_json,
                    effect_unix_ms(),
                )
                .await
                .map_err(protocol_persistence_error)
        })
    }
}

fn protocol_database_error(error: sqlx::Error) -> ProtocolError {
    ProtocolError::new("database_error", error.to_string())
}

fn protocol_persistence_error(error: DatabaseError) -> ProtocolError {
    ProtocolError::new("effect_persistence", error.to_string())
}

fn effect_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
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
            let changed = sqlx::query("insert into authority_grant (id, run_id, scope, granted_by, granted_unix_ms, expires_unix_ms) values (?, ?, ?, ?, ?, ?) on conflict(id) do nothing")
                .bind(&id).bind(&run_id).bind(&scope).bind(&granted_by).bind(now_unix_ms).bind(expires_unix_ms)
                .execute(&mut *connection).await.map_err(DatabaseError::Query)?.rows_affected();
            if changed == 0 {
                let matches: i64 = sqlx::query_scalar("select exists(select 1 from authority_grant where id=? and run_id=? and scope=? and granted_by=? and granted_unix_ms=? and expires_unix_ms is ?)")
                    .bind(id).bind(run_id).bind(scope).bind(granted_by).bind(now_unix_ms).bind(expires_unix_ms)
                    .fetch_one(connection).await.map_err(DatabaseError::Query)?;
                if matches != 1 {
                    return Err(DatabaseError::Conflict { operation: "grant immutable authority" });
                }
            }
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
        let now = effect_unix_ms();
        self.database
            .write_immediate(|connection| {
                Box::pin(async move {
                    sqlx::query("update effect_intent as effect set status = 'indeterminate', updated_unix_ms = ? where effect.status = 'dispatching' and not exists (select 1 from step_attempt attempt where attempt.id = effect.attempt_id and attempt.status = 'claimed' and attempt.fencing_token = effect.fencing_token and attempt.lease_expires_unix_ms > ?)")
                        .bind(now)
                        .bind(now)
                        .execute(connection)
                        .await
                        .map_err(DatabaseError::Query)?;
                    Ok(())
                })
            })
            .await?;
        sqlx::query_as("select effect.id, effect.effect_kind, effect.idempotency_key, effect.request_json, effect.result_json from effect_intent effect where effect.status = 'indeterminate' or (effect.status = 'dispatching' and not exists (select 1 from step_attempt attempt where attempt.id = effect.attempt_id and attempt.status = 'claimed' and attempt.fencing_token = effect.fencing_token and attempt.lease_expires_unix_ms > ?)) order by effect.updated_unix_ms, effect.id limit ?")
            .bind(now).bind(limit).fetch_all(self.database.readers()).await.map_err(DatabaseError::Query)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::extension::EffectLedger as _;
    use crate::persistence::run_ledger::{ClaimRequest, Coordinator};
    use prism_extension_protocol::{BrokeredEffectRequest, EffectPreconditions, OpaqueReference};

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

    #[test]
    fn production_host_ledger_rejects_gate_mutation_requests() {
        let path = std::env::temp_dir().join(format!(
            "prism-gate-effects-{}-{}.db",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let database = WorkflowDatabase::open(&path).await.unwrap();
                database.write_immediate(|connection| Box::pin(async move {
                    sqlx::query("insert into definition_snapshot (id, definition_name, revision, source, trusted, body_json, digest, created_unix_ms) values ('definition', 'test', '1', 'test', 1, '{}', 'digest', 1)").execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                    sqlx::query("insert into workflow_run (id, definition_snapshot_id, status, created_unix_ms, updated_unix_ms) values ('run', 'definition', 'runnable', 1, 1)").execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                    sqlx::query("insert into workflow_step (id, run_id, step_key, implementation, target_id, status, available_unix_ms, input_json, class) values ('step', 'run', 'gate', 'fake', 'local', 'runnable', 1, '{}', 'gate')").execute(connection).await.map_err(DatabaseError::Query)?;
                    Ok(())
                })).await.unwrap();
                let now = effect_unix_ms();
                let lease = Coordinator::new(database.clone()).claim(ClaimRequest {
                    attempt_id: "attempt", step_id: "step", worker_id: "worker", now_unix_ms: now, lease_expires_unix_ms: now + 60_000,
                }).await.unwrap().unwrap();
                let request = BrokeredEffectRequest {
                    effect_id: "effect".into(), idempotency_key: "key".into(), authority_scope: "git:write".into(),
                    preconditions: EffectPreconditions {
                        repository: OpaqueReference { id: "github:acme/widget".into(), revision: "repo-1".into() },
                        worktree_session: Some(OpaqueReference { id: "session".into(), revision: "incarnation".into() }),
                        expected_head: Some("0123456789abcdef0123456789abcdef01234567".into()), target_repository: None,
                        policy_revision: None, gate_revisions: Default::default(),
                    }, parameters: serde_json::json!({}),
                };
                let error = WorkflowEffectLedger::new(database.clone())
                    .prepare("attempt", u64::try_from(lease.fencing_token).unwrap(), crate::workflow::effect::ProtectedEffectKind::Push, &request)
                    .await.unwrap_err();
                assert_eq!(error.code, "class_forbidden");
                database.close().await;
            });
        let _ = std::fs::remove_file(path);
    }
}
