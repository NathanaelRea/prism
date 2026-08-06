use sqlx::FromRow;

use super::error::DatabaseError;
use super::pools::WorkflowDatabase;

#[derive(Clone)]
pub(crate) struct WakeupStore {
    database: WorkflowDatabase,
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub(crate) struct DueTrigger {
    pub id: String,
    pub trigger_id: String,
    pub definition_snapshot_id: String,
    pub config_json: String,
    pub deduplication_key: String,
    pub due_unix_ms: i64,
}

impl WakeupStore {
    pub(crate) fn new(database: WorkflowDatabase) -> Self {
        Self { database }
    }

    pub(crate) async fn register_trigger(
        &self,
        id: &str,
        definition_snapshot_id: &str,
        overlap_policy: &str,
        config_json: &str,
        enabled: bool,
    ) -> Result<(), DatabaseError> {
        if !matches!(overlap_policy, "allow" | "serialize") {
            return Err(DatabaseError::InvalidValue {
                field: "trigger overlap policy",
                value: overlap_policy.to_string(),
            });
        }
        let values = (
            id.to_string(),
            definition_snapshot_id.to_string(),
            overlap_policy.to_string(),
            config_json.to_string(),
        );
        self.database.write_immediate(|connection| Box::pin(async move {
            sqlx::query("insert into trigger_definition (id, definition_snapshot_id, overlap_policy, config_json, enabled) values (?, ?, ?, ?, ?)")
                .bind(values.0).bind(values.1).bind(values.2).bind(values.3).bind(enabled)
                .execute(connection).await.map_err(DatabaseError::Query)?;
            Ok(())
        })).await
    }

    pub(crate) async fn record_occurrence(
        &self,
        id: &str,
        trigger_id: &str,
        deduplication_key: &str,
        due_unix_ms: i64,
    ) -> Result<bool, DatabaseError> {
        let id = id.to_string();
        let trigger_id = trigger_id.to_string();
        let key = deduplication_key.to_string();
        self.database.write_immediate(|connection| Box::pin(async move {
            let changed = sqlx::query("insert into trigger_occurrence (id, trigger_id, deduplication_key, due_unix_ms, status) values (?, ?, ?, ?, 'pending') on conflict(trigger_id, deduplication_key) do nothing")
                .bind(id).bind(trigger_id).bind(key).bind(due_unix_ms).execute(connection).await
                .map_err(DatabaseError::Query)?.rows_affected();
            Ok(changed == 1)
        })).await
    }

    pub(crate) async fn due_triggers(
        &self,
        now_unix_ms: i64,
        limit: usize,
    ) -> Result<Vec<DueTrigger>, DatabaseError> {
        let limit = i64::try_from(limit).map_err(|_| DatabaseError::InvalidValue {
            field: "trigger batch limit",
            value: limit.to_string(),
        })?;
        sqlx::query_as(include_str!("../../sql/workflow_ledger/due_triggers.sql"))
            .bind(now_unix_ms)
            .bind(limit)
            .fetch_all(self.database.readers())
            .await
            .map_err(DatabaseError::Query)
    }

    pub(crate) async fn complete_trigger(
        &self,
        occurrence_id: &str,
        run_id: &str,
        checkpoint_json: &str,
        now_unix_ms: i64,
    ) -> Result<(), DatabaseError> {
        let occurrence_id = occurrence_id.to_string();
        let run_id = run_id.to_string();
        let checkpoint = checkpoint_json.to_string();
        self.database.write_immediate(|connection| Box::pin(async move {
            let trigger_id: Option<String> = sqlx::query_scalar("update trigger_occurrence set status = 'fired', run_id = ? where id = ? and status = 'pending' returning trigger_id")
                .bind(&run_id).bind(&occurrence_id).fetch_optional(&mut *connection).await.map_err(DatabaseError::Query)?;
            let Some(trigger_id) = trigger_id else { return Err(DatabaseError::Conflict { operation: "complete trigger" }); };
            sqlx::query("insert into trigger_checkpoint (trigger_id, checkpoint_json, updated_unix_ms) values (?, ?, ?) on conflict(trigger_id) do update set checkpoint_json = excluded.checkpoint_json, updated_unix_ms = excluded.updated_unix_ms")
                .bind(trigger_id).bind(checkpoint).bind(now_unix_ms).execute(connection).await.map_err(DatabaseError::Query)?;
            Ok(())
        })).await
    }

