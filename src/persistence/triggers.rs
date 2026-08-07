use sha2::{Digest as _, Sha256};
use sqlx::FromRow;

use super::error::DatabaseError;
use super::pools::WorkflowDatabase;
use crate::workflow::trigger::{
    AdmissionDecision, OverlapPolicy, ProviderItemKind, ProviderPollPage, TriggerOccurrenceStatus,
    TriggerRegistration, TriggerSchedule,
};

#[derive(Clone)]
pub(crate) struct TriggerStore {
    database: WorkflowDatabase,
}

#[derive(Clone, Debug, PartialEq, Eq, FromRow)]
pub(crate) struct TriggerRow {
    pub id: String,
    pub definition_snapshot_id: String,
    pub trigger_kind: String,
    pub overlap_policy: String,
    pub schedule_json: String,
    pub config_json: String,
    pub admission_purpose: String,
    pub enabled: bool,
    pub created_unix_ms: i64,
    pub checkpoint_json: Option<String>,
    pub checkpoint_unix_ms: Option<i64>,
    pub schedule_last_due_unix_ms: Option<i64>,
    pub consecutive_failures: Option<i64>,
    pub retry_after_unix_ms: Option<i64>,
    pub poll_diagnostic: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, FromRow)]
pub(crate) struct TriggerHistoryRow {
    pub id: String,
    pub trigger_id: String,
    pub deduplication_key: String,
    pub due_unix_ms: i64,
    pub status: String,
    pub run_id: Option<String>,
    pub provider_item_id: Option<String>,
    pub observation_revision: Option<String>,
    pub diagnostic: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, FromRow)]
pub(crate) struct ProviderObservationRow {
    pub provider_item_id: String,
    pub item_kind: String,
    pub observation_revision: String,
    pub observation_json: String,
    pub observed_unix_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, FromRow)]
pub(crate) struct DispatchRow {
    pub child_run_id: String,
}

pub(crate) struct RecordDispatch<'a> {
    pub item_id: &'a str,
    pub observation_revision: &'a str,
    pub snapshot_id: &'a str,
    pub purpose: &'a str,
    pub intake_run_id: &'a str,
    pub child_run_id: &'a str,
    pub now_unix_ms: i64,
}

struct PreparedObservation {
    id: String,
    kind: ProviderItemKind,
    revision: String,
    body_json: String,
}

impl TriggerStore {
    pub(crate) fn new(database: WorkflowDatabase) -> Self {
        Self { database }
    }

    pub(crate) async fn configure(
        &self,
        registration: &TriggerRegistration,
        now_unix_ms: i64,
    ) -> Result<(), DatabaseError> {
        registration
            .schedule
            .validate()
            .map_err(|error| DatabaseError::InvalidValue {
                field: "trigger schedule",
                value: error.to_string(),
            })?;
        if registration.id.trim().is_empty() || registration.admission_purpose.trim().is_empty() {
            return Err(DatabaseError::InvalidValue {
                field: "trigger identity",
                value: "trigger ID and admission purpose must not be empty".into(),
            });
        }
        let id = registration.id.clone();
        let snapshot = registration.definition_snapshot_id.clone();
        let kind = registration.schedule.kind().to_string();
        let overlap = registration.overlap_policy.as_str().to_string();
        let schedule = serde_json::to_string(&registration.schedule).map_err(|error| {
            DatabaseError::InvalidValue {
                field: "trigger schedule",
                value: error.to_string(),
            }
        })?;
        let config = serde_json::json!({
            "repository": registration.repository,
            "inputs": registration.inputs,
        })
        .to_string();
        let purpose = registration.admission_purpose.clone();
        let enabled = registration.enabled;
        self.database.write_immediate(|connection| Box::pin(async move {
            sqlx::query("insert into trigger_definition (id,definition_snapshot_id,overlap_policy,config_json,enabled,trigger_kind,schedule_json,admission_purpose,created_unix_ms,updated_unix_ms) values (?,?,?,?,?,?,?,?,?,?) on conflict(id) do update set definition_snapshot_id=excluded.definition_snapshot_id, overlap_policy=excluded.overlap_policy, config_json=excluded.config_json, enabled=excluded.enabled, trigger_kind=excluded.trigger_kind, schedule_json=excluded.schedule_json, admission_purpose=excluded.admission_purpose, updated_unix_ms=excluded.updated_unix_ms")
                .bind(id).bind(snapshot).bind(overlap).bind(config).bind(enabled).bind(kind)
                .bind(schedule).bind(purpose).bind(now_unix_ms).bind(now_unix_ms)
                .execute(connection).await.map_err(DatabaseError::Query)?;
            Ok(())
        })).await
    }

