use sqlx::FromRow;

use super::error::DatabaseError;
use super::pools::WorkflowDatabase;

#[derive(Clone)]
pub(crate) struct RunLedger {
    database: WorkflowDatabase,
}

pub(crate) struct StartRun<'a> {
    pub run_id: &'a str,
    pub definition_snapshot_id: &'a str,
    pub repository: Option<&'a str>,
    pub idempotency_key: &'a str,
    pub now_unix_ms: i64,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub(crate) struct MaterializedStep {
    pub id: String,
    pub key: String,
    pub implementation: String,
    pub target_id: String,
    pub input_json: String,
    pub dependencies: Vec<String>,
    pub resources: Vec<String>,
}

pub(crate) struct RegisterDefinition<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub revision: &'a str,
    pub source: &'a str,
    pub trusted: bool,
    pub body_json: &'a str,
    pub digest: &'a str,
    pub now_unix_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunCommand {
    Pause,
    Resume,
    Cancel,
    Retry,
}

pub(crate) struct RunProjection {
    pub id: String,
    pub definition_name: String,
    pub status: String,
    pub repository: Option<String>,
    pub created_unix_ms: i64,
    pub updated_unix_ms: i64,
    pub completed_unix_ms: Option<i64>,
    pub steps: Vec<StepProjection>,
    pub attempts: Vec<AttemptProjection>,
    pub events: Vec<AuditProjection>,
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub(crate) struct StepProjection {
    pub id: String,
    pub key: String,
    pub implementation: String,
    pub target_id: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub(crate) struct AttemptProjection {
    pub id: String,
    pub step_id: String,
    pub status: String,
    pub worker_id: String,
    pub target_id: String,
    pub fencing_token: i64,
    pub process_id: Option<i64>,
    pub process_start_time_ticks: Option<i64>,
    pub started_unix_ms: i64,
    pub finished_unix_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub(crate) struct AuditProjection {
    pub sequence: i64,
    pub step_id: Option<String>,
    pub attempt_id: Option<String>,
    pub kind: String,
    pub time_unix_ms: i64,
    pub data_json: String,
}

#[allow(
    dead_code,
    reason = "used by the generalized scheduler during workflow cutover"
)]
#[derive(Clone)]
pub(crate) struct Coordinator {
    database: WorkflowDatabase,
}

#[allow(
    dead_code,
    reason = "used by the generalized scheduler during workflow cutover"
)]
pub(crate) struct ClaimRequest<'a> {
    pub attempt_id: &'a str,
    pub step_id: &'a str,
    pub worker_id: &'a str,
    pub now_unix_ms: i64,
    pub lease_expires_unix_ms: i64,
}

#[allow(
    dead_code,
    reason = "used by the generalized scheduler during workflow cutover"
)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AttemptLease {
    pub attempt_id: String,
    pub step_id: String,
    pub worker_id: String,
    pub target_id: String,
    pub fencing_token: i64,
    pub lease_expires_unix_ms: i64,
}

#[allow(
    dead_code,
    reason = "used by the generalized scheduler during workflow cutover"
)]
pub(crate) struct AttemptResult<'a> {
    pub status: &'a str,
    pub result_json: &'a str,
    pub finished_unix_ms: i64,
}

impl RunLedger {
    pub(crate) fn new(database: WorkflowDatabase) -> Self {
        Self { database }
    }

    pub(crate) async fn register_definition(
        &self,
        command: RegisterDefinition<'_>,
    ) -> Result<(), DatabaseError> {
        let values = (
            command.id.to_string(),
            command.name.to_string(),
            command.revision.to_string(),
            command.source.to_string(),
            command.body_json.to_string(),
            command.digest.to_string(),
        );
        let trusted = command.trusted;
        let now_unix_ms = command.now_unix_ms;
        self.database.write_immediate(|connection| Box::pin(async move {
            let changed = sqlx::query("insert into definition_snapshot (id, definition_name, revision, source, trusted, body_json, digest, created_unix_ms) values (?, ?, ?, ?, ?, ?, ?, ?) on conflict(id) do nothing")
                .bind(&values.0).bind(&values.1).bind(&values.2).bind(&values.3).bind(trusted)
                .bind(&values.4).bind(&values.5).bind(now_unix_ms).execute(&mut *connection).await.map_err(DatabaseError::Query)?
                .rows_affected();
            if changed == 0 {
                let matches: i64 = sqlx::query_scalar("select exists(select 1 from definition_snapshot where id = ? and definition_name = ? and revision = ? and source = ? and trusted = ? and body_json = ? and digest = ?)")
                    .bind(&values.0).bind(&values.1).bind(&values.2).bind(&values.3).bind(trusted)
                    .bind(&values.4).bind(&values.5).fetch_one(connection).await.map_err(DatabaseError::Query)?;
                if matches != 1 {
                    return Err(DatabaseError::Conflict { operation: "register immutable definition snapshot" });
                }
            }
            Ok(())
        })).await
    }