    pub(crate) async fn wait_on_gate(
        &self,
        step_id: &str,
        gate_kind: &str,
        due_unix_ms: i64,
        checkpoint_json: &str,
        now_unix_ms: i64,
    ) -> Result<(), DatabaseError> {
        let step_id = step_id.to_string();
        let gate_kind = gate_kind.to_string();
        let checkpoint = checkpoint_json.to_string();
        self.database.write_immediate(|connection| Box::pin(async move {
            let run_id: Option<String> = sqlx::query_scalar("update workflow_step set status = 'waiting', available_unix_ms = ? where id = ? and status in ('runnable','waiting') returning run_id")
                .bind(due_unix_ms).bind(&step_id).fetch_optional(&mut *connection).await.map_err(DatabaseError::Query)?;
            let Some(run_id) = run_id else { return Err(DatabaseError::Conflict { operation: "wait on gate" }); };
            sqlx::query("insert into gate_wait (step_id, gate_kind, due_unix_ms, checkpoint_json) values (?, ?, ?, ?) on conflict(step_id) do update set gate_kind = excluded.gate_kind, due_unix_ms = excluded.due_unix_ms, checkpoint_json = excluded.checkpoint_json")
                .bind(&step_id).bind(&gate_kind).bind(due_unix_ms).bind(&checkpoint).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
            sqlx::query("update workflow_run set status = 'waiting', updated_unix_ms = ? where id = ? and status = 'runnable' and not exists (select 1 from workflow_step where run_id = ? and status in ('runnable','claimed'))")
                .bind(now_unix_ms).bind(&run_id).bind(&run_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
            append_run_event(
                connection,
                &run_id,
                "gate_waiting",
                &serde_json::json!({"step_id": step_id, "gate_kind": gate_kind, "due_unix_ms": due_unix_ms}).to_string(),
                now_unix_ms,
            ).await
        })).await
    }

    pub(crate) async fn release_due_gates(
        &self,
        now_unix_ms: i64,
        limit: usize,
    ) -> Result<usize, DatabaseError> {
        let limit = i64::try_from(limit).map_err(|_| DatabaseError::InvalidValue {
            field: "gate batch limit",
            value: limit.to_string(),
        })?;
        self.database.write_immediate(|connection| Box::pin(async move {
            let step_ids: Vec<String> = sqlx::query_scalar("select step_id from gate_wait where due_unix_ms <= ? order by due_unix_ms, step_id limit ?")
                .bind(now_unix_ms).bind(limit).fetch_all(&mut *connection).await.map_err(DatabaseError::Query)?;
            for step_id in &step_ids {
                let run_id: Option<String> = sqlx::query_scalar("update workflow_step set status = 'runnable', available_unix_ms = ? where id = ? and status = 'waiting' returning run_id")
                    .bind(now_unix_ms).bind(step_id).fetch_optional(&mut *connection).await.map_err(DatabaseError::Query)?;
                let Some(run_id) = run_id else { return Err(DatabaseError::Conflict { operation: "release due gate" }); };
                sqlx::query("delete from gate_wait where step_id = ?").bind(step_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                sqlx::query("update workflow_run set status = 'runnable', updated_unix_ms = ? where id = ? and status = 'waiting'")
                    .bind(now_unix_ms).bind(&run_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                append_run_event(
                    connection,
                    &run_id,
                    "gate_released",
                    &serde_json::json!({"step_id": step_id}).to_string(),
                    now_unix_ms,
                ).await?;
            }
            Ok(step_ids.len())
        })).await
    }
}

async fn append_run_event(
    connection: &mut sqlx::SqliteConnection,
    run_id: &str,
    kind: &str,
    data_json: &str,
    now_unix_ms: i64,
) -> Result<(), DatabaseError> {
    sqlx::query("insert into audit_event (run_id, sequence, kind, time_unix_ms, data_json) select ?, coalesce(max(sequence), 0) + 1, ?, ?, ? from audit_event where run_id = ?")
        .bind(run_id)
        .bind(kind)
        .bind(now_unix_ms)
        .bind(data_json)
        .bind(run_id)
        .execute(connection)
        .await
        .map_err(DatabaseError::Query)?;
    Ok(())
}