    pub(crate) async fn list(&self) -> Result<Vec<TriggerRow>, DatabaseError> {
        sqlx::query_as("select definition.id, definition.definition_snapshot_id, definition.trigger_kind, definition.overlap_policy, definition.schedule_json, definition.config_json, definition.admission_purpose, definition.enabled, definition.created_unix_ms, checkpoint.checkpoint_json, checkpoint.updated_unix_ms as checkpoint_unix_ms, schedule.last_due_unix_ms as schedule_last_due_unix_ms, poll.consecutive_failures, poll.retry_after_unix_ms, poll.diagnostic as poll_diagnostic from trigger_definition definition left join trigger_checkpoint checkpoint on checkpoint.trigger_id=definition.id left join trigger_schedule_checkpoint schedule on schedule.trigger_id=definition.id left join provider_poll_state poll on poll.trigger_id=definition.id order by definition.id")
            .fetch_all(self.database.readers()).await.map_err(DatabaseError::Query)
    }

    pub(crate) async fn show(&self, id: &str) -> Result<Option<TriggerRow>, DatabaseError> {
        sqlx::query_as("select definition.id, definition.definition_snapshot_id, definition.trigger_kind, definition.overlap_policy, definition.schedule_json, definition.config_json, definition.admission_purpose, definition.enabled, definition.created_unix_ms, checkpoint.checkpoint_json, checkpoint.updated_unix_ms as checkpoint_unix_ms, schedule.last_due_unix_ms as schedule_last_due_unix_ms, poll.consecutive_failures, poll.retry_after_unix_ms, poll.diagnostic as poll_diagnostic from trigger_definition definition left join trigger_checkpoint checkpoint on checkpoint.trigger_id=definition.id left join trigger_schedule_checkpoint schedule on schedule.trigger_id=definition.id left join provider_poll_state poll on poll.trigger_id=definition.id where definition.id=?")
            .bind(id).fetch_optional(self.database.readers()).await.map_err(DatabaseError::Query)
    }

    pub(crate) async fn set_enabled(
        &self,
        id: &str,
        enabled: bool,
        now_unix_ms: i64,
    ) -> Result<(), DatabaseError> {
        let id = id.to_string();
        self.database
            .write_immediate(|connection| {
                Box::pin(async move {
                    let changed = sqlx::query(
                        "update trigger_definition set enabled=?, updated_unix_ms=? where id=?",
                    )
                    .bind(enabled)
                    .bind(now_unix_ms)
                    .bind(id)
                    .execute(connection)
                    .await
                    .map_err(DatabaseError::Query)?
                    .rows_affected();
                    if changed == 1 {
                        Ok(())
                    } else {
                        Err(DatabaseError::Conflict {
                            operation: "enable or disable Trigger",
                        })
                    }
                })
            })
            .await
    }

    pub(crate) async fn history(
        &self,
        id: &str,
        limit: usize,
    ) -> Result<Vec<TriggerHistoryRow>, DatabaseError> {
        let limit = i64::try_from(limit).map_err(|_| DatabaseError::InvalidValue {
            field: "trigger history limit",
            value: limit.to_string(),
        })?;
        sqlx::query_as("select id,trigger_id,deduplication_key,due_unix_ms,status,run_id,provider_item_id,observation_revision,diagnostic from trigger_occurrence where trigger_id=? order by due_unix_ms desc,id limit ?")
            .bind(id).bind(limit).fetch_all(self.database.readers()).await.map_err(DatabaseError::Query)
    }

