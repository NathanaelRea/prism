use std::collections::{BTreeMap, BTreeSet};

use sqlx::FromRow;

use super::error::DatabaseError;
use super::pools::WorkflowDatabase;
use super::run_ledger::AttemptLease;

#[derive(Clone)]
pub(crate) struct AsyncCoordinator {
    database: WorkflowDatabase,
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub(crate) struct RunnableStep {
    pub id: String,
    pub run_id: String,
    pub implementation: String,
    pub target_id: String,
    pub input_json: String,
    pub input_revisions_json: String,
    pub repository: Option<String>,
}

pub(crate) struct DurableClaim<'a> {
    pub attempt_id: &'a str,
    pub step_id: &'a str,
    pub worker_id: &'a str,
    pub now_unix_ms: i64,
    pub lease_expires_unix_ms: i64,
    pub resources: &'a [String],
    pub capacities: &'a [CapacityRequirement],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CapacityRequirement {
    pub scope: String,
    pub key: String,
    pub maximum: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OutputChunk {
    pub stream: OutputStream,
    pub body: Vec<u8>,
    pub time_unix_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputStream {
    Stdout,
    Stderr,
    System,
}

impl OutputStream {
    fn persisted(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
            Self::System => "system",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub(crate) struct ExpiredAttempt {
    pub id: String,
    pub step_id: String,
    pub worker_id: String,
    pub target_id: String,
    pub fencing_token: i64,
    pub process_id: Option<i64>,
    pub process_start_time_ticks: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub(crate) struct MetricProjection {
    pub name: String,
    pub value: i64,
    pub labels_json: String,
    pub time_unix_ms: i64,
}

impl AsyncCoordinator {
    pub(crate) fn new(database: WorkflowDatabase) -> Self {
        Self { database }
    }

    pub(crate) async fn runnable(
        &self,
        now_unix_ms: i64,
        limit: usize,
    ) -> Result<Vec<RunnableStep>, DatabaseError> {
        self.refresh_readiness(now_unix_ms).await?;
        let limit = i64::try_from(limit).map_err(|_| DatabaseError::InvalidValue {
            field: "scheduler limit",
            value: limit.to_string(),
        })?;
        sqlx::query_as::<_, RunnableStep>(include_str!(
            "../../sql/workflow_ledger/select_runnable.sql"
        ))
        .bind(now_unix_ms)
        .bind(limit)
        .fetch_all(self.database.readers())
        .await
        .map_err(DatabaseError::Query)
    }

    /// Recompute the executable projection from durable dependency, binding, and condition
    /// state. Insertion order is deliberately absent from this transition.
    pub(crate) async fn refresh_readiness(&self, now_unix_ms: i64) -> Result<(), DatabaseError> {
        #[derive(FromRow)]
        struct WaitingRow {
            id: String,
            run_id: String,
            class: String,
            bindings_json: String,
            condition_json: Option<String>,
            on_unknown: String,
            settings_json: String,
        }
        self.database.write_immediate(|connection| Box::pin(async move {
            let waiting = sqlx::query_as::<_, WaitingRow>("select step.id, step.run_id, step.class, step.bindings_json, step.condition_json, step.on_unknown, step.settings_json from workflow_step step join workflow_run run on run.id = step.run_id where step.status = 'waiting' and step.runtime_status = 'waiting' and run.status in ('runnable','running','waiting') order by step.id")
                .fetch_all(&mut *connection).await.map_err(DatabaseError::Query)?;
            let affected_runs: BTreeSet<String> = waiting.iter().map(|step| step.run_id.clone()).collect();
            for step in waiting {
                let blocked: i64 = sqlx::query_scalar("select count(*) from step_dependency dependency join workflow_step prerequisite on prerequisite.id = dependency.depends_on_step_id where dependency.step_id = ? and prerequisite.status <> 'succeeded'")
                    .bind(&step.id).fetch_one(&mut *connection).await.map_err(DatabaseError::Query)?;
                if blocked != 0 { continue; }
                let run_inputs: String = sqlx::query_scalar("select input_json from workflow_run where id = ?")
                    .bind(&step.run_id).fetch_one(&mut *connection).await.map_err(DatabaseError::Query)?;
                let run_inputs: serde_json::Value = serde_json::from_str(&run_inputs).unwrap_or_default();
                let bindings: BTreeMap<String, crate::workflow::definition::Binding> = serde_json::from_str(&step.bindings_json)
                    .map_err(|error| DatabaseError::InvalidValue { field: "persisted bindings", value: error.to_string() })?;
                let mut resolved = serde_json::Map::new();
                let mut input_revisions = serde_json::Map::new();
                let mut unresolved_binding = false;
                let mut values = BTreeMap::new();
                if let Some(object) = run_inputs.as_object() {
                    for (name, value) in object { values.insert(format!("inputs.{name}"), crate::workflow::definition::ConditionValue::Known(value.clone())); }
                }
                for (name, binding) in bindings {
                    use crate::workflow::definition::Binding;
                    let revision = match &binding {
                        Binding::Reference { reference, .. } => load_reference_revision(&mut *connection, &step.run_id, reference).await?,
                        Binding::Literal { .. } | Binding::Parameter { .. } => None,
                    };
                    let value = match binding {
                        Binding::Literal { value } | Binding::Parameter { value, .. } => Some(value),
                        Binding::Reference { reference, .. } if reference.starts_with("inputs.") => run_inputs.pointer(&format!("/{}", reference.trim_start_matches("inputs.").replace('.', "/"))).cloned(),
                        Binding::Reference { reference, .. } if reference.starts_with("steps.") => {
                            load_step_reference(&mut *connection, &step.run_id, &reference).await?
                        }
                        Binding::Reference { .. } => None,
                    };
                    let Some(value) = value else {
                        unresolved_binding = true;
                        continue;
                    };
                    if let Some(revision) = revision { input_revisions.insert(name.clone(), revision); }
                    resolved.insert(name, value);
                }
                if let Some(condition) = &step.condition_json {
                    let expression: crate::workflow::definition::ConditionExpr = serde_json::from_str(condition)
                        .map_err(|error| DatabaseError::InvalidValue { field: "persisted condition", value: error.to_string() })?;
                    let mut references = Vec::new();
                    expression.references(&mut references);
                    for reference in references {
                        if values.contains_key(&reference) { continue; }
                        values.insert(reference.clone(), match load_step_reference(&mut *connection, &step.run_id, &reference).await? {
                            Some(value) => crate::workflow::definition::ConditionValue::Known(value),
                            None => crate::workflow::definition::ConditionValue::Missing,
                        });
                    }
                    match expression.evaluate(&values) {
                        crate::workflow::definition::ConditionValue::Known(serde_json::Value::Bool(true)) => {}
                        crate::workflow::definition::ConditionValue::Known(serde_json::Value::Bool(false)) => {
                            sqlx::query("update workflow_step set status = 'succeeded', runtime_status = 'skipped' where id = ? and status = 'waiting'")
                                .bind(&step.id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                            append_run_event(&mut *connection, &step.run_id, Some(&step.id), "step_skipped_by_condition", "{}", now_unix_ms).await?;
                            continue;
                        }
                        _ if step.on_unknown == "skip" => {
                            sqlx::query("update workflow_step set status = 'succeeded', runtime_status = 'skipped' where id = ? and status = 'waiting'")
                                .bind(&step.id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                            continue;
                        }
                        _ if step.on_unknown == "fail" => {
                            sqlx::query("update workflow_step set status = 'failed', runtime_status = 'failed' where id = ? and status = 'waiting'")
                                .bind(&step.id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                            sqlx::query("update workflow_run set status = 'failed', runtime_status = 'failed', completed_unix_ms = ?, updated_unix_ms = ? where id = ?")
                                .bind(now_unix_ms).bind(now_unix_ms).bind(&step.run_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                            continue;
                        }
                        _ => continue,
                    }
                }
                // A false condition skips a Step even when bindings produced only by the
                // untaken branch do not exist. A true/unconditional Step still requires every
                // declared binding before it can become runnable.
                if unresolved_binding {
                    continue;
                }
                let resolved_input_revisions = serde_json::Value::Object(input_revisions).to_string();
                sqlx::query("update workflow_step set input_json = ?, resolved_input_revisions_json = ? where id = ?")
                    .bind(serde_json::Value::Object(resolved.clone()).to_string())
                    .bind(&resolved_input_revisions).bind(&step.id)
                    .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                match step.class.as_str() {
                    "approval" => {
                        let approval_id = format!("{}:approval", step.id);
                        sqlx::query("insert into approval_request (id, run_id, step_id, status, requested_unix_ms) values (?, ?, ?, 'pending', ?) on conflict(id) do nothing")
                            .bind(&approval_id).bind(&step.run_id).bind(&step.id).bind(now_unix_ms).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                        sqlx::query("insert into approval_evidence (approval_id, subject_json, evidence_json, policy_json) values (?, ?, ?, ?) on conflict(approval_id) do nothing")
                            .bind(&approval_id).bind(&step.settings_json).bind(serde_json::json!({
                                "bindings": resolved.clone(),
                                "revisions": serde_json::from_str::<serde_json::Value>(&resolved_input_revisions).unwrap_or_default()
                            }).to_string()).bind(&step.settings_json)
                            .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                        sqlx::query("update workflow_step set input_json = ?, runtime_status = 'waiting_approval' where id = ?")
                            .bind(serde_json::Value::Object(resolved).to_string()).bind(&step.id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                    }
                    "wait" => {
                        let settings: serde_json::Value = serde_json::from_str(&step.settings_json).unwrap_or_default();
                        let due = settings.get("due_unix_ms").and_then(serde_json::Value::as_i64)
                            .or_else(|| settings.get("duration_ms").and_then(serde_json::Value::as_i64).map(|duration| now_unix_ms.saturating_add(duration)))
                            .unwrap_or(now_unix_ms);
                        sqlx::query("insert into gate_wait (step_id, gate_kind, due_unix_ms, checkpoint_json) values (?, 'wait', ?, ?) on conflict(step_id) do nothing")
                            .bind(&step.id).bind(due).bind(&step.settings_json).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                        sqlx::query("update workflow_step set available_unix_ms = ?, input_json = ?, runtime_status = 'waiting_wakeup' where id = ?")
                            .bind(due).bind(serde_json::Value::Object(resolved).to_string()).bind(&step.id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                    }
                    "workflow_call" => {
                        // Child creation is idempotent and handled by the ledger kernel tick.
                        sqlx::query("update workflow_step set input_json = ?, runtime_status = 'waiting_child' where id = ?")
                            .bind(serde_json::Value::Object(resolved).to_string()).bind(&step.id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                    }
                    _ => {
                        sqlx::query("update workflow_step set status = 'runnable', runtime_status = 'runnable', input_json = ?, available_unix_ms = ? where id = ? and status = 'waiting'")
                            .bind(serde_json::Value::Object(resolved).to_string()).bind(now_unix_ms).bind(&step.id)
                            .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                    }
                }
            }
            for run_id in affected_runs {
                super::run_ledger::project_run_state(&mut *connection, &run_id, now_unix_ms).await?;
            }
            Ok(())
        })).await
    }

    pub(crate) async fn required_resources(
        &self,
        step_id: &str,
    ) -> Result<Vec<String>, DatabaseError> {
        sqlx::query_scalar("select resource_key from step_resource_requirement where step_id = ? order by resource_key")
            .bind(step_id).fetch_all(self.database.readers()).await.map_err(DatabaseError::Query)
    }

    /// Claims a step and every durable resource/capacity slot in one immediate transaction.
    /// Contention is represented as `Ok(None)` and never leaves a partial claim.
    pub(crate) async fn claim(
        &self,
        request: DurableClaim<'_>,
    ) -> Result<Option<AttemptLease>, DatabaseError> {
        if request.lease_expires_unix_ms <= request.now_unix_ms {
            return Err(DatabaseError::InvalidValue {
                field: "lease_expires_unix_ms",
                value: request.lease_expires_unix_ms.to_string(),
            });
        }
        if request.resources.iter().collect::<BTreeSet<_>>().len() != request.resources.len() {
            return Err(DatabaseError::InvalidValue {
                field: "resource claims",
                value: "duplicate resource key".into(),
            });
        }
        if request
            .capacities
            .iter()
            .any(|capacity| capacity.maximum == 0)
        {
            return Err(DatabaseError::InvalidValue {
                field: "capacity maximum",
                value: "0".into(),
            });
        }

        let attempt_id = request.attempt_id.to_string();
        let step_id = request.step_id.to_string();
        let worker_id = request.worker_id.to_string();
        let now = request.now_unix_ms;
        let expires = request.lease_expires_unix_ms;
        let resources = request.resources.to_vec();
        let capacities = request.capacities.to_vec();
        self.database
            .write_immediate(|connection| {
                Box::pin(async move {
                    let budget: Option<(String, Option<i64>, i64)> = sqlx::query_as("select root.id, root.attempt_budget, root.attempts_consumed from workflow_step step join workflow_run run on run.id = step.run_id join workflow_run root on root.id = coalesce(run.lineage_root_id, run.id) where step.id = ?")
                        .bind(&step_id).fetch_optional(&mut *connection).await.map_err(DatabaseError::Query)?;
                    if let Some((_root_id, Some(maximum), consumed)) = &budget
                        && consumed >= maximum
                    {
                        sqlx::query("update workflow_step set status = 'waiting', runtime_status = 'input_required' where id = ? and status = 'runnable'")
                            .bind(&step_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                        sqlx::query("update workflow_run set status = 'waiting', runtime_status = 'input_required', updated_unix_ms = ? where id = (select run_id from workflow_step where id = ?)")
                            .bind(now).bind(&step_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                        return Ok(None);
                    }
                    for resource in &resources {
                        let quarantined: Option<String> = sqlx::query_scalar("select id from execution_workspace where state='quarantined' and (id=? or 'workspace:' || id=?) limit 1")
                            .bind(resource).bind(resource).fetch_optional(&mut *connection).await.map_err(DatabaseError::Query)?;
                        if let Some(workspace_id) = quarantined {
                            sqlx::query("update workflow_step set status='waiting', runtime_status='recovery_required' where id=? and status='runnable'")
                                .bind(&step_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                            sqlx::query("update workflow_run set status='recovery_required', runtime_status='recovery_required', updated_unix_ms=? where id=(select run_id from workflow_step where id=?)")
                                .bind(now).bind(&step_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                            let run_id: String = sqlx::query_scalar("select run_id from workflow_step where id=?")
                                .bind(&step_id).fetch_one(&mut *connection).await.map_err(DatabaseError::Query)?;
                            append_run_event(&mut *connection, &run_id, Some(&step_id), "workspace_quarantined", &serde_json::json!({"workspace_id": workspace_id}).to_string(), now).await?;
                            return Ok(None);
                        }
                    }
                    for resource in &resources {
                        let occupied: i64 = sqlx::query_scalar(
                            "select exists(select 1 from resource_claim where resource_key = ?)",
                        )
                        .bind(resource)
                        .fetch_one(&mut *connection)
                        .await
                        .map_err(DatabaseError::Query)?;
                        if occupied != 0 {
                            return Ok(None);
                        }
                    }
                    let mut slots = Vec::with_capacity(capacities.len());
                    for capacity in &capacities {
                        let maximum = i64::try_from(capacity.maximum).map_err(|_| {
                            DatabaseError::InvalidValue {
                                field: "capacity maximum",
                                value: capacity.maximum.to_string(),
                            }
                        })?;
                        let slot: Option<i64> = sqlx::query_scalar(
                            "with recursive slots(slot) as (select 1 union all select slot + 1 from slots where slot < ?) select slot from slots where not exists (select 1 from capacity_claim claim where claim.scope = ? and claim.capacity_key = ? and claim.slot = slots.slot) order by slot limit 1",
                        )
                        .bind(maximum)
                        .bind(&capacity.scope)
                        .bind(&capacity.key)
                        .fetch_optional(&mut *connection)
                        .await
                        .map_err(DatabaseError::Query)?;
                        let Some(slot) = slot else { return Ok(None) };
                        slots.push(slot);
                    }

                    #[derive(FromRow)]
                    struct LeaseRow {
                        #[sqlx(rename = "id!")]
                        id: String,
                        step_id: String,
                        worker_id: String,
                        target_id: String,
                        fencing_token: i64,
                        lease_expires_unix_ms: i64,
                    }
                    let row = sqlx::query_as::<_, LeaseRow>(include_str!(
                        "../../sql/workflow_ledger/claim_attempt.sql"
                    ))
                    .bind(&attempt_id)
                    .bind(&worker_id)
                    .bind(expires)
                    .bind(now)
                    .bind(&step_id)
                    .bind(now)
                    .fetch_optional(&mut *connection)
                    .await
                    .map_err(DatabaseError::Query)?;
                    let Some(row) = row else { return Ok(None) };
                    sqlx::query("update step_attempt set input_revisions_json = (select resolved_input_revisions_json from workflow_step where id = ?) where id = ?")
                        .bind(&step_id).bind(&attempt_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                    if let Some((root_id, _, _)) = budget {
                        sqlx::query("update workflow_run set attempts_consumed = attempts_consumed + 1 where id = ?")
                            .bind(root_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                    }
                    let changed = sqlx::query(
                        "update workflow_step set status = 'claimed', runtime_status='running' where id = ? and status = 'runnable'",
                    )
                    .bind(&step_id)
                    .execute(&mut *connection)
                    .await
                    .map_err(DatabaseError::Query)?
                    .rows_affected();
                    if changed != 1 {
                        return Err(DatabaseError::Conflict { operation: "claim step" });
                    }
                    sqlx::query("update workflow_run set status = 'running', runtime_status='running', updated_unix_ms = ? where id = (select run_id from workflow_step where id = ?) and status = 'runnable'")
                        .bind(now).bind(&step_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                    for resource in &resources {
                        sqlx::query("insert into resource_claim (resource_key, attempt_id, fencing_token, acquired_unix_ms) values (?, ?, ?, ?)")
                            .bind(resource).bind(&attempt_id).bind(row.fencing_token).bind(now)
                            .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                    }
                    for (capacity, slot) in capacities.iter().zip(slots) {
                        sqlx::query("insert into capacity_claim (scope, capacity_key, slot, attempt_id, fencing_token, acquired_unix_ms) values (?, ?, ?, ?, ?, ?)")
                            .bind(&capacity.scope).bind(&capacity.key).bind(slot).bind(&attempt_id)
                            .bind(row.fencing_token).bind(now).execute(&mut *connection).await
                            .map_err(DatabaseError::Query)?;
                    }
                    let lease = AttemptLease {
                        attempt_id: row.id,
                        step_id: row.step_id,
                        worker_id: row.worker_id,
                        target_id: row.target_id,
                        fencing_token: row.fencing_token,
                        lease_expires_unix_ms: row.lease_expires_unix_ms,
                    };
                    append_audit(&mut *connection, &lease, "attempt_claimed", "{}", now).await?;
                    Ok(Some(lease))
                })
            })
            .await
    }

    pub(crate) async fn renew_batch(
        &self,
        leases: &[AttemptLease],
        now_unix_ms: i64,
        lease_expires_unix_ms: i64,
    ) -> Result<Vec<String>, DatabaseError> {
        let leases = leases.to_vec();
        self.database
            .write_immediate(|connection| {
                Box::pin(async move {
                    let mut lost = Vec::new();
                    for lease in &leases {
                        let changed =
                            sqlx::query(include_str!("../../sql/workflow_ledger/renew_lease.sql"))
                                .bind(lease_expires_unix_ms)
                                .bind(&lease.attempt_id)
                                .bind(&lease.worker_id)
                                .bind(&lease.target_id)
                                .bind(lease.fencing_token)
                                .bind(now_unix_ms)
                                .execute(&mut *connection)
                                .await
                                .map_err(DatabaseError::Query)?
                                .rows_affected();
                        if changed != 1 {
                            lost.push(lease.attempt_id.clone());
                        }
                    }
                    Ok(lost)
                })
            })
            .await
    }

    /// Releases a durable claim when the in-memory execution handoff fails. The attempt is
    /// retained as cancelled history while its step becomes runnable again immediately.
    pub(crate) async fn release_handoff(
        &self,
        lease: &AttemptLease,
        now_unix_ms: i64,
    ) -> Result<(), DatabaseError> {
        let lease = lease.clone();
        self.database
            .write_immediate(|connection| {
                Box::pin(async move {
                    validate_lease(&mut *connection, &lease, now_unix_ms).await?;
                    append_audit(
                        &mut *connection,
                        &lease,
                        "attempt_handoff_released",
                        "{}",
                        now_unix_ms,
                    )
                    .await?;
                    sqlx::query("delete from resource_claim where attempt_id = ? and fencing_token = ?")
                        .bind(&lease.attempt_id)
                        .bind(lease.fencing_token)
                        .execute(&mut *connection)
                        .await
                        .map_err(DatabaseError::Query)?;
                    sqlx::query("delete from capacity_claim where attempt_id = ? and fencing_token = ?")
                        .bind(&lease.attempt_id)
                        .bind(lease.fencing_token)
                        .execute(&mut *connection)
                        .await
                        .map_err(DatabaseError::Query)?;
                    let step_changed = sqlx::query(
                        "update workflow_step set status = 'runnable', available_unix_ms = ? where id = ? and status = 'claimed'",
                    )
                    .bind(now_unix_ms)
                    .bind(&lease.step_id)
                    .execute(&mut *connection)
                    .await
                    .map_err(DatabaseError::Query)?
                    .rows_affected();
                    if step_changed != 1 {
                        return Err(DatabaseError::Conflict {
                            operation: "release handoff step",
                        });
                    }
                    let changed = sqlx::query(include_str!(
                        "../../sql/workflow_ledger/release_handoff.sql"
                    ))
                    .bind(now_unix_ms)
                    .bind(&lease.attempt_id)
                    .bind(&lease.worker_id)
                    .bind(&lease.target_id)
                    .bind(lease.fencing_token)
                    .bind(now_unix_ms)
                    .execute(connection)
                    .await
                    .map_err(DatabaseError::Query)?
                    .rows_affected();
                    if changed == 1 {
                        Ok(())
                    } else {
                        Err(DatabaseError::StaleClaim)
                    }
                })
            })
            .await
    }

    pub(crate) async fn record_process(
        &self,
        lease: &AttemptLease,
        process_id: u32,
        process_start_time_ticks: Option<u64>,
        now_unix_ms: i64,
    ) -> Result<(), DatabaseError> {
        let process_id = i64::from(process_id);
        let process_start_time_ticks = process_start_time_ticks
            .map(|value| {
                i64::try_from(value).map_err(|_| DatabaseError::InvalidValue {
                    field: "process start time",
                    value: value.to_string(),
                })
            })
            .transpose()?;
        let lease = lease.clone();
        self.database
            .write_immediate(|connection| {
                Box::pin(async move {
                    let changed = sqlx::query(
                        "update step_attempt set process_id = ?, process_start_time_ticks = ? where id = ? and status = 'claimed' and worker_id = ? and target_id = ? and fencing_token = ? and lease_expires_unix_ms > ?",
                    )
                    .bind(process_id)
                    .bind(process_start_time_ticks)
                    .bind(&lease.attempt_id)
                    .bind(&lease.worker_id)
                    .bind(&lease.target_id)
                    .bind(lease.fencing_token)
                    .bind(now_unix_ms)
                    .execute(&mut *connection)
                    .await
                    .map_err(DatabaseError::Query)?
                    .rows_affected();
                    if changed != 1 {
                        return Err(DatabaseError::StaleClaim);
                    }
                    append_audit(
                        connection,
                        &lease,
                        "process_recorded",
                        &serde_json::json!({
                            "process_id": process_id,
                            "process_start_time_ticks": process_start_time_ticks,
                        })
                        .to_string(),
                        now_unix_ms,
                    )
                    .await
                })
            })
            .await
    }

    pub(crate) async fn append_output(
        &self,
        lease: &AttemptLease,
        chunks: &[OutputChunk],
        maximum_bytes: usize,
        now_unix_ms: i64,
    ) -> Result<(), DatabaseError> {
        if chunks.is_empty() {
            return Ok(());
        }
        let attempted_bytes = chunks.iter().map(|chunk| chunk.body.len()).sum::<usize>();
        if attempted_bytes > maximum_bytes {
            return Err(DatabaseError::OutputBudgetExceeded {
                attempted_bytes,
                maximum_bytes,
            });
        }
        let lease = lease.clone();
        let chunk_count = chunks.len();
        let chunks = chunks.to_vec();
        self.database.write_immediate(|connection| Box::pin(async move {
            validate_lease(&mut *connection, &lease, now_unix_ms).await?;
            let persisted: i64 = sqlx::query_scalar("select coalesce(sum(length(body)), 0) from attempt_output where attempt_id = ?")
                .bind(&lease.attempt_id).fetch_one(&mut *connection).await.map_err(DatabaseError::Query)?;
            let persisted = usize::try_from(persisted).map_err(|_| DatabaseError::InvalidValue { field: "persisted output bytes", value: persisted.to_string() })?;
            if persisted.saturating_add(attempted_bytes) > maximum_bytes {
                return Err(DatabaseError::OutputBudgetExceeded { attempted_bytes: persisted.saturating_add(attempted_bytes), maximum_bytes });
            }
            let mut sequence: i64 = sqlx::query_scalar("select coalesce(max(sequence), 0) from attempt_output where attempt_id = ?")
                .bind(&lease.attempt_id).fetch_one(&mut *connection).await.map_err(DatabaseError::Query)?;
            for chunk in chunks {
                sequence += 1;
                sqlx::query("insert into attempt_output (attempt_id, sequence, stream, body, time_unix_ms) values (?, ?, ?, ?, ?)")
                    .bind(&lease.attempt_id).bind(sequence).bind(chunk.stream.persisted()).bind(chunk.body)
                    .bind(chunk.time_unix_ms).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
            }
            append_audit(&mut *connection, &lease, "output_batch", &format!("{{\"chunks\":{chunk_count},\"bytes\":{attempted_bytes}}}"), now_unix_ms).await
        })).await
    }

    pub(crate) async fn recover_expired(
        &self,
        now_unix_ms: i64,
    ) -> Result<Vec<ExpiredAttempt>, DatabaseError> {
        self.database.write_immediate(|connection| Box::pin(async move {
            let expired = sqlx::query_as::<_, ExpiredAttempt>(include_str!("../../sql/workflow_ledger/recover_expired.sql"))
                .bind(now_unix_ms).bind(now_unix_ms).fetch_all(&mut *connection).await.map_err(DatabaseError::Query)?;
            for attempt in &expired {
                sqlx::query("delete from resource_claim where attempt_id = ?").bind(&attempt.id)
                    .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                sqlx::query("delete from capacity_claim where attempt_id = ?").bind(&attempt.id)
                    .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                let run_id: String = sqlx::query_scalar("select run_id from workflow_step where id = ?")
                    .bind(&attempt.step_id).fetch_one(&mut *connection).await.map_err(DatabaseError::Query)?;
                if attempt.process_id.is_some() {
                    sqlx::query("update step_attempt set status = 'recovery_required' where id = ? and status = 'expired'")
                        .bind(&attempt.id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                    sqlx::query("update workflow_step set runtime_status='recovery_required' where id=?")
                        .bind(&attempt.step_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                    sqlx::query("update workflow_run set status = 'recovery_required', runtime_status='recovery_required', updated_unix_ms = ? where id = ? and status in ('runnable','running')")
                        .bind(now_unix_ms).bind(&run_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                    sqlx::query("insert into audit_event (run_id, step_id, attempt_id, sequence, kind, time_unix_ms, data_json) select ?, ?, ?, coalesce(max(sequence), 0) + 1, 'attempt_recovery_required', ?, ? from audit_event where run_id = ?")
                        .bind(&run_id).bind(&attempt.step_id).bind(&attempt.id).bind(now_unix_ms)
                        .bind(serde_json::json!({
                            "process_id": attempt.process_id,
                            "process_start_time_ticks": attempt.process_start_time_ticks,
                        }).to_string())
                        .bind(&run_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                } else {
                    sqlx::query("update workflow_step set status = 'runnable', runtime_status='runnable' where id = ? and status = 'claimed'")
                        .bind(&attempt.step_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                    sqlx::query("insert into audit_event (run_id, step_id, attempt_id, sequence, kind, time_unix_ms, data_json) select ?, ?, ?, coalesce(max(sequence), 0) + 1, 'attempt_expired', ?, '{}' from audit_event where run_id = ?")
                        .bind(&run_id).bind(&attempt.step_id).bind(&attempt.id).bind(now_unix_ms).bind(&run_id)
                        .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                }
            }
            Ok(expired)
        })).await
    }

    pub(crate) async fn latest_metrics(&self) -> Result<Vec<MetricProjection>, DatabaseError> {
        let mut metrics = sqlx::query_as("select metric.name, metric.value, metric.labels_json, metric.time_unix_ms from control_plane_metric metric join (select name, max(id) id from control_plane_metric group by name) latest on latest.id = metric.id order by metric.name")
            .fetch_all(self.database.readers()).await.map_err(DatabaseError::Query)?;
        let (writer_size, writer_idle, reader_size, reader_idle) = self.database.pool_utilization();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|duration| i64::try_from(duration.as_millis()).ok())
            .unwrap_or(0);
        for (name, value) in [
            (
                "writer_pool_in_use",
                usize::try_from(writer_size)
                    .unwrap_or(usize::MAX)
                    .saturating_sub(writer_idle),
            ),
            (
                "reader_pool_in_use",
                usize::try_from(reader_size)
                    .unwrap_or(usize::MAX)
                    .saturating_sub(reader_idle),
            ),
            (
                "reader_pool_size",
                usize::try_from(reader_size).unwrap_or(usize::MAX),
            ),
        ] {
            metrics.push(MetricProjection {
                name: name.into(),
                value: i64::try_from(value).unwrap_or(i64::MAX),
                labels_json: "{}".into(),
                time_unix_ms: now,
            });
        }
        metrics.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(metrics)
    }

    pub(crate) async fn metric(
        &self,
        name: &str,
        value: i64,
        labels_json: &str,
        now_unix_ms: i64,
    ) -> Result<(), DatabaseError> {
        const MAX_METRIC_ROWS: i64 = 10_000;
        if name.is_empty() || name.len() > 128 {
            return Err(DatabaseError::InvalidValue {
                field: "metric name",
                value: format!("{} bytes", name.len()),
            });
        }
        if labels_json.len() > 4096
            || serde_json::from_str::<serde_json::Value>(labels_json).is_err()
        {
            return Err(DatabaseError::InvalidValue {
                field: "metric labels",
                value: format!("invalid or oversized ({} bytes)", labels_json.len()),
            });
        }
        let name = name.to_string();
        let labels_json = labels_json.to_string();
        self.database
            .write_immediate(|connection| {
                Box::pin(async move {
                    sqlx::query("insert into control_plane_metric (name, value, labels_json, time_unix_ms) values (?, ?, ?, ?)")
                        .bind(name).bind(value).bind(labels_json).bind(now_unix_ms)
                        .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                    sqlx::query("delete from control_plane_metric where id <= coalesce((select id from control_plane_metric order by id desc limit 1 offset ?), 0)")
                        .bind(MAX_METRIC_ROWS)
                        .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                    Ok(())
                })
            })
            .await
    }
}

async fn load_step_reference(
    connection: &mut sqlx::SqliteConnection,
    run_id: &str,
    reference: &str,
) -> Result<Option<serde_json::Value>, DatabaseError> {
    let parts: Vec<_> = reference.split('.').collect();
    if parts.len() == 3 && parts[0] == "steps" && parts[2] == "outcome" {
        let status: Option<String> = sqlx::query_scalar(
            "select runtime_status from workflow_step where run_id = ? and step_key = ?",
        )
        .bind(run_id)
        .bind(parts[1])
        .fetch_optional(connection)
        .await
        .map_err(DatabaseError::Query)?;
        return Ok(status.map(|status| {
            serde_json::Value::Bool(matches!(status.as_str(), "succeeded" | "skipped"))
        }));
    }
    if parts.len() != 4 || parts[0] != "steps" || parts[2] != "outputs" {
        return Ok(None);
    }
    let value: Option<String> = sqlx::query_scalar("select value_json from (select binding.value_json, attempt.attempt_number as ordering from workflow_step step join step_attempt attempt on attempt.step_id = step.id join attempt_output_binding binding on binding.attempt_id = attempt.id where step.run_id = ? and step.step_key = ? and binding.name = ? and attempt.status = 'succeeded' and (step.invalidated_unix_ms is null or attempt.finished_unix_ms >= step.invalidated_unix_ms) union all select binding.value_json, 2147483647 from workflow_step step join step_output_binding binding on binding.step_id = step.id where step.run_id = ? and step.step_key = ? and binding.name = ?) order by ordering desc limit 1")
        .bind(run_id).bind(parts[1]).bind(parts[3])
        .bind(run_id).bind(parts[1]).bind(parts[3])
        .fetch_optional(connection).await.map_err(DatabaseError::Query)?;
    value
        .map(|value| {
            serde_json::from_str(&value).map_err(|error| DatabaseError::InvalidValue {
                field: "persisted output binding",
                value: error.to_string(),
            })
        })
        .transpose()
}

async fn load_reference_revision(
    connection: &mut sqlx::SqliteConnection,
    run_id: &str,
    reference: &str,
) -> Result<Option<serde_json::Value>, DatabaseError> {
    if let Some(name) = reference.strip_prefix("inputs.") {
        let row: Option<(String, i64, String, String)> = sqlx::query_as("select binding.artifact_id, binding.revision, binding.schema_id, artifact.digest from workflow_input_binding binding join artifact on artifact.id = binding.artifact_id where binding.run_id=? and binding.name=?")
            .bind(run_id).bind(name).fetch_optional(connection).await.map_err(DatabaseError::Query)?;
        return Ok(row.map(|(artifact_id, revision, schema, digest)| serde_json::json!({"artifact_id":artifact_id,"revision":revision,"schema":schema,"digest":digest})));
    }
    let parts: Vec<_> = reference.split('.').collect();
    if parts.len() != 4 || parts[0] != "steps" || parts[2] != "outputs" {
        return Ok(None);
    }
    let row: Option<(String, i64, String, String)> = sqlx::query_as("select artifact_id, revision, schema_id, digest from (select artifact.id as artifact_id, artifact.revision, binding.schema_id, artifact.digest, attempt.attempt_number as ordering from workflow_step step join step_attempt attempt on attempt.step_id=step.id join attempt_output_binding binding on binding.attempt_id=attempt.id join artifact on artifact.id=binding.artifact_id where step.run_id=? and step.step_key=? and binding.name=? and attempt.status='succeeded' and (step.invalidated_unix_ms is null or attempt.finished_unix_ms >= step.invalidated_unix_ms) union all select artifact.id, artifact.revision, binding.schema_id, artifact.digest, 2147483647 from workflow_step step join step_output_binding binding on binding.step_id=step.id join artifact on artifact.id=binding.artifact_id where step.run_id=? and step.step_key=? and binding.name=?) order by ordering desc limit 1")
        .bind(run_id).bind(parts[1]).bind(parts[3]).bind(run_id).bind(parts[1]).bind(parts[3])
        .fetch_optional(connection).await.map_err(DatabaseError::Query)?;
    Ok(row.map(|(artifact_id, revision, schema, digest)| serde_json::json!({"artifact_id":artifact_id,"revision":revision,"schema":schema,"digest":digest})))
}

async fn append_run_event(
    connection: &mut sqlx::SqliteConnection,
    run_id: &str,
    step_id: Option<&str>,
    kind: &str,
    data_json: &str,
    now_unix_ms: i64,
) -> Result<(), DatabaseError> {
    sqlx::query("insert into audit_event (run_id, step_id, sequence, kind, time_unix_ms, data_json) select ?, ?, coalesce(max(sequence), 0) + 1, ?, ?, ? from audit_event where run_id = ?")
        .bind(run_id).bind(step_id).bind(kind).bind(now_unix_ms).bind(data_json).bind(run_id)
        .execute(connection).await.map_err(DatabaseError::Query)?;
    Ok(())
}

async fn validate_lease(
    connection: &mut sqlx::SqliteConnection,
    lease: &AttemptLease,
    now_unix_ms: i64,
) -> Result<(), DatabaseError> {
    let valid: i64 =
        sqlx::query_scalar(include_str!("../../sql/workflow_ledger/validate_lease.sql"))
            .bind(&lease.attempt_id)
            .bind(&lease.worker_id)
            .bind(&lease.target_id)
            .bind(lease.fencing_token)
            .bind(now_unix_ms)
            .fetch_one(connection)
            .await
            .map_err(DatabaseError::Query)?;
    if valid == 1 {
        Ok(())
    } else {
        Err(DatabaseError::StaleClaim)
    }
}

async fn append_audit(
    connection: &mut sqlx::SqliteConnection,
    lease: &AttemptLease,
    kind: &str,
    data_json: &str,
    now_unix_ms: i64,
) -> Result<(), DatabaseError> {
    let changed = sqlx::query(include_str!(
        "../../sql/workflow_ledger/append_fenced_event.sql"
    ))
    .bind(kind)
    .bind(now_unix_ms)
    .bind(data_json)
    .bind(&lease.attempt_id)
    .bind(&lease.worker_id)
    .bind(&lease.target_id)
    .bind(lease.fencing_token)
    .bind(now_unix_ms)
    .execute(connection)
    .await
    .map_err(DatabaseError::Query)?
    .rows_affected();
    if changed == 1 {
        Ok(())
    } else {
        Err(DatabaseError::StaleClaim)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static SEQUENCE: AtomicU64 = AtomicU64::new(1);

    fn path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "prism-control-plane-{}-{}.db",
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
            sqlx::query("insert into definition_snapshot (id, definition_name, revision, source, trusted, body_json, digest, created_unix_ms) values ('definition', 'test', '1', 'test', 1, '{}', 'digest', 1)")
                .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
            sqlx::query("insert into workflow_run (id, definition_snapshot_id, status, created_unix_ms, updated_unix_ms) values ('run', 'definition', 'runnable', 1, 1)")
                .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
            for step in ["step-1", "step-2"] {
                sqlx::query("insert into workflow_step (id, run_id, step_key, implementation, target_id, status, available_unix_ms, input_json) values (?, 'run', ?, 'fake', 'local', 'runnable', 1, '{}')")
                    .bind(step).bind(step).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
            }
            Ok(())
        })).await.unwrap();
    }

    #[test]
    fn failed_dispatch_handoff_releases_claims_immediately() {
        let path = path();
        runtime().block_on(async {
            let database = WorkflowDatabase::open(&path).await.unwrap();
            fixture(&database).await;
            let coordinator = AsyncCoordinator::new(database.clone());
            let capacities = [CapacityRequirement {
                scope: "global".into(),
                key: "attempts".into(),
                maximum: 1,
            }];
            let lease = coordinator
                .claim(DurableClaim {
                    attempt_id: "attempt-handoff",
                    step_id: "step-1",
                    worker_id: "worker",
                    now_unix_ms: 2,
                    lease_expires_unix_ms: 100,
                    resources: &["repository:test".into()],
                    capacities: &capacities,
                })
                .await
                .unwrap()
                .unwrap();

            coordinator.release_handoff(&lease, 3).await.unwrap();

            let attempt_status: String = sqlx::query_scalar(
                "select status from step_attempt where id = 'attempt-handoff'",
            )
            .fetch_one(database.readers())
            .await
            .unwrap();
            let step_status: String =
                sqlx::query_scalar("select status from workflow_step where id = 'step-1'")
                    .fetch_one(database.readers())
                    .await
                    .unwrap();
            let remaining_claims: i64 = sqlx::query_scalar(
                "select (select count(*) from resource_claim) + (select count(*) from capacity_claim)",
            )
            .fetch_one(database.readers())
            .await
            .unwrap();
            assert_eq!(attempt_status, "cancelled");
            assert_eq!(step_status, "runnable");
            assert_eq!(remaining_claims, 0);
            assert!(matches!(
                coordinator.release_handoff(&lease, 4).await,
                Err(DatabaseError::StaleClaim)
            ));
            assert!(
                coordinator
                    .claim(DurableClaim {
                        attempt_id: "attempt-after-handoff",
                        step_id: "step-1",
                        worker_id: "worker",
                        now_unix_ms: 4,
                        lease_expires_unix_ms: 100,
                        resources: &["repository:test".into()],
                        capacities: &capacities,
                    })
                    .await
                    .unwrap()
                    .is_some()
            );
            database.close().await;
        });
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn false_condition_skips_before_resolving_untaken_branch_bindings() {
        let path = path();
        runtime().block_on(async {
            let database = WorkflowDatabase::open(&path).await.unwrap();
            let condition = crate::workflow::definition::ConditionExpr::parse(
                "steps.choose.outputs.needs_repair == true",
            )
            .unwrap();
            let bindings = std::collections::BTreeMap::from([
                (
                    "candidate".to_string(),
                    crate::workflow::definition::Binding::Reference {
                        reference: "steps.repair.outputs.candidate".into(),
                        schema: "prism.candidate-change/v1".into(),
                    },
                ),
            ]);
            database
                .write_immediate(|connection| {
                    Box::pin(async move {
                        sqlx::query("insert into definition_snapshot (id, definition_name, revision, source, trusted, body_json, digest, created_unix_ms) values ('definition', 'test', '1', 'test', 1, '{}', 'digest', 1)")
                            .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                        sqlx::query("insert into workflow_run (id, definition_snapshot_id, status, created_unix_ms, updated_unix_ms) values ('run', 'definition', 'runnable', 1, 1)")
                            .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                        sqlx::query("insert into workflow_step (id, run_id, step_key, implementation, target_id, status, available_unix_ms, input_json, runtime_status, bindings_json, condition_json, on_unknown) values ('choose', 'run', 'choose', 'fake', 'local', 'succeeded', 1, '{}', 'succeeded', '{}', null, 'wait')")
                            .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                        sqlx::query("insert into step_output_binding (step_id, name, schema_id, value_json) values ('choose', 'needs_repair', 'prism.boolean/v1', 'false')")
                            .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                        sqlx::query("insert into workflow_step (id, run_id, step_key, implementation, target_id, status, available_unix_ms, input_json, runtime_status, bindings_json, condition_json, on_unknown) values ('conditional', 'run', 'conditional', 'fake', 'local', 'waiting', 1, '{}', 'waiting', ?, ?, 'fail')")
                            .bind(serde_json::to_string(&bindings).unwrap())
                            .bind(serde_json::to_string(&condition).unwrap())
                            .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                        Ok(())
                    })
                })
                .await
                .unwrap();
            AsyncCoordinator::new(database.clone())
                .refresh_readiness(2)
                .await
                .unwrap();
            let status: (String, String) = sqlx::query_as(
                "select status, runtime_status from workflow_step where id='conditional'",
            )
            .fetch_one(database.readers())
            .await
            .unwrap();
            assert_eq!(status, ("succeeded".into(), "skipped".into()));
        });
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn durable_capacity_output_and_expiry_are_fenced() {
        let path = path();
        runtime().block_on(async {
            let database = WorkflowDatabase::open(&path).await.unwrap();
            fixture(&database).await;
            let coordinator = AsyncCoordinator::new(database.clone());
            let capacity = [CapacityRequirement {
                scope: "global".into(),
                key: "attempts".into(),
                maximum: 1,
            }];
            let lease = coordinator
                .claim(DurableClaim {
                    attempt_id: "attempt-1",
                    step_id: "step-1",
                    worker_id: "worker",
                    now_unix_ms: 2,
                    lease_expires_unix_ms: 10,
                    resources: &[],
                    capacities: &capacity,
                })
                .await
                .unwrap()
                .unwrap();
            assert!(
                coordinator
                    .claim(DurableClaim {
                        attempt_id: "attempt-2",
                        step_id: "step-2",
                        worker_id: "worker",
                        now_unix_ms: 2,
                        lease_expires_unix_ms: 10,
                        resources: &[],
                        capacities: &capacity,
                    })
                    .await
                    .unwrap()
                    .is_none()
            );
            coordinator
                .append_output(
                    &lease,
                    &[OutputChunk {
                        stream: OutputStream::Stdout,
                        body: b"ordered".to_vec(),
                        time_unix_ms: 3,
                    }],
                    64,
                    3,
                )
                .await
                .unwrap();
            assert!(matches!(
                coordinator
                    .append_output(
                        &lease,
                        &[OutputChunk {
                            stream: OutputStream::Stdout,
                            body: vec![0; 100],
                            time_unix_ms: 4,
                        }],
                        64,
                        4
                    )
                    .await,
                Err(DatabaseError::OutputBudgetExceeded { .. })
            ));
            assert_eq!(coordinator.recover_expired(10).await.unwrap().len(), 1);
            assert!(matches!(
                coordinator
                    .append_output(
                        &lease,
                        &[OutputChunk {
                            stream: OutputStream::Stdout,
                            body: b"late".to_vec(),
                            time_unix_ms: 11,
                        }],
                        64,
                        11
                    )
                    .await,
                Err(DatabaseError::StaleClaim)
            ));
            assert!(
                coordinator
                    .claim(DurableClaim {
                        attempt_id: "attempt-2",
                        step_id: "step-2",
                        worker_id: "worker",
                        now_unix_ms: 11,
                        lease_expires_unix_ms: 20,
                        resources: &[],
                        capacities: &capacity,
                    })
                    .await
                    .unwrap()
                    .is_some()
            );
            database.close().await;
        });
        let _ = std::fs::remove_file(path);
    }
}