    pub(crate) async fn definition_body(
        &self,
        definition_snapshot_id: &str,
    ) -> Result<String, DatabaseError> {
        sqlx::query_scalar("select body_json from definition_snapshot where id = ?")
            .bind(definition_snapshot_id)
            .fetch_one(self.database.readers())
            .await
            .map_err(DatabaseError::Query)
    }

    pub(crate) async fn start(&self, command: StartRun<'_>) -> Result<String, DatabaseError> {
        self.start_materialized(command, Vec::new()).await
    }

    pub(crate) async fn start_materialized(
        &self,
        command: StartRun<'_>,
        steps: Vec<MaterializedStep>,
    ) -> Result<String, DatabaseError> {
        let run_id = command.run_id.to_string();
        let definition_snapshot_id = command.definition_snapshot_id.to_string();
        let repository = command.repository.map(str::to_string);
        let idempotency_key = command.idempotency_key.to_string();
        let now_unix_ms = command.now_unix_ms;
        self.database.write_immediate(|connection| Box::pin(async move {
            let changed = sqlx::query_file!(
                "sql/workflow_ledger/start_run.sql",
                run_id,
                definition_snapshot_id,
                repository,
                now_unix_ms,
                now_unix_ms,
                idempotency_key
            )
                .execute(&mut *connection).await.map_err(DatabaseError::Query)?.rows_affected();
            if changed == 1 {
                sqlx::query("insert into idempotency_record (scope, key, result_kind, result_id, created_unix_ms) values ('manual_invocation', ?, 'run', ?, ?)")
                    .bind(&idempotency_key).bind(&run_id).bind(now_unix_ms)
                    .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                sqlx::query("insert into audit_event (run_id, sequence, kind, time_unix_ms, data_json) values (?, 1, 'run_started', ?, '{}')")
                    .bind(&run_id).bind(now_unix_ms).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                for step in &steps {
                    sqlx::query("insert into workflow_step (id, run_id, step_key, implementation, target_id, status, available_unix_ms, input_json) values (?, ?, ?, ?, ?, 'runnable', ?, ?)")
                        .bind(&step.id).bind(&run_id).bind(&step.key).bind(&step.implementation).bind(&step.target_id)
                        .bind(now_unix_ms).bind(&step.input_json).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                    for resource in &step.resources {
                        sqlx::query("insert into step_resource_requirement (step_id, resource_key) values (?, ?)")
                            .bind(&step.id).bind(resource).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                    }
                }
                for step in &steps {
                    for dependency in &step.dependencies {
                        sqlx::query("insert into step_dependency (step_id, depends_on_step_id) values (?, ?)")
                            .bind(&step.id).bind(dependency).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                    }
                }
                if !steps.is_empty() {
                    sqlx::query("update workflow_run set status = 'runnable' where id = ?")
                        .bind(&run_id).execute(connection).await.map_err(DatabaseError::Query)?;
                }
                Ok(run_id)
            } else {
                sqlx::query_scalar("select result_id from idempotency_record where scope = 'manual_invocation' and key = ?")
                    .bind(&idempotency_key).fetch_one(connection).await.map_err(DatabaseError::Query)
            }
        })).await
    }