    pub(crate) async fn materialize_due(
        &self,
        now_unix_ms: i64,
        limit: usize,
    ) -> Result<usize, DatabaseError> {
        let definitions = self.list().await?;
        let mut inserted = 0;
        for definition in definitions.into_iter().filter(|definition| {
            definition.enabled
                && definition
                    .retry_after_unix_ms
                    .is_none_or(|retry| retry <= now_unix_ms)
        }) {
            if inserted >= limit {
                break;
            }
            let schedule: TriggerSchedule = if definition.trigger_kind == "manual" {
                TriggerSchedule::Manual
            } else {
                serde_json::from_str(&definition.schedule_json).map_err(|error| {
                    DatabaseError::InvalidValue {
                        field: "trigger schedule",
                        value: error.to_string(),
                    }
                })?
            };
            let checkpoint = definition
                .schedule_last_due_unix_ms
                .unwrap_or_else(|| definition.created_unix_ms.saturating_sub(1));
            let due = schedule
                .due_between(checkpoint, now_unix_ms, limit - inserted)
                .map_err(|error| DatabaseError::InvalidValue {
                    field: "trigger schedule",
                    value: error.to_string(),
                })?;
            for due_unix_ms in due {
                let occurrence_id = format!("{}:scheduled:{due_unix_ms}", definition.id);
                let deduplication_key = format!(
                    "scheduled:{due_unix_ms}:definition:{}:purpose:{}",
                    definition.definition_snapshot_id, definition.admission_purpose
                );
                if self
                    .record_occurrence(
                        &occurrence_id,
                        &definition.id,
                        &deduplication_key,
                        due_unix_ms,
                    )
                    .await?
                {
                    inserted += 1;
                }
            }
        }
        Ok(inserted)
    }

    pub(crate) async fn run_now(
        &self,
        trigger_id: &str,
        occurrence_id: &str,
        now_unix_ms: i64,
    ) -> Result<bool, DatabaseError> {
        let definition = self
            .show(trigger_id)
            .await?
            .ok_or(DatabaseError::Conflict {
                operation: "run Trigger now",
            })?;
        let key = format!(
            "manual:{occurrence_id}:definition:{}:purpose:{}",
            definition.definition_snapshot_id, definition.admission_purpose
        );
        self.record_occurrence(occurrence_id, trigger_id, &key, now_unix_ms)
            .await
    }

    pub(crate) async fn record_startup(
        &self,
        worker_instance_id: &str,
        now_unix_ms: i64,
    ) -> Result<usize, DatabaseError> {
        let definitions = self.list().await?;
        let mut inserted = 0;
        for definition in definitions
            .into_iter()
            .filter(|definition| definition.enabled && definition.trigger_kind == "startup")
        {
            let occurrence_id = format!("{}:startup:{worker_instance_id}", definition.id);
            let key = format!(
                "startup:{worker_instance_id}:definition:{}:purpose:{}",
                definition.definition_snapshot_id, definition.admission_purpose
            );
            inserted += usize::from(
                self.record_occurrence(&occurrence_id, &definition.id, &key, now_unix_ms)
                    .await?,
            );
        }
        Ok(inserted)
    }

    pub(crate) async fn record_occurrence(
        &self,
        id: &str,
        trigger_id: &str,
        key: &str,
        due_unix_ms: i64,
    ) -> Result<bool, DatabaseError> {
        let values = (id.to_string(), trigger_id.to_string(), key.to_string());
        self.database.write_immediate(|connection| Box::pin(async move {
            let policy: String = sqlx::query_scalar("select overlap_policy from trigger_definition where id=?")
                .bind(&values.1).fetch_one(&mut *connection).await.map_err(DatabaseError::Query)?;
            let policy = OverlapPolicy::from_persisted(&policy).ok_or_else(|| DatabaseError::InvalidValue { field: "trigger overlap policy", value: policy })?;
            let status =
                apply_overlap_policy(connection, &values.1, None, policy, due_unix_ms).await?;
            let changed = sqlx::query("insert into trigger_occurrence (id,trigger_id,deduplication_key,due_unix_ms,status,created_unix_ms) values (?,?,?,?,?,?) on conflict(trigger_id,deduplication_key) do nothing")
                .bind(values.0).bind(values.1).bind(values.2).bind(due_unix_ms).bind(status.as_str()).bind(due_unix_ms)
                .execute(&mut *connection).await.map_err(DatabaseError::Query)?.rows_affected();
            Ok(changed == 1)
        })).await
    }

