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
    pub trigger_kind: String,
    pub schedule_json: String,
    pub checkpoint_json: Option<String>,
    pub input_json: Option<String>,
    pub provider_item_id: Option<String>,
    pub deduplication_key: String,
    pub due_unix_ms: i64,
}

impl WakeupStore {
    pub(crate) fn new(database: WorkflowDatabase) -> Self {
        Self { database }
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
        _checkpoint_json: &str,
        now_unix_ms: i64,
    ) -> Result<(), DatabaseError> {
        let occurrence_id = occurrence_id.to_string();
        let run_id = run_id.to_string();
        self.database.write_immediate(|connection| Box::pin(async move {
            let occurrence: Option<(String, i64, Option<String>)> = sqlx::query_as("update trigger_occurrence set status = 'fired', run_id = ?, completed_unix_ms=? where id = ? and status = 'pending' returning trigger_id,due_unix_ms,provider_item_id")
                .bind(&run_id).bind(now_unix_ms).bind(&occurrence_id).fetch_optional(&mut *connection).await.map_err(DatabaseError::Query)?;
            let Some((trigger_id, due_unix_ms, provider_item_id)) = occurrence else { return Err(DatabaseError::Conflict { operation: "complete trigger" }); };
            if provider_item_id.is_none() {
                sqlx::query("insert into trigger_schedule_checkpoint (trigger_id,last_due_unix_ms,updated_unix_ms) values (?,?,?) on conflict(trigger_id) do update set last_due_unix_ms=max(trigger_schedule_checkpoint.last_due_unix_ms,excluded.last_due_unix_ms),updated_unix_ms=excluded.updated_unix_ms")
                    .bind(trigger_id).bind(due_unix_ms).bind(now_unix_ms).execute(connection).await.map_err(DatabaseError::Query)?;
            }
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
            let run_id: Option<String> = sqlx::query_scalar("update workflow_step set status = 'waiting', runtime_status = 'waiting_gate', available_unix_ms = ? where id = ? and status in ('runnable','waiting') returning run_id")
                .bind(due_unix_ms).bind(&step_id).fetch_optional(&mut *connection).await.map_err(DatabaseError::Query)?;
            let Some(run_id) = run_id else { return Err(DatabaseError::Conflict { operation: "wait on gate" }); };
            sqlx::query("insert into gate_wait (step_id, gate_kind, due_unix_ms, checkpoint_json) values (?, ?, ?, ?) on conflict(step_id) do update set gate_kind = excluded.gate_kind, due_unix_ms = excluded.due_unix_ms, checkpoint_json = excluded.checkpoint_json")
                .bind(&step_id).bind(&gate_kind).bind(due_unix_ms).bind(&checkpoint).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
            sqlx::query("update workflow_run set status = 'waiting', runtime_status = 'waiting', updated_unix_ms = ? where id = ? and status = 'runnable' and not exists (select 1 from workflow_step where run_id = ? and status in ('runnable','claimed'))")
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
                let run_id: Option<String> = sqlx::query_scalar("update workflow_step set status = case when class = 'wait' then 'succeeded' else 'runnable' end, runtime_status = case when class = 'wait' then 'succeeded' else 'runnable' end, available_unix_ms = ? where id = ? and status = 'waiting' returning run_id")
                    .bind(now_unix_ms).bind(step_id).fetch_optional(&mut *connection).await.map_err(DatabaseError::Query)?;
                let Some(run_id) = run_id else { return Err(DatabaseError::Conflict { operation: "release due gate" }); };
                sqlx::query("delete from gate_wait where step_id = ?").bind(step_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                sqlx::query("update workflow_run set status = 'runnable', runtime_status='runnable', updated_unix_ms = ? where id = ? and status = 'waiting'")
                    .bind(now_unix_ms).bind(&run_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                let unfinished: i64 = sqlx::query_scalar("select count(*) from workflow_step where run_id = ? and status <> 'succeeded'")
                    .bind(&run_id).fetch_one(&mut *connection).await.map_err(DatabaseError::Query)?;
                if unfinished == 0 {
                    sqlx::query("update workflow_run set status = 'succeeded', runtime_status = 'succeeded', completed_unix_ms = ?, updated_unix_ms = ? where id = ?")
                        .bind(now_unix_ms).bind(now_unix_ms).bind(&run_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                }
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