    pub(crate) async fn command(
        &self,
        run_id: &str,
        command: RunCommand,
        now_unix_ms: i64,
    ) -> Result<(), DatabaseError> {
        let run_id = run_id.to_string();
        self.database
            .write_immediate(|connection| {
                Box::pin(async move {
                    let changed = match command {
                        RunCommand::Pause => {
                            sqlx::query("update workflow_run set status = 'paused', updated_unix_ms = ? where id = ? and status in ('waiting','runnable','running')")
                                .bind(now_unix_ms).bind(&run_id).execute(&mut *connection).await
                        }
                        RunCommand::Resume => {
                            sqlx::query("update workflow_run set status = case when exists (select 1 from workflow_step step join step_attempt attempt on attempt.step_id = step.id where step.run_id = workflow_run.id and attempt.status = 'claimed') then 'running' else 'runnable' end, updated_unix_ms = ? where id = ? and status = 'paused'")
                                .bind(now_unix_ms).bind(&run_id).execute(&mut *connection).await
                        }
                        RunCommand::Cancel => {
                            let result = sqlx::query("update workflow_run set status = 'cancelled', updated_unix_ms = ?, completed_unix_ms = ? where id = ? and status in ('waiting','runnable','running','paused','recovery_required')")
                                .bind(now_unix_ms).bind(now_unix_ms).bind(&run_id).execute(&mut *connection).await;
                            if result.as_ref().is_ok_and(|result| result.rows_affected() == 1) {
                                sqlx::query("update workflow_step set status = 'cancelled' where run_id = ? and status in ('waiting','runnable')")
                                    .bind(&run_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                            }
                            result
                        }
                        RunCommand::Retry => {
                            let result = sqlx::query("update workflow_run set status = 'runnable', updated_unix_ms = ?, completed_unix_ms = null where id = ? and status in ('failed','recovery_required')")
                                .bind(now_unix_ms).bind(&run_id).execute(&mut *connection).await;
                            if result.as_ref().is_ok_and(|result| result.rows_affected() == 1) {
                                sqlx::query("update workflow_step set status = 'runnable', available_unix_ms = ? where run_id = ? and (status in ('failed','cancelled') or (status = 'claimed' and exists (select 1 from step_attempt attempt where attempt.step_id = workflow_step.id and attempt.status = 'recovery_required')))")
                                    .bind(now_unix_ms).bind(&run_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                            }
                            result
                        }
                    }
                    .map_err(DatabaseError::Query)?
                    .rows_affected();
                    if changed != 1 {
                        return Err(DatabaseError::Conflict { operation: "command workflow run" });
                    }
                    let kind = match command {
                        RunCommand::Pause => "run_paused",
                        RunCommand::Resume => "run_resumed",
                        RunCommand::Cancel => "run_cancelled",
                        RunCommand::Retry => "run_retried",
                    };
                    sqlx::query("insert into audit_event (run_id, sequence, kind, time_unix_ms, data_json) select ?, coalesce(max(sequence), 0) + 1, ?, ?, '{}' from audit_event where run_id = ?")
                        .bind(&run_id).bind(kind).bind(now_unix_ms).bind(&run_id)
                        .execute(connection).await.map_err(DatabaseError::Query)?;
                    Ok(())
                })
            })
            .await
    }

    pub(crate) async fn list(
        &self,
        repository: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RunProjection>, DatabaseError> {
        let limit = i64::try_from(limit).map_err(|_| DatabaseError::InvalidValue {
            field: "workflow list limit",
            value: limit.to_string(),
        })?;
        if limit <= 0 || limit > 256 {
            return Err(DatabaseError::InvalidValue {
                field: "workflow list limit",
                value: limit.to_string(),
            });
        }
        let rows = sqlx::query_file!(
            "sql/workflow_ledger/list_runs.sql",
            repository,
            repository,
            limit
        )
        .fetch_all(self.database.readers())
        .await
        .map_err(DatabaseError::Query)?;
        let mut runs = Vec::with_capacity(rows.len());
        for row in rows {
            let run = self
                .inspect(&row.id)
                .await?
                .ok_or(DatabaseError::Conflict {
                    operation: "project listed workflow run",
                })?;
            runs.push(run);
        }
        Ok(runs)
    }

    pub(crate) async fn inspect(
        &self,
        run_id: &str,
    ) -> Result<Option<RunProjection>, DatabaseError> {
        let row = sqlx::query_file!("sql/workflow_ledger/inspect_run.sql", run_id)
            .fetch_optional(self.database.readers())
            .await
            .map_err(DatabaseError::Query)?;
        let Some(row) = row else { return Ok(None) };
        let steps = sqlx::query_as::<_, StepProjection>(
            "select id, step_key as key, implementation, target_id, status from workflow_step where run_id = ? order by id",
        )
        .bind(run_id)
        .fetch_all(self.database.readers())
        .await
        .map_err(DatabaseError::Query)?;
        let attempts = sqlx::query_as::<_, AttemptProjection>(
            "select attempt.id, attempt.step_id, attempt.status, attempt.worker_id, attempt.target_id, attempt.fencing_token, attempt.process_id, attempt.process_start_time_ticks, attempt.started_unix_ms, attempt.finished_unix_ms from step_attempt attempt join workflow_step step on step.id = attempt.step_id where step.run_id = ? order by attempt.started_unix_ms, attempt.id",
        )
        .bind(run_id)
        .fetch_all(self.database.readers())
        .await
        .map_err(DatabaseError::Query)?;
        let events = sqlx::query_as::<_, AuditProjection>(
            "select sequence, step_id, attempt_id, kind, time_unix_ms, data_json from audit_event where run_id = ? order by sequence",
        )
        .bind(run_id)
        .fetch_all(self.database.readers())
        .await
        .map_err(DatabaseError::Query)?;
        Ok(Some(RunProjection {
            id: row.id,
            definition_name: row.definition_name,
            status: row.status,
            repository: row.repository,
            created_unix_ms: row.created_unix_ms,
            updated_unix_ms: row.updated_unix_ms,
            completed_unix_ms: row.completed_unix_ms,
            steps,
            attempts,
            events,
        }))
    }
}

#[allow(
    dead_code,
    reason = "used by the generalized scheduler during workflow cutover"
)]
impl Coordinator {
    pub(crate) fn new(database: WorkflowDatabase) -> Self {
        Self { database }
    }