    /// Persists the entire provider page before advancing its opaque checkpoint.
    pub(crate) async fn record_provider_page(
        &self,
        page: &ProviderPollPage,
    ) -> Result<usize, DatabaseError> {
        let trigger_id = page.trigger_id.clone();
        let occurrence_id = page.occurrence_id.clone();
        let checkpoint = page.checkpoint.to_string();
        let observed = page.observed_unix_ms;
        let items = page
            .items
            .iter()
            .map(|item| {
                item.validate()
                    .map_err(|error| DatabaseError::InvalidValue {
                        field: "provider observation",
                        value: error.to_string(),
                    })?;
                Ok(PreparedObservation {
                    id: item.provider_item_id.clone(),
                    kind: item.kind,
                    revision: item.revision(),
                    body_json: serde_json::to_string(item).map_err(|error| {
                        DatabaseError::InvalidValue {
                            field: "provider observation",
                            value: error.to_string(),
                        }
                    })?,
                })
            })
            .collect::<Result<Vec<_>, DatabaseError>>()?;
        self.database.write_immediate(|connection| Box::pin(async move {
            let trigger: (String, String, String, String) = sqlx::query_as("select definition_snapshot_id,admission_purpose,overlap_policy,schedule_json from trigger_definition where id=? and trigger_kind='provider_poll' and enabled=1")
                .bind(&trigger_id).fetch_one(&mut *connection).await.map_err(DatabaseError::Query)?;
            let policy = OverlapPolicy::from_persisted(&trigger.2).ok_or_else(|| DatabaseError::InvalidValue { field: "trigger overlap policy", value: trigger.2.clone() })?;
            let expected_kind = match serde_json::from_str::<TriggerSchedule>(&trigger.3).map_err(|error| DatabaseError::InvalidValue { field: "trigger schedule", value: error.to_string() })? {
                TriggerSchedule::ProviderPoll { item_kind, .. } => item_kind,
                _ => return Err(DatabaseError::Conflict { operation: "persist provider page for non-provider Trigger" }),
            };
            let mut inserted = 0;
            for item in items {
                let changed = sqlx::query("insert into provider_item_observation (provider_item_id,item_kind,observation_revision,observation_json,trigger_id,occurrence_id,observed_unix_ms) values (?,?,?,?,?,?,?) on conflict(provider_item_id,observation_revision) do nothing")
                    .bind(&item.id).bind(item.kind.as_str()).bind(&item.revision).bind(&item.body_json).bind(&trigger_id).bind(&occurrence_id).bind(observed)
                    .execute(&mut *connection).await.map_err(DatabaseError::Query)?.rows_affected();
                inserted += changed;
                if changed == 1 && item.kind == expected_kind {
                    let occurrence_status = apply_overlap_policy(
                        connection,
                        &trigger_id,
                        Some(&item.id),
                        policy,
                        observed,
                    )
                    .await?;
                    let identity = format!("{}:{}:{}:{}", item.id, item.revision, trigger.0, trigger.1);
                    let occurrence_digest = format!("{:x}", Sha256::digest(identity.as_bytes()));
                    let intake_occurrence_id = format!("{trigger_id}:item:{occurrence_digest}");
                    let deduplication_key = format!("provider:{}:revision:{}:definition:{}:purpose:{}", item.id, item.revision, trigger.0, trigger.1);
                    let observation: serde_json::Value = serde_json::from_str(&item.body_json).map_err(|error| DatabaseError::InvalidValue { field: "provider observation", value: error.to_string() })?;
                    let item_binding = match item.kind { ProviderItemKind::Issue => "issue", ProviderItemKind::ChangeRequest => "change_request" };
                    let input_json = serde_json::json!({"intake": {
                        (item_binding): observation,
                        "observation_revision": item.revision,
                        "provenance": {"trigger_id": &trigger_id, "poll_occurrence_id": &occurrence_id},
                        "quarantined": true
                    }}).to_string();
                    sqlx::query("insert into trigger_occurrence (id,trigger_id,deduplication_key,due_unix_ms,status,provider_item_id,observation_revision,created_unix_ms,input_json) values (?,?,?,?,?,?,?,?,?) on conflict(trigger_id,deduplication_key) do nothing")
                        .bind(intake_occurrence_id).bind(&trigger_id).bind(deduplication_key).bind(observed).bind(occurrence_status.as_str())
                        .bind(&item.id).bind(&item.revision).bind(observed).bind(input_json)
                        .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                }
            }
            sqlx::query("insert into trigger_checkpoint (trigger_id,checkpoint_json,updated_unix_ms) values (?,?,?) on conflict(trigger_id) do update set checkpoint_json=excluded.checkpoint_json,updated_unix_ms=excluded.updated_unix_ms")
                .bind(&trigger_id).bind(checkpoint).bind(observed).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
            sqlx::query("delete from provider_poll_state where trigger_id=?")
                .bind(trigger_id).execute(connection).await.map_err(DatabaseError::Query)?;
            Ok(usize::try_from(inserted).unwrap_or(usize::MAX))
        })).await
    }

