#![allow(dead_code)]

use std::collections::BTreeSet;
use std::str::FromStr;

use chrono::{DateTime, TimeZone, Utc};
use chrono_tz::Tz;
use cron::Schedule;
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::remote::ProviderItemObservationState;
use crate::run::{RunId, RunLedger, now_ms, random_id, sha256};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OverlapPolicy {
    Coalesce,
    Supersede,
    Queue,
    Parallel,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MissedOccurrencePolicy {
    Skip,
    Latest,
    AllBounded,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum TriggerKind {
    Manual,
    Schedule {
        expression: String,
        timezone: String,
        missed: MissedOccurrencePolicy,
    },
    ProviderEvent {
        repository: String,
        event: String,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct TriggerDefinition {
    pub id: String,
    #[serde(default)]
    pub enabled: bool,
    pub definition_selector: String,
    pub admission_purpose: String,
    pub kind: TriggerKind,
    pub overlap: OverlapPolicy,
    pub max_fan_out: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct TriggerOccurrence {
    pub id: String,
    pub trigger_id: String,
    pub occurrence_key: String,
    pub run_id: Option<RunId>,
    pub state: String,
    pub created: bool,
    pub created_unix_ms: i64,
}

#[derive(Clone, Debug)]
pub(crate) struct OccurrenceIdentity<'a> {
    pub native_occurrence: &'a str,
    pub provider_item: Option<&'a str>,
    pub observation_revision: Option<&'a str>,
    pub definition_digest: &'a str,
    pub input_digest: &'a str,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct DueOccurrence {
    pub native_occurrence: String,
    pub scheduled_unix_ms: i64,
    pub local_time: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct TriggerStatus {
    pub definition: TriggerDefinition,
    pub snapshot_digest: String,
    pub enabled: bool,
    pub next_run_unix_ms: Option<i64>,
    pub checkpoint: Option<String>,
    pub recent_occurrences: Vec<TriggerOccurrence>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LaunchDisposition {
    Launch,
    Coalesced(RunId),
    Supersede(Vec<RunId>),
    Queued,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct AuthenticatedProviderFacts {
    pub host: String,
    pub repository: String,
    pub event: String,
    pub actor_relationship: Option<String>,
    pub label_ids: BTreeSet<String>,
    pub observation_revision: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct AdmissionPolicy {
    pub revision: String,
    pub hosts: BTreeSet<String>,
    pub repositories: BTreeSet<String>,
    pub events: BTreeSet<String>,
    pub actor_relationships: BTreeSet<String>,
    pub required_label_ids: BTreeSet<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AdvisoryClassification {
    Acceptable,
    Reject,
    Unknown,
}

#[derive(Clone, Debug)]
pub(crate) struct DecideAdmission<'a> {
    pub run_id: &'a RunId,
    pub provider_item: &'a str,
    pub purpose: &'a str,
    pub policy: &'a AdmissionPolicy,
    pub facts: &'a AuthenticatedProviderFacts,
    pub advisory: AdvisoryClassification,
    pub capability_envelope: &'a BTreeSet<String>,
    pub actor: &'a str,
    pub expires_unix_ms: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct AdmissionDecision {
    pub id: String,
    pub run_id: RunId,
    pub provider_item: String,
    pub observation_revision: String,
    pub policy_revision: String,
    pub purpose: String,
    pub admitted: bool,
    pub actor: String,
    pub expires_unix_ms: Option<i64>,
}

impl AdmissionPolicy {
    /// Only provider-authenticated normalized facts can authorize admission.
    /// Agent classification is advisory and can only make the outcome stricter.
    pub(crate) fn evaluate(
        &self,
        facts: &AuthenticatedProviderFacts,
        advisory: AdvisoryClassification,
    ) -> bool {
        let deterministic = self.hosts.contains(&facts.host)
            && self.repositories.contains(&facts.repository)
            && self.events.contains(&facts.event)
            && (self.actor_relationships.is_empty()
                || facts
                    .actor_relationship
                    .as_ref()
                    .is_some_and(|value| self.actor_relationships.contains(value)))
            && self.required_label_ids.is_subset(&facts.label_ids)
            && !facts.observation_revision.is_empty();
        deterministic && advisory != AdvisoryClassification::Reject
    }
}

#[derive(Clone)]
pub(crate) struct TriggerEngine {
    ledger: RunLedger,
}

impl TriggerEngine {
    pub(crate) fn new(ledger: RunLedger) -> Result<Self, String> {
        let engine = Self { ledger };
        engine.ensure_schema()?;
        Ok(engine)
    }

    fn ensure_schema(&self) -> Result<(), String> {
        self.ledger.connection()?.execute_batch(
            "create table if not exists trigger_checkpoint (trigger_id text primary key references trigger(id), checkpoint_json text not null, updated_unix_ms integer not null);
             create index if not exists trigger_occurrence_page on trigger_occurrence(trigger_id,created_unix_ms,id);
             create table if not exists trigger_definition_snapshot (
               digest text primary key, canonical_json text not null, created_unix_ms integer not null
             );
             create table if not exists trigger_revision (
               trigger_id text primary key references trigger(id), snapshot_digest text not null references trigger_definition_snapshot(digest)
             );
             create table if not exists trigger_occurrence_detail (
               occurrence_id text primary key references trigger_occurrence(id) on delete cascade,
               native_occurrence text not null, provider_item text, observation_revision text,
               definition_digest text not null, input_digest text not null, admission_purpose text not null,
               state text not null default 'pending'
             );
             create index if not exists trigger_item_active on trigger_occurrence_detail(provider_item,admission_purpose,state);
             create table if not exists provider_item_observation (
               provider_item text not null, observation_revision text not null, observation_json text not null,
               state text not null, safe_error text, observed_unix_ms integer not null,
               primary key(provider_item,observation_revision)
             );
             create table if not exists provider_item_current (
               provider_item text primary key, observation_revision text, state text not null,
               safe_error text, updated_unix_ms integer not null
             );
             create table if not exists workflow_admission_record (
               id text primary key, run_id text not null references workflow_run(id), provider_item text not null,
               observation_revision text not null, policy_revision text not null, purpose text not null,
               authenticated_facts_json text not null, advisory text not null, capability_envelope_json text not null,
               actor text not null, outcome text not null, expires_unix_ms integer, created_unix_ms integer not null,
               unique(run_id,provider_item,observation_revision,policy_revision,purpose)
             );",
        ).map_err(|error| error.to_string())
    }

    pub(crate) fn register(
        &self,
        definition: &TriggerDefinition,
        enabled: bool,
    ) -> Result<(), String> {
        validate_definition(definition)?;
        let json = serde_json::to_string(definition).map_err(|error| error.to_string())?;
        let conn = self.ledger.connection()?;
        let snapshot_digest = sha256(json.as_bytes());
        conn.execute("insert or ignore into trigger_definition_snapshot(digest,canonical_json,created_unix_ms) values(?1,?2,?3)",params![snapshot_digest,json,now_ms()]).map_err(|error|error.to_string())?;
        if let Some(existing) = conn
            .query_row(
                "select definition_json from trigger where id=?1",
                [&definition.id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| error.to_string())?
        {
            if existing != json {
                return Err(format!(
                    "Trigger '{}' already exists with a different snapshot; register a new identity",
                    definition.id
                ));
            }
            // Explicit operator enable/disable state wins over repeated source
            // discovery. Source enablement is applied only on first registration.
            let _ = enabled;
        } else {
            conn.execute("insert into trigger(id,definition_json,enabled,created_unix_ms) values(?1,?2,?3,?4)", params![definition.id,json,enabled,now_ms()]).map_err(|error| error.to_string())?;
        }
        conn.execute(
            "insert or replace into trigger_revision(trigger_id,snapshot_digest) values(?1,?2)",
            params![definition.id, snapshot_digest],
        )
        .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub(crate) fn set_enabled(&self, id: &str, enabled: bool) -> Result<(), String> {
        let changed = self
            .ledger
            .connection()?
            .execute(
                "update trigger set enabled=?2 where id=?1",
                params![id, enabled],
            )
            .map_err(|error| error.to_string())?;
        (changed == 1)
            .then_some(())
            .ok_or_else(|| format!("Trigger '{id}' was not found"))
    }

    /// Evaluates a cron expression in its declared IANA timezone. The cron
    /// crate performs calendar iteration and chrono-tz owns DST ambiguity and
    /// nonexistent-local-time behavior; Prism does not hand-roll either.
    pub(crate) fn evaluate_due(
        &self,
        trigger_id: &str,
        after_unix_ms: i64,
        through_unix_ms: i64,
    ) -> Result<Vec<DueOccurrence>, String> {
        let started = std::time::Instant::now();
        if through_unix_ms < after_unix_ms {
            return Err("Trigger evaluation interval is reversed".to_string());
        }
        let definition = self.definition(trigger_id, true)?;
        let TriggerKind::Schedule {
            expression,
            timezone,
            missed,
        } = &definition.kind
        else {
            return Err(format!("Trigger '{trigger_id}' is not scheduled"));
        };
        let schedule = parse_schedule(expression)?;
        let timezone = parse_timezone(timezone)?;
        let after = utc_millis(after_unix_ms)?.with_timezone(&timezone);
        let through = utc_millis(through_unix_ms)?.with_timezone(&timezone);
        let cap = definition.max_fan_out as usize;
        // Keep one extra occurrence to detect bounded fan-out rather than
        // silently walking an unbounded downtime interval.
        let mut due = schedule
            .after(&after)
            .take_while(|value| value <= &through)
            .take(cap.saturating_add(1))
            .map(|value| DueOccurrence {
                native_occurrence: value.to_rfc3339(),
                scheduled_unix_ms: value.timestamp_millis(),
                local_time: value.to_rfc3339(),
            })
            .collect::<Vec<_>>();
        let due = match missed {
            MissedOccurrencePolicy::AllBounded => {
                due.truncate(cap);
                due
            }
            MissedOccurrencePolicy::Latest => due.pop().into_iter().collect(),
            MissedOccurrencePolicy::Skip => due
                .pop()
                .filter(|value| through_unix_ms.saturating_sub(value.scheduled_unix_ms) < 60_000)
                .into_iter()
                .collect(),
        };
        crate::flight_recorder::record(
            "workflow_trigger",
            "evaluate_due",
            Some(started.elapsed()),
            vec![
                crate::flight_recorder::unsigned("due_count", due.len()),
                crate::flight_recorder::boolean("bounded", due.len() <= cap),
            ],
        );
        Ok(due)
    }

    pub(crate) fn next_run(
        &self,
        trigger_id: &str,
        after_unix_ms: i64,
    ) -> Result<Option<i64>, String> {
        let definition = self.definition(trigger_id, false)?;
        let TriggerKind::Schedule {
            expression,
            timezone,
            ..
        } = definition.kind
        else {
            return Ok(None);
        };
        let after = utc_millis(after_unix_ms)?.with_timezone(&parse_timezone(&timezone)?);
        Ok(parse_schedule(&expression)?
            .after(&after)
            .next()
            .map(|value| value.timestamp_millis()))
    }

    /// Persists delivery before launch. Repeating the complete identity returns
    /// the existing occurrence; changing any consumed revision creates a new key.
    pub(crate) fn record_occurrence(
        &self,
        trigger_id: &str,
        identity: OccurrenceIdentity<'_>,
    ) -> Result<TriggerOccurrence, String> {
        let started = std::time::Instant::now();
        let definition = self.definition(trigger_id, true)?;
        if matches!(definition.kind, TriggerKind::ProviderEvent { .. })
            && (identity.provider_item.is_none() || identity.observation_revision.is_none())
        {
            return Err(
                "provider occurrence requires canonical item and Observation Revision".to_string(),
            );
        }
        let fields = serde_json::json!({
            // Provider transports may redeliver the same item revision under a
            // different poll/event ID. Item revision, not delivery, is its
            // idempotency identity.
            "native": identity.provider_item.is_none().then_some(identity.native_occurrence),
            "item": identity.provider_item,
            "observation": identity.observation_revision,
            "definition": identity.definition_digest,
            "purpose": definition.admission_purpose,
            "input": identity.input_digest,
            "trigger_definition": sha256(serde_json::to_string(&definition).map_err(|error|error.to_string())?.as_bytes()),
        });
        let occurrence_key =
            sha256(&serde_json::to_vec(&fields).map_err(|error| error.to_string())?);
        let mut conn = self.ledger.connection()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let id = random_id(&tx)?;
        let created_at = now_ms();
        let changed = tx.execute(
            "insert or ignore into trigger_occurrence(id,trigger_id,occurrence_key,created_unix_ms) values(?1,?2,?3,?4)",
            params![id,trigger_id,occurrence_key,created_at],
        ).map_err(|error| error.to_string())?;
        let (id, run_id, existing_created): (String, Option<String>, i64) = tx.query_row(
            "select id,run_id,created_unix_ms from trigger_occurrence where trigger_id=?1 and occurrence_key=?2",
            params![trigger_id,occurrence_key],
            |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
        ).map_err(|error| error.to_string())?;
        if changed == 1 {
            tx.execute(
                "insert into trigger_occurrence_detail(occurrence_id,native_occurrence,provider_item,observation_revision,definition_digest,input_digest,admission_purpose,state) values(?1,?2,?3,?4,?5,?6,?7,'pending')",
                params![id,identity.native_occurrence,identity.provider_item,identity.observation_revision,identity.definition_digest,identity.input_digest,definition.admission_purpose],
            ).map_err(|error| error.to_string())?;
        }
        tx.commit().map_err(|error| error.to_string())?;
        let state = if run_id.is_some() {
            "launched"
        } else {
            "pending"
        }
        .to_string();
        crate::flight_recorder::record(
            "workflow_trigger",
            "record_occurrence",
            Some(started.elapsed()),
            vec![
                crate::flight_recorder::boolean("created", changed == 1),
                crate::flight_recorder::boolean("provider_item", identity.provider_item.is_some()),
            ],
        );
        Ok(TriggerOccurrence {
            id,
            trigger_id: trigger_id.to_string(),
            occurrence_key,
            run_id: run_id.map(RunId),
            state,
            created: changed == 1,
            created_unix_ms: existing_created,
        })
    }

    /// Applies overlap policy before launch. Provider coalescing is scoped to
    /// canonical item plus admission purpose, so unrelated purposes can proceed.
    pub(crate) fn launch_disposition(
        &self,
        occurrence_id: &str,
    ) -> Result<LaunchDisposition, String> {
        let conn = self.ledger.connection()?;
        let (trigger_id, item, purpose): (String, Option<String>, String) = conn.query_row(
            "select occurrence.trigger_id,detail.provider_item,detail.admission_purpose from trigger_occurrence occurrence join trigger_occurrence_detail detail on detail.occurrence_id=occurrence.id where occurrence.id=?1",
            [occurrence_id],
            |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
        ).optional().map_err(|error| error.to_string())?.ok_or_else(|| "Trigger occurrence was not found".to_string())?;
        let definition = self.definition(&trigger_id, true)?;
        let mut statement = conn.prepare(
            "select distinct run.id from trigger_occurrence occurrence join trigger_occurrence_detail detail on detail.occurrence_id=occurrence.id join workflow_run run on run.id=occurrence.run_id where occurrence.id!=?1 and ((?2 is null and occurrence.trigger_id=?3) or detail.provider_item=?2) and detail.admission_purpose=?4 and run.state not in ('completed','failed','cancelled') order by run.created_unix_ms"
        ).map_err(|error| error.to_string())?;
        let active = statement
            .query_map(params![occurrence_id, item, trigger_id, purpose], |row| {
                row.get::<_, String>(0)
            })
            .map_err(|error| error.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(RunId)
            .collect::<Vec<_>>();
        if active.is_empty() || definition.overlap == OverlapPolicy::Parallel {
            return Ok(LaunchDisposition::Launch);
        }
        Ok(match definition.overlap {
            OverlapPolicy::Coalesce => LaunchDisposition::Coalesced(active[0].clone()),
            OverlapPolicy::Supersede => LaunchDisposition::Supersede(active),
            OverlapPolicy::Queue => LaunchDisposition::Queued,
            OverlapPolicy::Parallel => unreachable!(),
        })
    }

    pub(crate) fn attach_run(&self, occurrence_id: &str, run_id: &RunId) -> Result<(), String> {
        let mut conn = self.ledger.connection()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let existing: Option<String> = tx
            .query_row(
                "select run_id from trigger_occurrence where id=?1",
                [occurrence_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?
            .flatten();
        if let Some(existing) = existing {
            return if existing == run_id.as_str() {
                Ok(())
            } else {
                Err("Trigger occurrence is already attached to another Run".to_string())
            };
        }
        let changed = tx
            .execute(
                "update trigger_occurrence set run_id=?2 where id=?1 and run_id is null",
                params![occurrence_id, run_id.as_str()],
            )
            .map_err(|error| error.to_string())?;
        if changed != 1 {
            return Err("Trigger occurrence was not found".to_string());
        }
        tx.execute(
            "update trigger_occurrence_detail set state='launched' where occurrence_id=?1",
            [occurrence_id],
        )
        .map_err(|error| error.to_string())?;
        tx.commit().map_err(|error| error.to_string())
    }

    /// Stores a complete item revision before it can be included in a provider
    /// checkpoint. Older exact revisions remain available for audit and stale UI.
    pub(crate) fn record_provider_observation(
        &self,
        state: &ProviderItemObservationState,
    ) -> Result<(), String> {
        let observation = match state {
            ProviderItemObservationState::Current(value)
            | ProviderItemObservationState::Stale(value)
            | ProviderItemObservationState::Partial(value) => Some(value),
            ProviderItemObservationState::NeverLoaded
            | ProviderItemObservationState::Failed { .. }
            | ProviderItemObservationState::ConfirmedAbsent => None,
        };
        let Some(observation) = observation else {
            return Err(
                "an item identity is required to persist a non-present observation state"
                    .to_string(),
            );
        };
        let item = observation.id.canonical_key();
        let revision = observation.revision();
        let state_label = provider_state_label(state);
        let payload = serde_json::to_string(observation).map_err(|error| error.to_string())?;
        let mut conn = self.ledger.connection()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        tx.execute(
            "insert or ignore into provider_item_observation(provider_item,observation_revision,observation_json,state,observed_unix_ms) values(?1,?2,?3,?4,?5)",
            params![item,revision,payload,state_label,now_ms()],
        ).map_err(|error| error.to_string())?;
        tx.execute(
            "insert into provider_item_current(provider_item,observation_revision,state,updated_unix_ms) values(?1,?2,?3,?4) on conflict(provider_item) do update set observation_revision=excluded.observation_revision,state=excluded.state,updated_unix_ms=excluded.updated_unix_ms",
            params![item,revision,state_label,now_ms()],
        ).map_err(|error| error.to_string())?;
        tx.commit().map_err(|error| error.to_string())
    }

    /// Records refresh failure without deleting the last exact value. Existing
    /// data becomes stale; a never-loaded item is represented as failed.
    pub(crate) fn record_provider_failure(
        &self,
        provider_item: &str,
        safe_error: &str,
    ) -> Result<(), String> {
        let safe_error = safe_error
            .chars()
            .filter(|character| !character.is_control())
            .take(512)
            .collect::<String>();
        let conn = self.ledger.connection()?;
        let existing: Option<String> = conn
            .query_row(
                "select observation_revision from provider_item_current where provider_item=?1",
                [provider_item],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?
            .flatten();
        conn.execute(
            "insert into provider_item_current(provider_item,observation_revision,state,safe_error,updated_unix_ms) values(?1,?2,?3,?4,?5) on conflict(provider_item) do update set state=excluded.state,safe_error=excluded.safe_error,updated_unix_ms=excluded.updated_unix_ms",
            params![provider_item,existing,if existing.is_some(){"stale"}else{"failed"},safe_error,now_ms()],
        ).map_err(|error|error.to_string())?;
        Ok(())
    }

    pub(crate) fn record_provider_absent(&self, provider_item: &str) -> Result<(), String> {
        self.ledger.connection()?.execute(
            "insert into provider_item_current(provider_item,observation_revision,state,updated_unix_ms) values(?1,null,'confirmed_absent',?2) on conflict(provider_item) do update set observation_revision=null,state='confirmed_absent',safe_error=null,updated_unix_ms=excluded.updated_unix_ms",
            params![provider_item,now_ms()],
        ).map_err(|error|error.to_string())?;
        Ok(())
    }

    pub(crate) fn current_provider_observation(
        &self,
        provider_item: &str,
    ) -> Result<Option<ProviderItemObservationState>, String> {
        let conn = self.ledger.connection()?;
        let row: Option<(Option<String>,String,Option<String>)> = conn.query_row(
            "select observation_revision,state,safe_error from provider_item_current where provider_item=?1",
            [provider_item], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
        ).optional().map_err(|error| error.to_string())?;
        let Some((revision, state, error)) = row else {
            return Ok(None);
        };
        let observation = revision.as_deref().map(|revision| conn.query_row(
            "select observation_json from provider_item_observation where provider_item=?1 and observation_revision=?2",
            params![provider_item,revision], |row| row.get::<_,String>(0),
        ).map_err(|error| error.to_string()).and_then(|json| serde_json::from_str(&json).map_err(|error| error.to_string()))).transpose()?;
        Ok(Some(match (state.as_str(), observation) {
            ("current", Some(value)) => ProviderItemObservationState::Current(value),
            ("stale", Some(value)) => ProviderItemObservationState::Stale(value),
            ("partial", Some(value)) => ProviderItemObservationState::Partial(value),
            ("confirmed_absent", _) => ProviderItemObservationState::ConfirmedAbsent,
            ("failed", _) => ProviderItemObservationState::Failed {
                safe_error: error.unwrap_or_else(|| "provider refresh failed".into()),
            },
            _ => ProviderItemObservationState::NeverLoaded,
        }))
    }

    /// Advances a provider checkpoint only after every item discovered in that
    /// page has both an exact persisted observation and a durable occurrence.
    pub(crate) fn checkpoint(
        &self,
        trigger_id: &str,
        checkpoint: &str,
        discovered_occurrences: &[String],
    ) -> Result<(), String> {
        let mut conn = self.ledger.connection()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        for occurrence in discovered_occurrences {
            let valid: bool = tx.query_row(
                "select exists(select 1 from trigger_occurrence occurrence join trigger_occurrence_detail detail on detail.occurrence_id=occurrence.id left join provider_item_observation observation on observation.provider_item=detail.provider_item and observation.observation_revision=detail.observation_revision where occurrence.id=?1 and occurrence.trigger_id=?2 and (detail.provider_item is null or (observation.state='current'))) ",
                params![occurrence,trigger_id], |row| row.get(0),
            ).map_err(|error| error.to_string())?;
            if !valid {
                return Err(format!(
                    "cannot advance checkpoint before occurrence '{occurrence}' and its current observation are persisted"
                ));
            }
        }
        tx.execute("insert into trigger_checkpoint(trigger_id,checkpoint_json,updated_unix_ms) values(?1,?2,?3) on conflict(trigger_id) do update set checkpoint_json=excluded.checkpoint_json,updated_unix_ms=excluded.updated_unix_ms",params![trigger_id,checkpoint,now_ms()]).map_err(|error| error.to_string())?;
        tx.commit().map_err(|error| error.to_string())
    }

    pub(crate) fn checkpoint_value(&self, trigger_id: &str) -> Result<Option<String>, String> {
        self.ledger
            .connection()?
            .query_row(
                "select checkpoint_json from trigger_checkpoint where trigger_id=?1",
                [trigger_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())
    }

    pub(crate) fn decide_admission(
        &self,
        request: DecideAdmission<'_>,
    ) -> Result<AdmissionDecision, String> {
        let DecideAdmission {
            run_id,
            provider_item,
            purpose,
            policy,
            facts,
            advisory,
            capability_envelope,
            actor,
            expires_unix_ms,
        } = request;
        if facts.observation_revision.is_empty() {
            return Err("Admission requires an exact Observation Revision".to_string());
        }
        let current: Option<(String,String)> = self.ledger.connection()?.query_row(
            "select observation_revision,state from provider_item_current where provider_item=?1",
            [provider_item], |row| Ok((row.get(0)?,row.get(1)?)),
        ).optional().map_err(|error| error.to_string())?;
        if current
            .as_ref()
            .map(|(revision, state)| (revision.as_str(), state.as_str()))
            != Some((facts.observation_revision.as_str(), "current"))
        {
            return Err(
                "Admission observation is changed, stale, partial, or unavailable".to_string(),
            );
        }
        let admitted = policy.evaluate(facts, advisory);
        let conn = self.ledger.connection()?;
        let id = random_id(&conn)?;
        conn.execute(
            "insert or ignore into workflow_admission_record(id,run_id,provider_item,observation_revision,policy_revision,purpose,authenticated_facts_json,advisory,capability_envelope_json,actor,outcome,expires_unix_ms,created_unix_ms) values(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![id,run_id.as_str(),provider_item,facts.observation_revision,policy.revision,purpose,serde_json::to_string(facts).map_err(|error|error.to_string())?,format!("{advisory:?}").to_ascii_lowercase(),serde_json::to_string(capability_envelope).map_err(|error|error.to_string())?,actor,if admitted{"admitted"}else{"rejected"},expires_unix_ms,now_ms()],
        ).map_err(|error| error.to_string())?;
        let (id, admitted, actor, expires): (String,String,String,Option<i64>) = conn.query_row(
            "select id,outcome,actor,expires_unix_ms from workflow_admission_record where run_id=?1 and provider_item=?2 and observation_revision=?3 and policy_revision=?4 and purpose=?5",
            params![run_id.as_str(),provider_item,facts.observation_revision,policy.revision,purpose],
            |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)),
        ).map_err(|error| error.to_string())?;
        Ok(AdmissionDecision {
            id,
            run_id: run_id.clone(),
            provider_item: provider_item.into(),
            observation_revision: facts.observation_revision.clone(),
            policy_revision: policy.revision.clone(),
            purpose: purpose.into(),
            admitted: admitted == "admitted",
            actor,
            expires_unix_ms: expires,
        })
    }

    pub(crate) fn valid_admission(
        &self,
        provider_item: &str,
        observation_revision: &str,
        purpose: &str,
        at_unix_ms: i64,
    ) -> Result<bool, String> {
        self.ledger.connection()?.query_row(
            "select exists(select 1 from workflow_admission_record where provider_item=?1 and observation_revision=?2 and purpose=?3 and outcome='admitted' and (expires_unix_ms is null or expires_unix_ms>?4))",
            params![provider_item,observation_revision,purpose,at_unix_ms], |row| row.get(0),
        ).map_err(|error| error.to_string())
    }

    pub(crate) fn recent_occurrences(
        &self,
        trigger_id: &str,
        limit: usize,
    ) -> Result<Vec<TriggerOccurrence>, String> {
        let conn = self.ledger.connection()?;
        let mut statement = conn.prepare(
            "select occurrence.id,occurrence.occurrence_key,occurrence.run_id,coalesce(detail.state,'pending'),occurrence.created_unix_ms from trigger_occurrence occurrence left join trigger_occurrence_detail detail on detail.occurrence_id=occurrence.id where occurrence.trigger_id=?1 order by occurrence.created_unix_ms desc,occurrence.id limit ?2"
        ).map_err(|error| error.to_string())?;
        statement
            .query_map(params![trigger_id, limit.min(1000) as i64], |row| {
                Ok(TriggerOccurrence {
                    id: row.get(0)?,
                    trigger_id: trigger_id.into(),
                    occurrence_key: row.get(1)?,
                    run_id: row.get::<_, Option<String>>(2)?.map(RunId),
                    state: row.get(3)?,
                    created: false,
                    created_unix_ms: row.get(4)?,
                })
            })
            .map_err(|error| error.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())
    }

    pub(crate) fn list(&self) -> Result<Vec<(TriggerDefinition, bool)>, String> {
        let conn = self.ledger.connection()?;
        let mut statement = conn
            .prepare("select definition_json,enabled from trigger order by id")
            .map_err(|error| error.to_string())?;
        statement
            .query_map([], |row| {
                Ok((
                    serde_json::from_str::<TriggerDefinition>(&row.get::<_, String>(0)?).map_err(
                        |error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                0,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        },
                    )?,
                    row.get::<_, bool>(1)?,
                ))
            })
            .map_err(|error| error.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())
    }

    pub(crate) fn statuses(
        &self,
        at_unix_ms: i64,
        recent_limit: usize,
    ) -> Result<Vec<TriggerStatus>, String> {
        self.list()?
            .into_iter()
            .map(|(definition, enabled)| {
                let next_run_unix_ms = self.next_run(&definition.id, at_unix_ms)?;
                let checkpoint = self.checkpoint_value(&definition.id)?;
                let recent_occurrences = self.recent_occurrences(&definition.id, recent_limit)?;
                let snapshot_digest = sha256(
                    serde_json::to_string(&definition)
                        .map_err(|error| error.to_string())?
                        .as_bytes(),
                );
                Ok(TriggerStatus {
                    definition,
                    snapshot_digest,
                    enabled,
                    next_run_unix_ms,
                    checkpoint,
                    recent_occurrences,
                })
            })
            .collect()
    }

    pub(crate) fn get(&self, id: &str) -> Result<TriggerDefinition, String> {
        self.definition(id, false)
    }

    fn definition(&self, id: &str, require_enabled: bool) -> Result<TriggerDefinition, String> {
        let query = if require_enabled {
            "select definition_json from trigger where id=?1 and enabled=1"
        } else {
            "select definition_json from trigger where id=?1"
        };
        let json: String = self
            .ledger
            .connection()?
            .query_row(query, [id], |row| row.get(0))
            .optional()
            .map_err(|error| error.to_string())?
            .ok_or_else(|| {
                format!(
                    "Trigger '{id}' is missing{}",
                    if require_enabled { " or disabled" } else { "" }
                )
            })?;
        serde_json::from_str(&json).map_err(|error| error.to_string())
    }
}

pub(crate) fn validate_definition(definition: &TriggerDefinition) -> Result<(), String> {
    if definition.id.is_empty()
        || definition.definition_selector.is_empty()
        || definition.admission_purpose.is_empty()
    {
        return Err("Trigger identity, definition, and admission purpose are required".to_string());
    }
    if definition.max_fan_out == 0 {
        return Err("Trigger max_fan_out must be greater than zero".to_string());
    }
    if let TriggerKind::Schedule {
        expression,
        timezone,
        ..
    } = &definition.kind
    {
        parse_schedule(expression)?;
        parse_timezone(timezone)?;
    }
    Ok(())
}

fn parse_schedule(expression: &str) -> Result<Schedule, String> {
    let expression = expression.trim();
    if expression.is_empty() {
        return Err("scheduled Trigger requires an expression".to_string());
    }
    let fields = expression.split_whitespace().count();
    let normalized = if fields == 5 {
        format!("0 {expression}")
    } else {
        expression.to_string()
    };
    Schedule::from_str(&normalized).map_err(|error| format!("invalid Trigger schedule: {error}"))
}

fn parse_timezone(timezone: &str) -> Result<Tz, String> {
    timezone
        .parse::<Tz>()
        .map_err(|_| format!("invalid IANA Trigger timezone '{timezone}'"))
}

fn utc_millis(value: i64) -> Result<DateTime<Utc>, String> {
    Utc.timestamp_millis_opt(value)
        .single()
        .ok_or_else(|| "Trigger timestamp is out of range".to_string())
}

fn provider_state_label(state: &ProviderItemObservationState) -> &'static str {
    match state {
        ProviderItemObservationState::NeverLoaded => "never_loaded",
        ProviderItemObservationState::Current(_) => "current",
        ProviderItemObservationState::Stale(_) => "stale",
        ProviderItemObservationState::Partial(_) => "partial",
        ProviderItemObservationState::Failed { .. } => "failed",
        ProviderItemObservationState::ConfirmedAbsent => "confirmed_absent",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::{
        HostIdentity, ProviderItemId, ProviderItemKind, ProviderItemObservation, ProviderKind,
        RemoteRepositoryId,
    };
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DATABASE: AtomicU64 = AtomicU64::new(1);

    fn setup() -> (TriggerEngine, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "prism-trigger-{}-{}-{}.db",
            std::process::id(),
            now_ms(),
            NEXT_DATABASE.fetch_add(1, Ordering::Relaxed)
        ));
        let engine = TriggerEngine::new(RunLedger::open(path.clone()).unwrap()).unwrap();
        engine
            .register(
                &TriggerDefinition {
                    id: "issues".into(),
                    enabled: true,
                    definition_selector: "builtin:triage".into(),
                    admission_purpose: "implementation".into(),
                    kind: TriggerKind::ProviderEvent {
                        repository: "github:owner/repo".into(),
                        event: "issue".into(),
                    },
                    overlap: OverlapPolicy::Coalesce,
                    max_fan_out: 20,
                },
                true,
            )
            .unwrap();
        (engine, path)
    }

    fn observation() -> ProviderItemObservation {
        let repository = RemoteRepositoryId::new(
            ProviderKind::GitHub,
            HostIdentity::new("github.com", None).unwrap(),
            "owner/repo",
        )
        .unwrap();
        ProviderItemObservation {
            id: ProviderItemId::new(repository, "1", ProviderItemKind::Issue).unwrap(),
            title: "bug".into(),
            body: "untrusted".into(),
            lifecycle: "open".into(),
            author: "alice".into(),
            author_relationship: Some("member".into()),
            labels: BTreeMap::new(),
            assignees: vec![],
            updated_at: Some("2026-01-01T00:00:00Z".into()),
        }
    }

    #[test]
    fn free_form_or_agent_output_cannot_supply_admission_authority() {
        let policy = AdmissionPolicy {
            revision: "1".into(),
            hosts: BTreeSet::from(["github.com".into()]),
            repositories: BTreeSet::from(["owner/repo".into()]),
            events: BTreeSet::from(["issue".into()]),
            actor_relationships: BTreeSet::from(["member".into()]),
            required_label_ids: BTreeSet::new(),
        };
        let facts = AuthenticatedProviderFacts {
            host: "github.com".into(),
            repository: "other/repo".into(),
            event: "issue".into(),
            actor_relationship: Some("member".into()),
            label_ids: BTreeSet::new(),
            observation_revision: "rev".into(),
        };
        assert!(!policy.evaluate(&facts, AdvisoryClassification::Acceptable));
        let mut matching = facts;
        matching.repository = "owner/repo".into();
        assert!(policy.evaluate(&matching, AdvisoryClassification::Acceptable));
        assert!(!policy.evaluate(&matching, AdvisoryClassification::Reject));
    }

    #[test]
    fn overlapping_delivery_deduplicates_complete_identity() {
        let (engine, path) = setup();
        let make = || OccurrenceIdentity {
            native_occurrence: "poll-1",
            provider_item: Some("github:owner/repo:issue:1"),
            observation_revision: Some("revision-a"),
            definition_digest: "definition-a",
            input_digest: "input-a",
        };
        assert!(engine.record_occurrence("issues", make()).unwrap().created);
        assert!(!engine.record_occurrence("issues", make()).unwrap().created);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn checkpoint_requires_exact_current_observation() {
        let (engine, path) = setup();
        let observed = observation();
        let revision = observed.revision();
        let item = observed.id.canonical_key();
        let occurrence = engine
            .record_occurrence(
                "issues",
                OccurrenceIdentity {
                    native_occurrence: "poll",
                    provider_item: Some(&item),
                    observation_revision: Some(&revision),
                    definition_digest: "def",
                    input_digest: "input",
                },
            )
            .unwrap();
        assert!(
            engine
                .checkpoint("issues", "page-2", std::slice::from_ref(&occurrence.id))
                .is_err()
        );
        engine
            .record_provider_observation(&ProviderItemObservationState::Current(observed))
            .unwrap();
        engine
            .checkpoint("issues", "page-2", &[occurrence.id])
            .unwrap();
        assert_eq!(
            engine.checkpoint_value("issues").unwrap().as_deref(),
            Some("page-2")
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn schedules_use_timezone_database_across_dst() {
        let (engine, path) = setup();
        engine
            .register(
                &TriggerDefinition {
                    id: "daily".into(),
                    enabled: true,
                    definition_selector: "builtin:triage".into(),
                    admission_purpose: "triage".into(),
                    kind: TriggerKind::Schedule {
                        expression: "30 2 * * *".into(),
                        timezone: "America/New_York".into(),
                        missed: MissedOccurrencePolicy::AllBounded,
                    },
                    overlap: OverlapPolicy::Queue,
                    max_fan_out: 10,
                },
                true,
            )
            .unwrap();
        // The spring-forward day has no 02:30 local occurrence.
        let from = DateTime::parse_from_rfc3339("2026-03-07T00:00:00Z")
            .unwrap()
            .timestamp_millis();
        let to = DateTime::parse_from_rfc3339("2026-03-10T00:00:00Z")
            .unwrap()
            .timestamp_millis();
        let due = engine.evaluate_due("daily", from, to).unwrap();
        assert_eq!(due.len(), 2);
        assert!(
            due.iter()
                .all(|value| value.local_time.contains("02:30:00"))
        );
        std::fs::remove_file(path).unwrap();
    }
}