    pub(crate) async fn claim(
        &self,
        request: ClaimRequest<'_>,
    ) -> Result<Option<AttemptLease>, DatabaseError> {
        let attempt_id = request.attempt_id.to_string();
        let step_id = request.step_id.to_string();
        let worker_id = request.worker_id.to_string();
        let now_unix_ms = request.now_unix_ms;
        let lease_expires_unix_ms = request.lease_expires_unix_ms;
        if lease_expires_unix_ms <= now_unix_ms {
            return Err(DatabaseError::InvalidValue {
                field: "lease_expires_unix_ms",
                value: lease_expires_unix_ms.to_string(),
            });
        }
        self.database.write_immediate(|connection| Box::pin(async move {
            let row = sqlx::query_file!(
                "sql/workflow_ledger/claim_attempt.sql",
                attempt_id,
                worker_id,
                lease_expires_unix_ms,
                now_unix_ms,
                step_id,
                now_unix_ms
            )
                .fetch_optional(&mut *connection).await.map_err(DatabaseError::Query)?;
            let Some(row) = row else { return Ok(None) };
            sqlx::query("update workflow_step set status = 'claimed' where id = ? and status = 'runnable'")
                .bind(&step_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
            let lease = AttemptLease {
                attempt_id: row.id,
                step_id: row.step_id,
                worker_id: row.worker_id,
                target_id: row.target_id,
                fencing_token: row.fencing_token,
                lease_expires_unix_ms: row.lease_expires_unix_ms,
            };
            append_event(connection, &lease, "attempt_claimed", "{}", now_unix_ms).await?;
            Ok(Some(lease))
        })).await
    }

    pub(crate) async fn append_event(
        &self,
        lease: &AttemptLease,
        kind: &str,
        data_json: &str,
        now: i64,
    ) -> Result<(), DatabaseError> {
        let lease = lease.clone();
        let kind = kind.to_string();
        let data_json = data_json.to_string();
        self.database
            .write_immediate(|connection| {
                Box::pin(
                    async move { append_event(connection, &lease, &kind, &data_json, now).await },
                )
            })
            .await
    }

    pub(crate) async fn renew(
        &self,
        lease: &AttemptLease,
        now: i64,
        expires: i64,
    ) -> Result<(), DatabaseError> {
        let lease = lease.clone();
        self.database
            .write_immediate(|connection| {
                Box::pin(async move {
                    let changed = sqlx::query_file!(
                        "sql/workflow_ledger/renew_lease.sql",
                        expires,
                        lease.attempt_id,
                        lease.worker_id,
                        lease.target_id,
                        lease.fencing_token,
                        now
                    )
                    .execute(connection)
                    .await
                    .map_err(DatabaseError::Query)?
                    .rows_affected();
                    exactly_one_fenced(changed)
                })
            })
            .await
    }

    pub(crate) async fn finish(
        &self,
        lease: &AttemptLease,
        result: AttemptResult<'_>,
    ) -> Result<(), DatabaseError> {
        if !matches!(result.status, "succeeded" | "failed" | "cancelled") {
            return Err(DatabaseError::InvalidValue {
                field: "attempt status",
                value: result.status.into(),
            });
        }
        let lease = lease.clone();
        let status = result.status.to_string();
        let result_json = result.result_json.to_string();
        let finished_unix_ms = result.finished_unix_ms;
        self.database
            .write_immediate(|connection| {
                Box::pin(async move {
                    append_event(
                        &mut *connection,
                        &lease,
                        "attempt_finished",
                        &result_json,
                        finished_unix_ms,
                    )
                    .await?;
                    let changed = sqlx::query_file!(
                        "sql/workflow_ledger/finish_attempt.sql",
                        status,
                        result_json,
                        finished_unix_ms,
                        lease.attempt_id,
                        lease.worker_id,
                        lease.target_id,
                        lease.fencing_token,
                        finished_unix_ms
                    )
                    .execute(&mut *connection)
                    .await
                    .map_err(DatabaseError::Query)?
                    .rows_affected();
                    exactly_one_fenced(changed)?;
                    let step_changed = sqlx::query(
                        "update workflow_step set status = ? where id = ? and status = 'claimed'",
                    )
                    .bind(&status)
                    .bind(&lease.step_id)
                    .execute(&mut *connection)
                    .await
                    .map_err(DatabaseError::Query)?
                    .rows_affected();
                    if step_changed != 1 {
                        return Err(DatabaseError::Conflict {
                            operation: "finish step",
                        });
                    }
                    sqlx::query("delete from resource_claim where attempt_id = ?")
                        .bind(&lease.attempt_id)
                        .execute(&mut *connection)
                        .await
                        .map_err(DatabaseError::Query)?;
                    sqlx::query("delete from capacity_claim where attempt_id = ?")
                        .bind(&lease.attempt_id)
                        .execute(&mut *connection)
                        .await
                        .map_err(DatabaseError::Query)?;
                    let run_id: String = sqlx::query_scalar("select run_id from workflow_step where id = ?")
                        .bind(&lease.step_id).fetch_one(&mut *connection).await.map_err(DatabaseError::Query)?;
                    if status == "failed" || status == "cancelled" {
                        sqlx::query("update workflow_run set status = ?, updated_unix_ms = ?, completed_unix_ms = ? where id = ? and status in ('runnable','running')")
                            .bind(&status).bind(finished_unix_ms).bind(finished_unix_ms).bind(&run_id)
                            .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                    } else {
                        let unfinished: i64 = sqlx::query_scalar("select count(*) from workflow_step where run_id = ? and status <> 'succeeded'")
                            .bind(&run_id).fetch_one(&mut *connection).await.map_err(DatabaseError::Query)?;
                        if unfinished == 0 {
                            sqlx::query("update workflow_run set status = 'succeeded', updated_unix_ms = ?, completed_unix_ms = ? where id = ? and status in ('runnable','running')")
                                .bind(finished_unix_ms).bind(finished_unix_ms).bind(&run_id)
                                .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                        } else {
                            sqlx::query("update workflow_run set updated_unix_ms = ? where id = ?")
                                .bind(finished_unix_ms).bind(&run_id).execute(connection).await.map_err(DatabaseError::Query)?;
                        }
                    }
                    Ok(())
                })
            })
            .await
    }
}

#[allow(
    dead_code,
    reason = "used by the generalized scheduler during workflow cutover"
)]
async fn append_event(
    connection: &mut sqlx::SqliteConnection,
    lease: &AttemptLease,
    kind: &str,
    data: &str,
    now: i64,
) -> Result<(), DatabaseError> {
    let changed = sqlx::query_file!(
        "sql/workflow_ledger/append_fenced_event.sql",
        kind,
        now,
        data,
        lease.attempt_id,
        lease.worker_id,
        lease.target_id,
        lease.fencing_token,
        now
    )
    .execute(connection)
    .await
    .map_err(DatabaseError::Query)?
    .rows_affected();
    exactly_one_fenced(changed)
}