    pub(crate) async fn record_poll_failure(
        &self,
        trigger_id: &str,
        diagnostic: &str,
        now_unix_ms: i64,
        provider_retry_after_unix_ms: Option<i64>,
    ) -> Result<i64, DatabaseError> {
        let trigger_id = trigger_id.to_string();
        let diagnostic = diagnostic.chars().take(1024).collect::<String>();
        self.database.write_immediate(|connection| Box::pin(async move {
            let previous: i64 = sqlx::query_scalar("select coalesce((select consecutive_failures from provider_poll_state where trigger_id=?),0)")
                .bind(&trigger_id).fetch_one(&mut *connection).await.map_err(DatabaseError::Query)?;
            let failures = previous.saturating_add(1);
            let exponent = u32::try_from(failures.saturating_sub(1).min(10)).unwrap_or(10);
            let backoff_ms = 1_000_i64.saturating_mul(2_i64.saturating_pow(exponent)).min(3_600_000);
            let retry_after = provider_retry_after_unix_ms.unwrap_or_else(|| now_unix_ms.saturating_add(backoff_ms)).max(now_unix_ms);
            sqlx::query("insert into provider_poll_state (trigger_id,consecutive_failures,retry_after_unix_ms,diagnostic,updated_unix_ms) values (?,?,?,?,?) on conflict(trigger_id) do update set consecutive_failures=excluded.consecutive_failures,retry_after_unix_ms=excluded.retry_after_unix_ms,diagnostic=excluded.diagnostic,updated_unix_ms=excluded.updated_unix_ms")
                .bind(trigger_id).bind(failures).bind(retry_after).bind(diagnostic).bind(now_unix_ms)
                .execute(connection).await.map_err(DatabaseError::Query)?;
            Ok(retry_after)
        })).await
    }

    pub(crate) async fn complete_poll_occurrence(
        &self,
        occurrence_id: &str,
        now_unix_ms: i64,
    ) -> Result<(), DatabaseError> {
        let occurrence_id = occurrence_id.to_string();
        self.database.write_immediate(|connection| Box::pin(async move {
            let occurrence: Option<(String, i64)> = sqlx::query_as("update trigger_occurrence set status='fired',completed_unix_ms=? where id=? and status='pending' returning trigger_id,due_unix_ms")
                .bind(now_unix_ms).bind(occurrence_id).fetch_optional(&mut *connection).await.map_err(DatabaseError::Query)?;
            let Some((trigger_id, due_unix_ms)) = occurrence else { return Err(DatabaseError::Conflict { operation: "complete provider poll occurrence" }); };
            sqlx::query("insert into trigger_schedule_checkpoint (trigger_id,last_due_unix_ms,updated_unix_ms) values (?,?,?) on conflict(trigger_id) do update set last_due_unix_ms=max(trigger_schedule_checkpoint.last_due_unix_ms,excluded.last_due_unix_ms),updated_unix_ms=excluded.updated_unix_ms")
                .bind(trigger_id).bind(due_unix_ms).bind(now_unix_ms).execute(connection).await.map_err(DatabaseError::Query)?;
            Ok(())
        })).await
    }

    pub(crate) async fn defer_poll_occurrence(
        &self,
        occurrence_id: &str,
        retry_after_unix_ms: i64,
        diagnostic: &str,
    ) -> Result<(), DatabaseError> {
        let occurrence_id = occurrence_id.to_string();
        let diagnostic = diagnostic.to_string();
        self.database.write_immediate(|connection| Box::pin(async move {
            let changed = sqlx::query("update trigger_occurrence set due_unix_ms=?,diagnostic=? where id=? and status='pending'")
                .bind(retry_after_unix_ms).bind(diagnostic).bind(occurrence_id)
                .execute(&mut *connection).await.map_err(DatabaseError::Query)?.rows_affected();
            if changed == 1 { Ok(()) } else { Err(DatabaseError::Conflict { operation: "defer provider poll occurrence" }) }
        })).await
    }

    pub(crate) async fn latest_observation(
        &self,
        item_id: &str,
    ) -> Result<Option<ProviderObservationRow>, DatabaseError> {
        sqlx::query_as("select provider_item_id,item_kind,observation_revision,observation_json,observed_unix_ms from provider_item_observation where provider_item_id=? order by observed_unix_ms desc,rowid desc limit 1")
            .bind(item_id).fetch_optional(self.database.readers()).await.map_err(DatabaseError::Query)
    }

    pub(crate) async fn decide_admission(
        &self,
        decision: &AdmissionDecision,
    ) -> Result<(), DatabaseError> {
        let id = decision.id.clone();
        let item = decision.provider_item_id.clone();
        let revision = decision.observation_revision.clone();
        let purpose = decision.purpose.clone();
        let outcome = decision.outcome.as_str().to_string();
        let authority = serde_json::to_string(&decision.authority).map_err(|error| {
            DatabaseError::InvalidValue {
                field: "admission authority",
                value: error.to_string(),
            }
        })?;
        let evidence = decision.evidence.to_string();
        let decided_by = decision.decided_by.clone();
        let decided = decision.decided_unix_ms;
        self.database.write_immediate(|connection| Box::pin(async move {
            let current: Option<String> = sqlx::query_scalar("select observation_revision from provider_item_observation where provider_item_id=? order by observed_unix_ms desc,rowid desc limit 1")
                .bind(&item).fetch_optional(&mut *connection).await.map_err(DatabaseError::Query)?;
            if current.as_deref() != Some(revision.as_str()) {
                return Err(DatabaseError::Conflict { operation: "record admission against current Observation Revision" });
            }
            sqlx::query("insert into admission_decision (id,provider_item_id,observation_revision,purpose,outcome,authority_json,evidence_json,decided_by,decided_unix_ms) values (?,?,?,?,?,?,?,?,?)")
                .bind(id).bind(item).bind(revision).bind(purpose).bind(outcome).bind(authority).bind(evidence).bind(decided_by).bind(decided)
                .execute(connection).await.map_err(DatabaseError::Query)?;
            Ok(())
        })).await
    }

    pub(crate) async fn admitted_authority(
        &self,
        item_id: &str,
        observation_revision: &str,
        purpose: &str,
    ) -> Result<Option<String>, DatabaseError> {
        sqlx::query_scalar("select decision.authority_json from admission_decision decision join provider_item_observation observation on observation.provider_item_id=decision.provider_item_id and observation.observation_revision=decision.observation_revision where decision.provider_item_id=? and decision.observation_revision=? and decision.purpose=? and decision.outcome='admitted' and not exists (select 1 from provider_item_observation newer where newer.provider_item_id=observation.provider_item_id and (newer.observed_unix_ms>observation.observed_unix_ms or (newer.observed_unix_ms=observation.observed_unix_ms and newer.rowid>observation.rowid)))")
            .bind(item_id).bind(observation_revision).bind(purpose).fetch_optional(self.database.readers()).await.map_err(DatabaseError::Query)
    }