#[allow(
    dead_code,
    reason = "used by the generalized scheduler during workflow cutover"
)]
fn exactly_one_fenced(changed: u64) -> Result<(), DatabaseError> {
    if changed == 1 {
        Ok(())
    } else {
        Err(DatabaseError::StaleClaim)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use sqlx::Connection;

    use super::*;

    static SEQUENCE: AtomicU64 = AtomicU64::new(1);

    fn path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "prism-workflow-ledger-{}-{}.db",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
    }

    async fn fixture(database: &WorkflowDatabase) {
        database.write_immediate(|connection| Box::pin(async move {
            sqlx::query("insert into definition_snapshot (id, definition_name, revision, source, trusted, body_json, digest, created_unix_ms) values ('definition-1', 'approval-tracer', '1', 'bundled', 1, '{}', 'digest', 1)")
                .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
            sqlx::query("insert into workflow_run (id, definition_snapshot_id, status, created_unix_ms, updated_unix_ms) values ('run-1', 'definition-1', 'runnable', 1, 1)")
                .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
            sqlx::query("insert into workflow_step (id, run_id, step_key, implementation, target_id, status, available_unix_ms, input_json) values ('step-1', 'run-1', 'approval', 'approval', 'local', 'runnable', 1, '{}')")
                .execute(connection).await.map_err(DatabaseError::Query)?;
            Ok(())
        })).await.unwrap();
    }

    #[test]
    fn manual_invocation_is_idempotent_and_survives_reopen() {
        let path = path();
        runtime().block_on(async {
            let database = WorkflowDatabase::open(&path).await.unwrap();
            database.write_immediate(|connection| Box::pin(async move {
                sqlx::query("insert into definition_snapshot (id, definition_name, revision, source, trusted, body_json, digest, created_unix_ms) values ('definition-1', 'approval-tracer', '1', 'bundled', 1, '{}', 'digest', 1)")
                    .execute(connection).await.map_err(DatabaseError::Query)?;
                Ok(())
            })).await.unwrap();
            let ledger = RunLedger::new(database.clone());
            for proposed_id in ["run-1", "run-2"] {
                assert_eq!(ledger.start(StartRun {
                    run_id: proposed_id,
                    definition_snapshot_id: "definition-1",
                    repository: None,
                    idempotency_key: "invocation-1",
                    now_unix_ms: 2,
                }).await.unwrap(), "run-1");
            }
            drop(ledger);
            database.close().await;
            drop(database);
            let reopened = WorkflowDatabase::open(&path).await.unwrap();
            assert_eq!(RunLedger::new(reopened).inspect("run-1").await.unwrap().unwrap().definition_name, "approval-tracer");
        });
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn claims_are_exclusive_and_all_attempt_writes_are_fenced() {
        let path = path();
        runtime().block_on(async {
            let database = WorkflowDatabase::open(&path).await.unwrap();
            fixture(&database).await;
            let coordinator = Coordinator::new(database);
            let lease = coordinator
                .claim(ClaimRequest {
                    attempt_id: "attempt-1",
                    step_id: "step-1",
                    worker_id: "worker-1",
                    now_unix_ms: 2,
                    lease_expires_unix_ms: 10,
                })
                .await
                .unwrap()
                .unwrap();
            assert!(
                coordinator
                    .claim(ClaimRequest {
                        attempt_id: "attempt-2",
                        step_id: "step-1",
                        worker_id: "worker-2",
                        now_unix_ms: 2,
                        lease_expires_unix_ms: 10,
                    })
                    .await
                    .unwrap()
                    .is_none()
            );
            coordinator
                .append_event(&lease, "output", "{}", 3)
                .await
                .unwrap();
            assert!(matches!(
                coordinator
                    .append_event(&lease, "late_output", "{}", 11)
                    .await,
                Err(DatabaseError::StaleClaim)
            ));
            coordinator
                .finish(
                    &lease,
                    AttemptResult {
                        status: "succeeded",
                        result_json: "{}",
                        finished_unix_ms: 4,
                    },
                )
                .await
                .unwrap();
            assert!(matches!(
                coordinator
                    .append_event(&lease, "after_finish", "{}", 5)
                    .await,
                Err(DatabaseError::StaleClaim)
            ));
        });
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn workflow_database_refuses_a_repository_database_without_mutating_it() {
        let path = path();
        runtime().block_on(async {
            let mut connection = sqlx::SqliteConnection::connect_with(
                &super::super::pools::options(&path, true, false).unwrap(),
            )
            .await
            .unwrap();
            sqlx::query("create table plan_run (id text primary key)")
                .execute(&mut connection)
                .await
                .unwrap();
            connection.close().await.unwrap();
            let before = std::fs::read(&path).unwrap();
            assert!(matches!(
                WorkflowDatabase::open(&path).await,
                Err(DatabaseError::WrongDatabase { .. })
            ));
            assert_eq!(std::fs::read(&path).unwrap(), before);
        });
        let _ = std::fs::remove_file(&path);
    }
}