    pub(crate) async fn active_dispatch(
        &self,
        item_id: &str,
        purpose: &str,
    ) -> Result<Option<String>, DatabaseError> {
        sqlx::query_scalar("select dispatch.child_run_id from implementation_dispatch dispatch join workflow_run run on run.id=dispatch.child_run_id where dispatch.provider_item_id=? and dispatch.purpose=? and run.status in ('waiting','runnable','running','paused') order by dispatch.created_unix_ms desc limit 1")
            .bind(item_id).bind(purpose).fetch_optional(self.database.readers()).await.map_err(DatabaseError::Query)
    }

    pub(crate) async fn attach_input_provenance(
        &self,
        run_id: &str,
        item_id: &str,
        observation_revision: &str,
        purpose: &str,
    ) -> Result<(), DatabaseError> {
        let values = (
            run_id.to_string(),
            item_id.to_string(),
            observation_revision.to_string(),
            purpose.to_string(),
        );
        self.database.write_immediate(|connection| Box::pin(async move {
            let changed = sqlx::query("insert into artifact_provenance (artifact_id,provider_item_id,observation_revision,trigger_occurrence_id,admission_decision_id) select binding.artifact_id,observation.provider_item_id,observation.observation_revision,observation.occurrence_id,decision.id from workflow_input_binding binding join provider_item_observation observation on observation.provider_item_id=? and observation.observation_revision=? join admission_decision decision on decision.provider_item_id=observation.provider_item_id and decision.observation_revision=observation.observation_revision and decision.purpose=? and decision.outcome='admitted' where binding.run_id=? on conflict(artifact_id) do nothing")
                .bind(&values.1).bind(&values.2).bind(&values.3).bind(&values.0)
                .execute(&mut *connection).await.map_err(DatabaseError::Query)?.rows_affected();
            if changed == 0 {
                let source: Option<(String, Option<String>, String)> = sqlx::query_as("select observation.observation_json,observation.occurrence_id,decision.id from provider_item_observation observation join admission_decision decision on decision.provider_item_id=observation.provider_item_id and decision.observation_revision=observation.observation_revision where observation.provider_item_id=? and observation.observation_revision=? and decision.purpose=? and decision.outcome='admitted'")
                    .bind(&values.1).bind(&values.2).bind(&values.3).fetch_optional(&mut *connection).await.map_err(DatabaseError::Query)?;
                let Some((body, occurrence_id, decision_id)) = source else {
                    return Err(DatabaseError::Conflict { operation: "attach admitted input Artifact provenance" });
                };
                let artifact_id = format!("{}:provider-intake", values.0);
                sqlx::query("insert into artifact (id,run_id,revision,digest,size_bytes,sensitivity,inline_body,created_unix_ms) select ?,?,1,?,?, 'untrusted',?,created_unix_ms from workflow_run where id=? on conflict(id) do nothing")
                    .bind(&artifact_id).bind(&values.0).bind(&values.2).bind(i64::try_from(body.len()).unwrap_or(i64::MAX)).bind(body.as_bytes()).bind(&values.0)
                    .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                sqlx::query("insert into artifact_provenance (artifact_id,provider_item_id,observation_revision,trigger_occurrence_id,admission_decision_id) values (?,?,?,?,?) on conflict(artifact_id) do nothing")
                    .bind(artifact_id).bind(values.1).bind(values.2).bind(occurrence_id).bind(decision_id)
                    .execute(connection).await.map_err(DatabaseError::Query)?;
            }
            Ok(())
        })).await
    }

    pub(crate) async fn dispatch(
        &self,
        command: RecordDispatch<'_>,
    ) -> Result<String, DatabaseError> {
        let values = (
            command.item_id.to_string(),
            command.observation_revision.to_string(),
            command.snapshot_id.to_string(),
            command.purpose.to_string(),
            command.intake_run_id.to_string(),
            command.child_run_id.to_string(),
        );
        let now_unix_ms = command.now_unix_ms;
        self.database.write_immediate(|connection| Box::pin(async move {
            let active: Option<String> = sqlx::query_scalar("select dispatch.child_run_id from implementation_dispatch dispatch join workflow_run run on run.id=dispatch.child_run_id where dispatch.provider_item_id=? and dispatch.purpose=? and run.status in ('waiting','runnable','running','paused') order by dispatch.created_unix_ms desc limit 1")
                .bind(&values.0).bind(&values.3).fetch_optional(&mut *connection).await.map_err(DatabaseError::Query)?;
            if let Some(active) = active {
                return Ok(active);
            }
            sqlx::query("insert into implementation_dispatch (provider_item_id,observation_revision,definition_snapshot_id,purpose,intake_run_id,child_run_id,created_unix_ms) values (?,?,?,?,?,?,?) on conflict(provider_item_id,observation_revision,definition_snapshot_id,purpose) do nothing")
                .bind(&values.0).bind(&values.1).bind(&values.2).bind(&values.3).bind(&values.4).bind(&values.5).bind(now_unix_ms)
                .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
            let row: DispatchRow = sqlx::query_as("select child_run_id from implementation_dispatch where provider_item_id=? and observation_revision=? and definition_snapshot_id=? and purpose=?")
                .bind(values.0).bind(values.1).bind(values.2).bind(values.3).fetch_one(connection).await.map_err(DatabaseError::Query)?;
            Ok(row.child_run_id)
        })).await
    }
}

async fn apply_overlap_policy(
    connection: &mut sqlx::SqliteConnection,
    trigger_id: &str,
    provider_item_id: Option<&str>,
    policy: OverlapPolicy,
    now_unix_ms: i64,
) -> Result<TriggerOccurrenceStatus, DatabaseError> {
    let active: i64 = sqlx::query_scalar("select exists(select 1 from trigger_occurrence occurrence join workflow_run run on run.id=occurrence.run_id where occurrence.trigger_id=? and (? is null or occurrence.provider_item_id=?) and run.status in ('waiting','runnable','running','paused'))")
        .bind(trigger_id).bind(provider_item_id).bind(provider_item_id).fetch_one(&mut *connection).await.map_err(DatabaseError::Query)?;
    if let Some(replacement) = policy.replacement_status() {
        sqlx::query("update trigger_occurrence set status=? where trigger_id=? and (? is null or provider_item_id=?) and status='pending'")
        .bind(replacement.as_str())
        .bind(trigger_id)
        .bind(provider_item_id)
        .bind(provider_item_id)
        .execute(&mut *connection)
        .await
        .map_err(DatabaseError::Query)?;
    }
    if policy == OverlapPolicy::Supersede && active == 1 {
        sqlx::query("update workflow_step set status='cancelled',runtime_status='cancelled' where run_id in (select run.id from trigger_occurrence occurrence join workflow_run run on run.id=occurrence.run_id where occurrence.trigger_id=? and (? is null or occurrence.provider_item_id=?) and run.status in ('waiting','runnable','running','paused')) and status in ('waiting','runnable','claimed')")
            .bind(trigger_id).bind(provider_item_id).bind(provider_item_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
        sqlx::query("update workflow_run set status='cancelled',runtime_status='cancelled',updated_unix_ms=?,completed_unix_ms=? where id in (select occurrence.run_id from trigger_occurrence occurrence where occurrence.trigger_id=? and (? is null or occurrence.provider_item_id=?)) and status in ('waiting','runnable','running','paused')")
            .bind(now_unix_ms).bind(now_unix_ms).bind(trigger_id).bind(provider_item_id).bind(provider_item_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
        sqlx::query("update trigger_occurrence set status='superseded',completed_unix_ms=? where trigger_id=? and (? is null or provider_item_id=?) and run_id is not null and status='fired'")
            .bind(now_unix_ms).bind(trigger_id).bind(provider_item_id).bind(provider_item_id).execute(connection).await.map_err(DatabaseError::Query)?;
    }
    Ok(if policy == OverlapPolicy::Coalesce && active == 1 {
        TriggerOccurrenceStatus::Coalesced
    } else {
        TriggerOccurrenceStatus::Pending
    })
}
