#![allow(dead_code)] // The generalized engine remains parallel until the Phase 8 cutover.

use std::collections::BTreeSet;

use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::definition::{InputBinding, Port, PrimitiveClass, SnapshotContent, StepSettings};
use crate::run::{
    ArtifactInput, ArtifactRef, AttemptId, AuthorityGrant, AuthorityGrantId, ExecutionWorkspaceId,
    RunId, RunLedger, StepId, now_ms, random_id, recompute_run_state,
};
use crate::target::{CancellationToken, WorkspaceRef};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ClaimAccess {
    ImmutableRead,
    MutableRead,
    Write,
    Exclusive,
}

impl ClaimAccess {
    fn label(self) -> &'static str {
        match self {
            Self::ImmutableRead => "immutable_read",
            Self::MutableRead => "mutable_read",
            Self::Write => "write",
            Self::Exclusive => "exclusive",
        }
    }

    fn conflicts(self, other: Self) -> bool {
        !matches!((self, other), (Self::ImmutableRead, Self::ImmutableRead))
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct ResourceClaimSpec {
    pub key: String,
    pub access: ClaimAccess,
    pub expected_generation: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct AttemptLease {
    pub attempt_id: AttemptId,
    pub worker_id: String,
    pub target_id: String,
    pub fencing_token: i64,
    pub expires_unix_ms: i64,
}

#[derive(Clone)]
pub(crate) struct AttemptEnvelope {
    pub run_id: RunId,
    pub step_id: StepId,
    pub attempt_id: AttemptId,
    pub implementation: String,
    pub implementation_revision: u32,
    pub primitive_class: PrimitiveClass,
    pub settings: StepSettings,
    pub authority: AuthorityGrant,
    pub inputs: Vec<BoundArtifact>,
    pub resource_claims: Vec<ResourceClaimSpec>,
    pub workspace: Option<WorkspaceRef>,
    pub cancellation: CancellationToken,
    pub output_budget_bytes: u64,
    pub fencing_token: i64,
}

pub(crate) struct ClaimedAttempt {
    pub lease: AttemptLease,
    pub envelope: AttemptEnvelope,
}

#[derive(Clone, Debug)]
pub(crate) struct PrepareAttempt {
    pub run_id: RunId,
    pub step_id: StepId,
    pub input_digest: String,
    pub target_id: String,
    pub workspace: Option<ExecutionWorkspaceId>,
    pub resource_claims: Vec<ResourceClaimSpec>,
    pub input_artifacts: Vec<BoundArtifact>,
}

#[derive(Clone, Debug)]
pub(crate) struct BoundArtifact {
    pub port: String,
    pub artifact: ArtifactRef,
    pub payload: serde_json::Value,
}

#[derive(Clone, Debug)]
pub(crate) struct AttemptResult {
    pub outcome: String,
    pub outputs: Vec<ArtifactInput>,
}

#[derive(Clone)]
pub(crate) struct Coordinator {
    ledger: RunLedger,
    lease_ms: i64,
    global_limit: usize,
    repository_limit: usize,
    definition_limit: usize,
    implementation_limit: usize,
}

impl Coordinator {
    pub(crate) fn new(ledger: RunLedger) -> Self {
        Self {
            ledger,
            lease_ms: 15_000,
            global_limit: 4,
            repository_limit: 2,
            definition_limit: 2,
            implementation_limit: 2,
        }
    }

    #[cfg(test)]
    fn with_lease(ledger: RunLedger, lease_ms: i64) -> Self {
        Self {
            ledger,
            lease_ms,
            global_limit: 4,
            repository_limit: 2,
            definition_limit: 2,
            implementation_limit: 2,
        }
    }

    pub(crate) fn prepare(&self, request: PrepareAttempt) -> Result<AttemptId, String> {
        let mut claims = request.resource_claims;
        claims.sort_by(|left, right| left.key.cmp(&right.key));
        if claims.windows(2).any(|pair| pair[0].key == pair[1].key) {
            return Err("an Attempt cannot declare the same resource more than once".to_string());
        }
        let mut conn = self.ledger.connection()?;
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let (implementation, revision, state, input_bindings): (String, u32, String, String) = transaction.query_row(
            "select implementation_id, implementation_revision, state, input_bindings_json from workflow_step where id = ?1 and run_id = ?2",
            params![request.step_id.as_str(), request.run_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        ).map_err(sql_error)?;
        if !matches!(state.as_str(), "runnable" | "failed") {
            return Err(format!("Step is {state}, not runnable"));
        }
        let remaining: i64 = transaction
            .query_row(
                "select b.remaining_attempts from workflow_run r join workflow_budget b on b.id=r.budget_id where r.id = ?1",
                [request.run_id.as_str()],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        if remaining <= 0 {
            return Err("shared attempt budget is exhausted".to_string());
        }
        let ordinal: u32 = transaction
            .query_row(
                "select attempt_count + 1 from workflow_step where id = ?1",
                [request.step_id.as_str()],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        let attempt_id = AttemptId(random_id(&transaction)?);
        let now = now_ms();
        transaction.execute("insert into step_attempt (id, run_id, step_id, ordinal, state, input_digest, implementation_id, implementation_revision, target_id, workspace_id, requested_claims_json, created_unix_ms, updated_unix_ms) values (?1, ?2, ?3, ?4, 'prepared', ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)", params![attempt_id.as_str(), request.run_id.as_str(), request.step_id.as_str(), ordinal, request.input_digest, implementation, revision, request.target_id, request.workspace.as_ref().map(ExecutionWorkspaceId::as_str), serde_json::to_string(&claims).map_err(|error| error.to_string())?, now]).map_err(sql_error)?;
        validate_and_bind_inputs(
            &transaction,
            &attempt_id,
            &request.run_id,
            &input_bindings,
            &request.input_artifacts,
        )?;
        transaction.execute("update workflow_step set state = 'runnable', attempt_count = attempt_count + 1, updated_unix_ms = ?2 where id = ?1", params![request.step_id.as_str(), now]).map_err(sql_error)?;
        transaction.execute("update workflow_run set remaining_attempts = remaining_attempts - 1, revision = revision + 1, updated_unix_ms = ?2 where id = ?1", params![request.run_id.as_str(), now]).map_err(sql_error)?;
        transaction.execute("update workflow_budget set remaining_attempts=remaining_attempts-1,updated_unix_ms=?2 where id=(select budget_id from workflow_run where id=?1)",params![request.run_id.as_str(),now]).map_err(sql_error)?;
        transaction.commit().map_err(sql_error)?;
        Ok(attempt_id)
    }

    pub(crate) fn claim(
        &self,
        worker_id: &str,
        supported_targets: &BTreeSet<String>,
    ) -> Result<Option<ClaimedAttempt>, String> {
        let started = std::time::Instant::now();
        let mut conn = self.ledger.connection()?;
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        recover_expired(&transaction, now_ms())?;
        let active: i64 = transaction
            .query_row(
                "select count(*) from attempt_lease where expires_unix_ms > ?1",
                [now_ms()],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        if active >= self.global_limit as i64 {
            transaction.commit().map_err(sql_error)?;
            record_coordinator_timing("claim", started, "global_limit", 0);
            return Ok(None);
        }
        let candidates = {
            let mut statement = transaction.prepare("select a.id, a.run_id, a.step_id, a.implementation_id, a.implementation_revision, a.target_id, a.workspace_id, a.requested_claims_json from step_attempt a join workflow_run r on r.id=a.run_id where a.state = 'prepared' and r.control='running' order by a.created_unix_ms, a.id limit 64").map_err(sql_error)?;
            statement
                .query_map([], |row| {
                    Ok((
                        AttemptId(row.get(0)?),
                        RunId(row.get(1)?),
                        StepId(row.get(2)?),
                        row.get::<_, String>(3)?,
                        row.get::<_, u32>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, String>(7)?,
                    ))
                })
                .map_err(sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)?
        };
        for candidate in candidates {
            if !supported_targets.contains(&candidate.5) {
                continue;
            }
            let (repository_id, definition_name): (Option<String>, String) = transaction
                .query_row(
                    "select repository_id,definition_name from workflow_run where id=?1",
                    [candidate.1.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(sql_error)?;
            let definition_active: i64 = transaction.query_row("select count(*) from attempt_lease lease join step_attempt attempt on attempt.id=lease.attempt_id join workflow_run run on run.id=attempt.run_id where run.definition_name=?1 and lease.expires_unix_ms>?2",params![definition_name,now_ms()],|row|row.get(0)).map_err(sql_error)?;
            if definition_active >= self.definition_limit as i64 {
                continue;
            }
            let implementation_active: i64 = transaction.query_row("select count(*) from attempt_lease lease join step_attempt attempt on attempt.id=lease.attempt_id where attempt.implementation_id=?1 and lease.expires_unix_ms>?2",params![candidate.3,now_ms()],|row|row.get(0)).map_err(sql_error)?;
            if implementation_active >= self.implementation_limit as i64 {
                continue;
            }
            if let Some(repository_id) = repository_id {
                let repository_active: i64 = transaction.query_row("select count(*) from attempt_lease lease join step_attempt attempt on attempt.id=lease.attempt_id join workflow_run run on run.id=attempt.run_id where run.repository_id=?1 and lease.expires_unix_ms>?2",params![repository_id,now_ms()],|row|row.get(0)).map_err(sql_error)?;
                if repository_active >= self.repository_limit as i64 {
                    continue;
                }
            }
            let claims: Vec<ResourceClaimSpec> =
                serde_json::from_str(&candidate.7).map_err(|error| error.to_string())?;
            if resource_conflict(&transaction, &claims)? {
                continue;
            }
            if !resource_generations_match(&transaction, &claims)? {
                continue;
            }
            if let Some(workspace_id) = candidate.6.as_deref() {
                let state: Option<String> = transaction
                    .query_row(
                        "select state from execution_workspace where id = ?1",
                        [workspace_id],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(sql_error)?;
                if state.as_deref() != Some("available") {
                    continue;
                }
            }
            let previous_token: i64 = transaction
                .query_row(
                    "select fencing_generation from step_attempt where id=?1",
                    [candidate.0.as_str()],
                    |row| row.get(0),
                )
                .map_err(sql_error)?;
            let token = previous_token + 1;
            let now = now_ms();
            let expires = now.saturating_add(self.lease_ms);
            transaction.execute("insert into attempt_lease (attempt_id, worker_id, target_id, fencing_token, expires_unix_ms, interruption_generation) values (?1, ?2, ?3, ?4, ?5, 0) on conflict(attempt_id) do update set worker_id=excluded.worker_id,target_id=excluded.target_id,fencing_token=excluded.fencing_token,expires_unix_ms=excluded.expires_unix_ms", params![candidate.0.as_str(), worker_id, candidate.5, token, expires]).map_err(sql_error)?;
            transaction
                .execute(
                    "update step_attempt set fencing_generation=?2 where id=?1",
                    params![candidate.0.as_str(), token],
                )
                .map_err(sql_error)?;
            for claim in &claims {
                transaction.execute("insert into resource_claim (attempt_id, resource_key, access, expected_generation, acquired_unix_ms) values (?1, ?2, ?3, ?4, ?5)", params![candidate.0.as_str(), claim.key, claim.access.label(), claim.expected_generation, now]).map_err(sql_error)?;
            }
            if let Some(workspace_id) = candidate.6.as_deref() {
                transaction.execute("update execution_workspace set state='leased', updated_unix_ms=?2 where id=?1", params![workspace_id, now]).map_err(sql_error)?;
            }
            transaction.execute("update step_attempt set state='leased', updated_unix_ms=?2 where id=?1 and state='prepared'", params![candidate.0.as_str(), now]).map_err(sql_error)?;
            transaction
                .execute(
                    "update workflow_step set state='active', updated_unix_ms=?2 where id=?1",
                    params![candidate.2.as_str(), now],
                )
                .map_err(sql_error)?;
            recompute_run_state(&transaction, &candidate.1, now)?;
            let class_text: String = transaction
                .query_row(
                    "select class from workflow_step where id=?1",
                    [candidate.2.as_str()],
                    |row| row.get(0),
                )
                .map_err(sql_error)?;
            let class = parse_enum(&class_text)?;
            let settings = load_step_settings(&transaction, &candidate.1, &candidate.2)?;
            let grant = load_grant(&transaction, &candidate.1, &candidate.2)?;
            let inputs = load_bound_inputs(&transaction, &candidate.0)?;
            let workspace = candidate
                .6
                .as_deref()
                .map(|id| load_workspace(&transaction, id))
                .transpose()?;
            transaction.commit().map_err(sql_error)?;
            let lease = AttemptLease {
                attempt_id: candidate.0.clone(),
                worker_id: worker_id.to_string(),
                target_id: candidate.5,
                fencing_token: token,
                expires_unix_ms: expires,
            };
            record_coordinator_timing("claim", started, "claimed", 1);
            return Ok(Some(ClaimedAttempt {
                envelope: AttemptEnvelope {
                    run_id: candidate.1,
                    step_id: candidate.2,
                    attempt_id: candidate.0,
                    implementation: candidate.3,
                    implementation_revision: candidate.4,
                    primitive_class: class,
                    settings,
                    authority: grant,
                    inputs,
                    resource_claims: claims,
                    workspace,
                    cancellation: CancellationToken::default(),
                    output_budget_bytes: 4 * 1024 * 1024,
                    fencing_token: token,
                },
                lease,
            }));
        }
        transaction.commit().map_err(sql_error)?;
        record_coordinator_timing("claim", started, "waiting", 0);
        Ok(None)
    }

    pub(crate) fn heartbeat(&self, lease: &AttemptLease) -> Result<AttemptLease, String> {
        let started = std::time::Instant::now();
        let conn = self.ledger.connection()?;
        let now = now_ms();
        let expires = now.saturating_add(self.lease_ms);
        let changed = conn.execute("update attempt_lease set expires_unix_ms=?5 where attempt_id=?1 and worker_id=?2 and target_id=?3 and fencing_token=?4 and expires_unix_ms>?6 and exists(select 1 from step_attempt a join workflow_run r on r.id=a.run_id where a.id=?1 and r.control='running')", params![lease.attempt_id.as_str(), lease.worker_id, lease.target_id, lease.fencing_token, expires, now]).map_err(sql_error)?;
        if changed == 0 {
            return Err(
                "Attempt lease is stale, expired, or interrupted by Run control".to_string(),
            );
        }
        let mut renewed = lease.clone();
        renewed.expires_unix_ms = expires;
        record_coordinator_timing("heartbeat", started, "renewed", 1);
        Ok(renewed)
    }

    pub(crate) fn record_process(
        &self,
        attempt_id: &AttemptId,
        fencing_token: i64,
        process: crate::process::RecordedProcess,
    ) -> Result<(), String> {
        let conn = self.ledger.connection()?;
        let identity = process
            .identity
            .map(|identity| i64::try_from(identity.stored_value()))
            .transpose()
            .map_err(|_| "process identity exceeds SQLite integer range".to_string())?;
        let changed = conn.execute("insert into attempt_process(attempt_id,target_process_id,pid,process_identity,state,updated_unix_ms) select ?1,?2,?3,?4,'running',?5 where exists(select 1 from attempt_lease where attempt_id=?1 and fencing_token=?6 and expires_unix_ms>?5) on conflict(attempt_id) do update set target_process_id=excluded.target_process_id,pid=excluded.pid,process_identity=excluded.process_identity,state='running',updated_unix_ms=excluded.updated_unix_ms",params![attempt_id.as_str(),format!("pid:{}",process.pid),process.pid,identity,now_ms(),fencing_token]).map_err(sql_error)?;
        if changed != 1 {
            return Err("stale Attempt cannot record a process".to_string());
        }
        Ok(())
    }

    pub(crate) fn append_output(
        &self,
        attempt_id: &AttemptId,
        fencing_token: i64,
        stream: &str,
        bytes: &[u8],
    ) -> Result<(), String> {
        self.ledger
            .append_output(attempt_id, fencing_token, stream, bytes)
    }

    pub(crate) fn interrupt_for_control(&self, lease: &AttemptLease) -> Result<bool, String> {
        let mut conn = self.ledger.connection()?;
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        validate_lease(&transaction, lease)?;
        let (run_id, step_id, workspace_id, control): (RunId, StepId, Option<String>, String) =
            transaction
                .query_row(
                    "select a.run_id,a.step_id,a.workspace_id,r.control from step_attempt a join workflow_run r on r.id=a.run_id where a.id=?1",
                    [lease.attempt_id.as_str()],
                    |row| Ok((RunId(row.get(0)?), StepId(row.get(1)?), row.get(2)?, row.get(3)?)),
                )
                .map_err(sql_error)?;
        if control == "running" {
            transaction.commit().map_err(sql_error)?;
            return Ok(false);
        }
        let now = now_ms();
        let (attempt_state, step_state, reason) = if control == "cancel_requested" {
            ("cancelled", "cancelled", "Run cancellation requested")
        } else {
            ("prepared", "runnable", "Run paused")
        };
        transaction.execute("update step_attempt set state=?2,terminal_reason=?3,updated_unix_ms=?4 where id=?1",params![lease.attempt_id.as_str(),attempt_state,reason,now]).map_err(sql_error)?;
        transaction
            .execute(
                "update workflow_step set state=?2,blocker=?3,updated_unix_ms=?4 where id=?1",
                params![step_id.as_str(), step_state, reason, now],
            )
            .map_err(sql_error)?;
        transaction
            .execute(
                "delete from resource_claim where attempt_id=?1",
                [lease.attempt_id.as_str()],
            )
            .map_err(sql_error)?;
        transaction
            .execute(
                "delete from attempt_lease where attempt_id=?1",
                [lease.attempt_id.as_str()],
            )
            .map_err(sql_error)?;
        if let Some(workspace_id) = workspace_id {
            transaction.execute("update execution_workspace set state='available',updated_unix_ms=?2 where id=?1",params![workspace_id,now]).map_err(sql_error)?;
        }
        recompute_run_state(&transaction, &run_id, now)?;
        transaction.commit().map_err(sql_error)?;
        Ok(true)
    }

    pub(crate) fn finish(&self, lease: &AttemptLease, result: AttemptResult) -> Result<(), String> {
        let trace = crate::flight_recorder::TransactionTrace::begin("workflow.attempt.finish");
        let mut conn = self.ledger.connection()?;
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        validate_lease(&transaction, lease)?;
        let (run_id, step_id, workspace_id): (RunId, StepId, Option<String>) = transaction
            .query_row(
                "select run_id, step_id, workspace_id from step_attempt where id=?1",
                [lease.attempt_id.as_str()],
                |row| Ok((RunId(row.get(0)?), StepId(row.get(1)?), row.get(2)?)),
            )
            .map_err(sql_error)?;
        let declared_outputs: std::collections::BTreeMap<String, Port> = transaction
            .query_row(
                "select outputs_json from workflow_step where id=?1",
                [step_id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .map_err(sql_error)
            .and_then(|value| serde_json::from_str(&value).map_err(|error| error.to_string()))?;
        let mut output_names = BTreeSet::new();
        for output in &result.outputs {
            let declared = declared_outputs
                .get(&output.name)
                .ok_or_else(|| format!("Attempt produced undeclared output '{}'", output.name))?;
            if declared.artifact_type != output.artifact_type {
                return Err(format!(
                    "Attempt output '{}' has type '{}', expected '{}'",
                    output.name, output.artifact_type, declared.artifact_type
                ));
            }
            if !output_names.insert(&output.name) {
                return Err(format!(
                    "Attempt produced output '{}' more than once",
                    output.name
                ));
            }
            let size = serde_json::to_vec(&output.payload)
                .map_err(|error| error.to_string())?
                .len();
            if size > 64 * 1024 {
                return Err(format!(
                    "Attempt output '{}' exceeds 65536 bytes",
                    output.name
                ));
            }
        }
        for (name, declared) in &declared_outputs {
            if declared.required && !output_names.contains(name) {
                return Err(format!("Attempt did not produce required output '{name}'"));
            }
        }
        let has_untrusted_provenance: bool = transaction
            .query_row(
                "select exists(select 1 from attempt_input i join artifact a on a.id=i.artifact_id and a.revision=i.artifact_revision where i.attempt_id=?1 and a.trust!='trusted')",
                [lease.attempt_id.as_str()],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        let input_sensitivity: Option<String> = transaction.query_row("select case when sum(a.sensitivity='sensitive')>0 then 'sensitive' when sum(a.sensitivity='internal')>0 then 'internal' when count(*)>0 then 'public' end from attempt_input i join artifact a on a.id=i.artifact_id and a.revision=i.artifact_revision where i.attempt_id=?1",[lease.attempt_id.as_str()],|row|row.get(0)).map_err(sql_error)?;
        let now = now_ms();
        for output in &result.outputs {
            let bytes = serde_json::to_vec(&output.payload).map_err(|error| error.to_string())?;
            let artifact_id = random_id(&transaction)?;
            let trust = if has_untrusted_provenance {
                "derived_untrusted".to_string()
            } else {
                serde_json::to_value(output.trust)
                    .expect("Artifact trust serializes")
                    .as_str()
                    .expect("Artifact trust serializes as text")
                    .to_string()
            };
            let declared_sensitivity = serde_json::to_value(output.sensitivity)
                .expect("Artifact sensitivity serializes")
                .as_str()
                .expect("Artifact sensitivity serializes as text")
                .to_string();
            let sensitivity =
                stricter_sensitivity(input_sensitivity.as_deref(), &declared_sensitivity);
            transaction.execute("insert into artifact (id, revision, run_id, producer_attempt_id, port, artifact_type, schema_revision, digest, trust, sensitivity, payload_inline, size, created_unix_ms) values (?1,1,?2,?3,?4,?5,1,?6,?7,?8,?9,?10,?11)", params![artifact_id, run_id.as_str(), lease.attempt_id.as_str(), output.name, output.artifact_type, crate::run::sha256(&bytes), trust, sensitivity, bytes, bytes.len() as i64, now]).map_err(sql_error)?;
            transaction.execute("insert into artifact_lineage(artifact_id,artifact_revision,source_artifact_id,source_revision,consumer_port) select ?1,1,artifact_id,artifact_revision,port from attempt_input where attempt_id=?2",params![artifact_id,lease.attempt_id.as_str()]).map_err(sql_error)?;
        }
        transaction.execute("update step_attempt set state='completed',terminal_reason=?2,updated_unix_ms=?3 where id=?1", params![lease.attempt_id.as_str(), result.outcome, now]).map_err(sql_error)?;
        transaction
            .execute(
                "update attempt_process set state='exited',updated_unix_ms=?2 where attempt_id=?1",
                params![lease.attempt_id.as_str(), now],
            )
            .map_err(sql_error)?;
        transaction.execute("update workflow_step set state='completed',outcome=?2,updated_unix_ms=?3 where id=?1", params![step_id.as_str(), result.outcome, now]).map_err(sql_error)?;
        transaction
            .execute(
                "delete from resource_claim where attempt_id=?1",
                [lease.attempt_id.as_str()],
            )
            .map_err(sql_error)?;
        transaction
            .execute(
                "delete from attempt_lease where attempt_id=?1",
                [lease.attempt_id.as_str()],
            )
            .map_err(sql_error)?;
        if let Some(workspace_id) = workspace_id {
            transaction.execute("update execution_workspace set state='available',updated_unix_ms=?2 where id=?1", params![workspace_id, now]).map_err(sql_error)?;
        }
        recompute_run_state(&transaction, &run_id, now)?;
        transaction.commit().map_err(sql_error)?;
        trace.committed();
        Ok(())
    }

    pub(crate) fn fail(&self, lease: &AttemptLease, reason: &str) -> Result<(), String> {
        let mut conn = self.ledger.connection()?;
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        validate_lease(&transaction, lease)?;
        let (run_id, step_id, workspace_id): (RunId, StepId, Option<String>) = transaction
            .query_row(
                "select run_id,step_id,workspace_id from step_attempt where id=?1",
                [lease.attempt_id.as_str()],
                |row| Ok((RunId(row.get(0)?), StepId(row.get(1)?), row.get(2)?)),
            )
            .map_err(sql_error)?;
        let now = now_ms();
        transaction.execute("update step_attempt set state='failed',terminal_reason=?2,updated_unix_ms=?3 where id=?1",params![lease.attempt_id.as_str(),reason,now]).map_err(sql_error)?;
        transaction
            .execute(
                "update attempt_process set state='exited',updated_unix_ms=?2 where attempt_id=?1",
                params![lease.attempt_id.as_str(), now],
            )
            .map_err(sql_error)?;
        transaction.execute("update workflow_step set state='failed',outcome='failed',blocker=?2,updated_unix_ms=?3 where id=?1",params![step_id.as_str(),reason,now]).map_err(sql_error)?;
        transaction
            .execute(
                "delete from resource_claim where attempt_id=?1",
                [lease.attempt_id.as_str()],
            )
            .map_err(sql_error)?;
        transaction
            .execute(
                "delete from attempt_lease where attempt_id=?1",
                [lease.attempt_id.as_str()],
            )
            .map_err(sql_error)?;
        if let Some(workspace_id) = workspace_id {
            transaction.execute("update execution_workspace set state='available',updated_unix_ms=?2 where id=?1",params![workspace_id,now]).map_err(sql_error)?;
        }
        recompute_run_state(&transaction, &run_id, now)?;
        transaction.commit().map_err(sql_error)
    }

    pub(crate) fn recover(&self, attempt_id: &AttemptId, retry: bool) -> Result<(), String> {
        {
            let conn = self.ledger.connection()?;
            let recoverable: bool = conn
                .query_row(
                    "select exists(select 1 from step_attempt where id=?1 and state='recovery_required')",
                    [attempt_id.as_str()],
                    |row| row.get(0),
                )
                .map_err(sql_error)?;
            if !recoverable {
                return Err("Attempt is not recovery-required".to_string());
            }
            let process: Option<(u32, Option<u64>)> = conn
                .query_row(
                    "select pid,process_identity from attempt_process where attempt_id=?1 and state='running'",
                    [attempt_id.as_str()],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get::<_, Option<i64>>(1)?.map(|value| value as u64),
                        ))
                    },
                )
                .optional()
                .map_err(sql_error)?;
            drop(conn);
            if let Some((pid, identity)) = process {
                let recorded = crate::process::RecordedProcess::from_stored(pid, identity);
                match crate::process::terminate_recorded_process(
                    recorded,
                    std::time::Duration::from_secs(2),
                )
                .map_err(|error| error.to_string())?
                {
                    crate::process::TerminationOutcome::Terminated
                    | crate::process::TerminationOutcome::AlreadyExited
                    | crate::process::TerminationOutcome::IdentityReused => {}
                    crate::process::TerminationOutcome::Unverifiable => {
                        return Err(
                            "prior Attempt process identity is unverifiable; retry is unsafe"
                                .to_string(),
                        );
                    }
                }
            }
        }
        let mut conn = self.ledger.connection()?;
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let (run_id, step_id, input_digest, target_id, workspace_id, claims): (RunId, StepId, String, String, Option<String>, String) = transaction.query_row("select run_id,step_id,input_digest,target_id,workspace_id,requested_claims_json from step_attempt where id=?1 and state='recovery_required'", [attempt_id.as_str()], |row| Ok((RunId(row.get(0)?), StepId(row.get(1)?), row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?))).map_err(sql_error)?;
        let now = now_ms();
        if retry {
            let mut claim_specs: Vec<ResourceClaimSpec> =
                serde_json::from_str(&claims).map_err(|error| error.to_string())?;
            if let Some(workspace_id) = workspace_id.as_deref() {
                let generation: i64 = transaction
                    .query_row(
                        "select generation+1 from execution_workspace where id=?1 and state='quarantined'",
                        [workspace_id],
                        |row| row.get(0),
                    )
                    .map_err(sql_error)?;
                for claim in &mut claim_specs {
                    let workspace_claim = claim.key == "workspace"
                        || claim.key == format!("workspace:{workspace_id}");
                    if workspace_claim && claim.expected_generation.is_some() {
                        claim.expected_generation = Some(generation);
                        transaction.execute("insert into resource_generation(resource_key,generation,updated_unix_ms) values(?1,?2,?3) on conflict(resource_key) do update set generation=excluded.generation,updated_unix_ms=excluded.updated_unix_ms",params![claim.key,generation,now]).map_err(sql_error)?;
                    }
                }
                transaction.execute("update execution_workspace set generation=?2,state='available',quarantine_reason=null,updated_unix_ms=?3 where id=?1",params![workspace_id,generation,now]).map_err(sql_error)?;
            }
            let retry_claims =
                serde_json::to_string(&claim_specs).map_err(|error| error.to_string())?;
            let remaining: i64 = transaction.query_row(
                "select b.remaining_attempts from workflow_run r join workflow_budget b on b.id=r.budget_id where r.id=?1",
                [run_id.as_str()],
                |row| row.get(0),
            ).map_err(sql_error)?;
            if remaining <= 0 {
                return Err("shared attempt budget is exhausted".to_string());
            }
            let new_id = random_id(&transaction)?;
            let ordinal: u32 = transaction
                .query_row(
                    "select max(ordinal)+1 from step_attempt where step_id=?1",
                    [step_id.as_str()],
                    |row| row.get(0),
                )
                .map_err(sql_error)?;
            transaction.execute("insert into step_attempt (id,run_id,step_id,ordinal,state,input_digest,implementation_id,implementation_revision,target_id,workspace_id,requested_claims_json,created_unix_ms,updated_unix_ms) select ?1,run_id,step_id,?2,'prepared',input_digest,implementation_id,implementation_revision,?3,?4,?5,?6,?6 from step_attempt where id=?7", params![new_id, ordinal, target_id, workspace_id, retry_claims, now, attempt_id.as_str()]).map_err(sql_error)?;
            transaction.execute("insert into attempt_input(attempt_id,port,artifact_id,artifact_revision) select ?1,port,artifact_id,artifact_revision from attempt_input where attempt_id=?2",params![new_id,attempt_id.as_str()]).map_err(sql_error)?;
            transaction.execute("update attempt_process set state='terminated_for_recovery',updated_unix_ms=?2 where attempt_id=?1",params![attempt_id.as_str(),now]).map_err(sql_error)?;
            transaction
                .execute(
                    "delete from resource_claim where attempt_id=?1",
                    [attempt_id.as_str()],
                )
                .map_err(sql_error)?;
            transaction.execute("update workflow_step set state='runnable',attempt_count=attempt_count+1,updated_unix_ms=?2 where id=?1", params![step_id.as_str(), now]).map_err(sql_error)?;
            transaction.execute("update workflow_run set remaining_attempts=remaining_attempts-1,revision=revision+1,updated_unix_ms=?2 where id=?1", params![run_id.as_str(), now]).map_err(sql_error)?;
            transaction.execute("update workflow_budget set remaining_attempts=remaining_attempts-1,updated_unix_ms=?2 where id=(select budget_id from workflow_run where id=?1)",params![run_id.as_str(),now]).map_err(sql_error)?;
        } else {
            transaction.execute("update workflow_step set state='failed',blocker='recovery disposition: fail',updated_unix_ms=?2 where id=?1", params![step_id.as_str(), now]).map_err(sql_error)?;
            transaction
                .execute(
                    "delete from resource_claim where attempt_id=?1",
                    [attempt_id.as_str()],
                )
                .map_err(sql_error)?;
        }
        let _ = input_digest;
        recompute_run_state(&transaction, &run_id, now)?;
        transaction.commit().map_err(sql_error)
    }
}

fn record_coordinator_timing(
    operation: &'static str,
    started: std::time::Instant,
    outcome: &'static str,
    claimed: u64,
) {
    crate::flight_recorder::record(
        "workflow_coordinator",
        operation,
        Some(started.elapsed()),
        vec![
            crate::flight_recorder::text("outcome", outcome),
            crate::flight_recorder::unsigned("claimed", claimed),
        ],
    );
}

fn validate_and_bind_inputs(
    conn: &rusqlite::Connection,
    attempt_id: &AttemptId,
    run_id: &RunId,
    bindings_json: &str,
    artifacts: &[BoundArtifact],
) -> Result<(), String> {
    let bindings: std::collections::BTreeMap<String, InputBinding> =
        serde_json::from_str(bindings_json).map_err(|error| error.to_string())?;
    let provided = artifacts
        .iter()
        .map(|artifact| (artifact.port.as_str(), artifact))
        .collect::<std::collections::BTreeMap<_, _>>();
    if bindings.len() != provided.len() || artifacts.len() != provided.len() {
        return Err("Attempt inputs do not exactly match the Step bindings".to_string());
    }
    for (port, binding) in bindings {
        let provided = provided
            .get(port.as_str())
            .ok_or_else(|| format!("Attempt input '{port}' is missing"))?;
        if provided.artifact.artifact_type != binding.artifact_type {
            return Err(format!(
                "Attempt input '{port}' has type '{}', expected '{}'",
                provided.artifact.artifact_type, binding.artifact_type
            ));
        }
        let payload_digest = crate::run::sha256(
            &serde_json::to_vec(&provided.payload).map_err(|error| error.to_string())?,
        );
        if payload_digest != provided.artifact.digest {
            return Err(format!(
                "Attempt input '{port}' payload does not match its digest"
            ));
        }
        let (source, source_port) = binding
            .from
            .split_once('.')
            .ok_or_else(|| format!("invalid compiled input binding '{}'", binding.from))?;
        let valid: bool = if source == "run" {
            conn.query_row("select exists(select 1 from artifact where id=?1 and revision=?2 and digest=?3 and artifact_type=?4 and run_id=?5 and producer_attempt_id is null and port=?6)",params![provided.artifact.id.as_str(),provided.artifact.revision,provided.artifact.digest,provided.artifact.artifact_type,run_id.as_str(),source_port],|row|row.get(0)).map_err(sql_error)?
        } else {
            conn.query_row("select exists(select 1 from artifact a join step_attempt producer on producer.id=a.producer_attempt_id join workflow_step s on s.id=producer.step_id where a.id=?1 and a.revision=?2 and a.digest=?3 and a.artifact_type=?4 and a.run_id=?5 and s.definition_step_id=?6 and a.port=?7)",params![provided.artifact.id.as_str(),provided.artifact.revision,provided.artifact.digest,provided.artifact.artifact_type,run_id.as_str(),source,source_port],|row|row.get(0)).map_err(sql_error)?
        };
        if !valid {
            return Err(format!(
                "Attempt input '{port}' is not an exact Artifact revision from this Run"
            ));
        }
        conn.execute("insert into attempt_input(attempt_id,port,artifact_id,artifact_revision) values(?1,?2,?3,?4)",params![attempt_id.as_str(),port,provided.artifact.id.as_str(),provided.artifact.revision]).map_err(sql_error)?;
    }
    Ok(())
}

fn stricter_sensitivity<'a>(input: Option<&'a str>, output: &'a str) -> &'a str {
    fn rank(value: &str) -> u8 {
        match value {
            "sensitive" => 2,
            "internal" => 1,
            _ => 0,
        }
    }
    input
        .filter(|input| rank(input) > rank(output))
        .unwrap_or(output)
}

fn resource_conflict(
    conn: &rusqlite::Connection,
    requested: &[ResourceClaimSpec],
) -> Result<bool, String> {
    for claim in requested {
        let mut statement = conn
            .prepare("select access from resource_claim where resource_key=?1")
            .map_err(sql_error)?;
        let existing = statement
            .query_map([&claim.key], |row| row.get::<_, String>(0))
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?;
        for access in existing {
            if claim.access.conflicts(parse_enum(&access)?) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn resource_generations_match(
    conn: &rusqlite::Connection,
    requested: &[ResourceClaimSpec],
) -> Result<bool, String> {
    for claim in requested {
        let Some(expected) = claim.expected_generation else {
            continue;
        };
        let actual: Option<i64> = conn
            .query_row(
                "select generation from resource_generation where resource_key=?1",
                [&claim.key],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_error)?;
        if actual != Some(expected) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn validate_lease(conn: &rusqlite::Connection, lease: &AttemptLease) -> Result<(), String> {
    let valid: bool = conn.query_row("select exists(select 1 from attempt_lease where attempt_id=?1 and worker_id=?2 and target_id=?3 and fencing_token=?4 and expires_unix_ms>?5)", params![lease.attempt_id.as_str(), lease.worker_id, lease.target_id, lease.fencing_token, now_ms()], |row| row.get(0)).map_err(sql_error)?;
    if valid {
        Ok(())
    } else {
        Err("stale or expired Attempt lease".to_string())
    }
}

fn recover_expired(conn: &rusqlite::Connection, now: i64) -> Result<(), String> {
    let expired = {
        let mut statement = conn.prepare("select l.attempt_id,a.run_id,a.step_id,a.workspace_id from attempt_lease l join step_attempt a on a.id=l.attempt_id where l.expires_unix_ms<=?1").map_err(sql_error)?;
        statement
            .query_map([now], |row| {
                Ok((
                    AttemptId(row.get(0)?),
                    RunId(row.get(1)?),
                    StepId(row.get(2)?),
                    row.get::<_, Option<String>>(3)?,
                ))
            })
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?
    };
    for (attempt, run, step, workspace) in expired {
        let writing: bool = conn.query_row("select exists(select 1 from resource_claim where attempt_id=?1 and access in ('write','exclusive'))", [attempt.as_str()], |row| row.get(0)).map_err(sql_error)?;
        conn.execute("update step_attempt set state='recovery_required',terminal_reason='lease expired',updated_unix_ms=?2 where id=?1", params![attempt.as_str(), now]).map_err(sql_error)?;
        conn.execute("update workflow_step set state='recovery_required',blocker='Attempt lease expired',updated_unix_ms=?2 where id=?1", params![step.as_str(), now]).map_err(sql_error)?;
        if writing && let Some(workspace) = workspace {
            conn.execute("update execution_workspace set state='quarantined',quarantine_reason='writing Attempt lease expired',updated_unix_ms=?2 where id=?1", params![workspace, now]).map_err(sql_error)?;
        }
        if !writing {
            conn.execute(
                "delete from resource_claim where attempt_id=?1",
                [attempt.as_str()],
            )
            .map_err(sql_error)?;
        }
        conn.execute(
            "delete from attempt_lease where attempt_id=?1",
            [attempt.as_str()],
        )
        .map_err(sql_error)?;
        recompute_run_state(conn, &run, now)?;
    }
    Ok(())
}

fn load_step_settings(
    conn: &rusqlite::Connection,
    run_id: &RunId,
    step_id: &StepId,
) -> Result<StepSettings, String> {
    let (bytes, definition_step_id): (Vec<u8>, String) = conn
        .query_row(
            "select snapshot.canonical_bytes,step.definition_step_id from workflow_run run join definition_snapshot snapshot on snapshot.digest=run.snapshot_digest join workflow_step step on step.run_id=run.id where run.id=?1 and step.id=?2",
            params![run_id.as_str(), step_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(sql_error)?;
    let snapshot: SnapshotContent = serde_json::from_slice(&bytes)
        .map_err(|error| format!("decode Definition Snapshot: {error}"))?;
    snapshot
        .steps
        .into_iter()
        .find(|step| step.id == definition_step_id)
        .map(|step| step.settings)
        .ok_or_else(|| "materialized Step is absent from its Definition Snapshot".to_string())
}

fn load_grant(
    conn: &rusqlite::Connection,
    run_id: &RunId,
    step_id: &StepId,
) -> Result<AuthorityGrant, String> {
    conn.query_row("select g.id,g.capabilities_json,g.secret_scope_json,g.target_scope_json,g.expires_unix_ms,s.capabilities_json from authority_grant g join workflow_run r on r.authority_grant_id=g.id join workflow_step s on s.run_id=r.id where r.id=?1 and s.id=?2", params![run_id.as_str(),step_id.as_str()], |row| {
        let granted: BTreeSet<crate::definition::Capability> = serde_json::from_str(&row.get::<_, String>(1)?).map_err(json_sql_error)?;
        let declared: BTreeSet<crate::definition::Capability> = serde_json::from_str(&row.get::<_, String>(5)?).map_err(json_sql_error)?;
        Ok(AuthorityGrant { id: AuthorityGrantId(row.get(0)?), capabilities: granted.intersection(&declared).cloned().collect(), secret_handles: serde_json::from_str(&row.get::<_, String>(2)?).map_err(json_sql_error)?, target_scope: serde_json::from_str(&row.get::<_, String>(3)?).map_err(json_sql_error)?, expires_unix_ms: row.get(4)? })
    }).map_err(sql_error)
}

fn load_bound_inputs(
    conn: &rusqlite::Connection,
    attempt_id: &AttemptId,
) -> Result<Vec<BoundArtifact>, String> {
    let mut statement=conn.prepare("select i.port,a.id,a.revision,a.digest,a.artifact_type,a.payload_inline from attempt_input i join artifact a on a.id=i.artifact_id and a.revision=i.artifact_revision where i.attempt_id=?1 order by i.port").map_err(sql_error)?;
    statement
        .query_map([attempt_id.as_str()], |row| {
            Ok(BoundArtifact {
                port: row.get(0)?,
                artifact: ArtifactRef {
                    id: crate::run::ArtifactId(row.get(1)?),
                    revision: row.get(2)?,
                    digest: row.get(3)?,
                    artifact_type: row.get(4)?,
                },
                payload: serde_json::from_slice(&row.get::<_, Vec<u8>>(5)?)
                    .map_err(json_sql_error)?,
            })
        })
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)
}

fn load_workspace(conn: &rusqlite::Connection, id: &str) -> Result<WorkspaceRef, String> {
    conn.query_row(
        "select id,repository_id,generation,base_revision from execution_workspace where id=?1",
        [id],
        |row| {
            Ok(WorkspaceRef {
                id: ExecutionWorkspaceId(row.get(0)?),
                repository_id: row
                    .get::<_, Option<String>>(1)?
                    .map(crate::run::RepositoryId),
                generation: row.get(2)?,
                base_revision: row.get(3)?,
            })
        },
    )
    .map_err(sql_error)
}

fn parse_enum<T: for<'de> Deserialize<'de>>(value: &str) -> Result<T, String> {
    serde_json::from_str(&format!("\"{value}\"")).map_err(|error| error.to_string())
}

fn json_sql_error(error: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}
fn sql_error(error: rusqlite::Error) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::{Capability, DefinitionCatalog};
    use crate::run::{ArtifactInput, Sensitivity, StartRun, TrustClass};
    use std::path::PathBuf;

    fn setup() -> (RunLedger, RunId, StepId, Vec<BoundArtifact>, PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "prism-coordinator-{}-{}-{:?}.db",
            std::process::id(),
            now_ms(),
            std::thread::current().id()
        ));
        let ledger = RunLedger::open(path.clone()).unwrap();
        let snapshot = DefinitionCatalog::discover(None)
            .resolve("builtin:action")
            .unwrap();
        let run = ledger
            .start(StartRun {
                snapshot,
                repository_id: None,
                inputs: vec![ArtifactInput {
                    name: "task".into(),
                    artifact_type: "builtin:task@1".into(),
                    payload: serde_json::json!({"task":"x"}),
                    trust: TrustClass::Trusted,
                    sensitivity: Sensitivity::Internal,
                }],
                idempotency_key: None,
                actor: "test".into(),
                actor_capabilities: BTreeSet::from([
                    Capability::RepositoryRead,
                    Capability::ProcessExecute,
                ]),
            })
            .unwrap();
        let step = ledger.inspect(&run.run_id).unwrap().steps[0].id.clone();
        let conn = ledger.connection().unwrap();
        conn.execute(
            "update workflow_step set state='runnable' where id=?1",
            [step.as_str()],
        )
        .unwrap();
        let input = conn.query_row("select id,revision,digest,artifact_type,payload_inline from artifact where run_id=?1 and port='task'",[run.run_id.as_str()],|row|Ok(BoundArtifact{port:"task".to_string(),artifact:ArtifactRef{id:crate::run::ArtifactId(row.get(0)?),revision:row.get(1)?,digest:row.get(2)?,artifact_type:row.get(3)?},payload:serde_json::from_slice(&row.get::<_,Vec<u8>>(4)?).unwrap()})).unwrap();
        drop(conn);
        (ledger, run.run_id, step, vec![input], path)
    }

    #[test]
    fn one_claimant_and_stale_owner_is_fenced() {
        let (ledger, run, step, inputs, path) = setup();
        let coordinator = Coordinator::new(ledger.clone());
        let attempt = coordinator
            .prepare(PrepareAttempt {
                run_id: run,
                step_id: step,
                input_digest: "input".into(),
                target_id: "local".into(),
                workspace: None,
                resource_claims: vec![ResourceClaimSpec {
                    key: "repo:test".into(),
                    access: ClaimAccess::MutableRead,
                    expected_generation: None,
                }],
                input_artifacts: inputs,
            })
            .unwrap();
        let targets = BTreeSet::from(["local".to_string()]);
        let claim = coordinator.claim("worker-a", &targets).unwrap().unwrap();
        assert_eq!(claim.lease.attempt_id, attempt);
        assert!(coordinator.claim("worker-b", &targets).unwrap().is_none());
        let stale = AttemptLease {
            fencing_token: claim.lease.fencing_token + 1,
            ..claim.lease.clone()
        };
        assert!(
            coordinator
                .finish(
                    &stale,
                    AttemptResult {
                        outcome: "succeeded".into(),
                        outputs: vec![]
                    }
                )
                .unwrap_err()
                .contains("stale")
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn claim_intersects_run_authority_with_step_capabilities() {
        let (ledger, run, step, inputs, path) = setup();
        let conn = ledger.connection().unwrap();
        conn.execute("update authority_grant set capabilities_json='[\"git_push\",\"process_execute\"]' where run_id=?1",[run.as_str()]).unwrap();
        conn.execute(
            "update workflow_step set capabilities_json='[\"process_execute\"]' where id=?1",
            [step.as_str()],
        )
        .unwrap();
        drop(conn);
        let coordinator = Coordinator::new(ledger);
        coordinator
            .prepare(PrepareAttempt {
                run_id: run,
                step_id: step,
                input_digest: "input".into(),
                target_id: "local".into(),
                workspace: None,
                resource_claims: vec![],
                input_artifacts: inputs,
            })
            .unwrap();
        let claim = coordinator
            .claim("worker", &BTreeSet::from(["local".to_string()]))
            .unwrap()
            .unwrap();
        assert_eq!(
            claim.envelope.authority.capabilities,
            BTreeSet::from([Capability::ProcessExecute])
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn pause_blocks_claims_and_returns_active_attempt_to_prepared() {
        let (ledger, run, step, inputs, path) = setup();
        let coordinator = Coordinator::new(ledger.clone());
        coordinator
            .prepare(PrepareAttempt {
                run_id: run.clone(),
                step_id: step,
                input_digest: "input".into(),
                target_id: "local".into(),
                workspace: None,
                resource_claims: vec![],
                input_artifacts: inputs,
            })
            .unwrap();
        ledger.set_control(&run, "pause_requested").unwrap();
        let targets = BTreeSet::from(["local".to_string()]);
        assert!(coordinator.claim("worker", &targets).unwrap().is_none());
        ledger.set_control(&run, "running").unwrap();
        let claim = coordinator.claim("worker", &targets).unwrap().unwrap();
        ledger.set_control(&run, "pause_requested").unwrap();
        assert!(coordinator.interrupt_for_control(&claim.lease).unwrap());
        let state: String = ledger
            .connection()
            .unwrap()
            .query_row(
                "select state from step_attempt where id=?1",
                [claim.lease.attempt_id.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "prepared");
        ledger.set_control(&run, "running").unwrap();
        let reclaimed = coordinator.claim("worker", &targets).unwrap().unwrap();
        assert!(reclaimed.lease.fencing_token > claim.lease.fencing_token);
        assert!(
            coordinator
                .finish(
                    &claim.lease,
                    AttemptResult {
                        outcome: "succeeded".into(),
                        outputs: vec![]
                    }
                )
                .unwrap_err()
                .contains("stale")
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn resource_claims_are_atomic_and_waiting_uses_no_slot() {
        let (ledger, run, step, inputs, path) = setup();
        let coordinator = Coordinator::new(ledger.clone());
        coordinator
            .prepare(PrepareAttempt {
                run_id: run,
                step_id: step,
                input_digest: "input".into(),
                target_id: "local".into(),
                workspace: None,
                resource_claims: vec![
                    ResourceClaimSpec {
                        key: "shared".into(),
                        access: ClaimAccess::Write,
                        expected_generation: None,
                    },
                    ResourceClaimSpec {
                        key: "other".into(),
                        access: ClaimAccess::Write,
                        expected_generation: None,
                    },
                ],
                input_artifacts: inputs,
            })
            .unwrap();
        let targets = BTreeSet::from(["local".to_string()]);
        let claim = coordinator.claim("worker", &targets).unwrap().unwrap();
        let conn = ledger.connection().unwrap();
        assert_eq!(
            conn.query_row(
                "select count(*) from resource_claim where attempt_id=?1",
                [claim.lease.attempt_id.as_str()],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            2
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn expired_writer_quarantines_workspace() {
        let (ledger, run, step, inputs, path) = setup();
        let workspace = ExecutionWorkspaceId("workspace".into());
        let conn = ledger.connection().unwrap();
        conn.execute("insert into execution_workspace(id,target_id,base_revision,generation,state,updated_unix_ms) values(?1,'local','abc',1,'available',?2)",params![workspace.as_str(),now_ms()]).unwrap();
        conn.execute("insert into resource_generation(resource_key,generation,updated_unix_ms) values('workspace',1,?1)",[now_ms()]).unwrap();
        drop(conn);
        let coordinator = Coordinator::with_lease(ledger.clone(), -1_000);
        coordinator
            .prepare(PrepareAttempt {
                run_id: run,
                step_id: step,
                input_digest: "input".into(),
                target_id: "local".into(),
                workspace: Some(workspace.clone()),
                resource_claims: vec![ResourceClaimSpec {
                    key: "workspace".into(),
                    access: ClaimAccess::Write,
                    expected_generation: Some(1),
                }],
                input_artifacts: inputs,
            })
            .unwrap();
        let targets = BTreeSet::from(["local".to_string()]);
        coordinator.claim("worker", &targets).unwrap().unwrap();
        assert!(coordinator.claim("other", &targets).unwrap().is_none());
        let conn = ledger.connection().unwrap();
        let state: String = conn
            .query_row(
                "select state from execution_workspace where id=?1",
                [workspace.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "quarantined");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn undeclared_attempt_output_is_rejected_without_releasing_lease() {
        let (ledger, run, step, inputs, path) = setup();
        let coordinator = Coordinator::new(ledger.clone());
        coordinator
            .prepare(PrepareAttempt {
                run_id: run,
                step_id: step,
                input_digest: "input".into(),
                target_id: "local".into(),
                workspace: None,
                resource_claims: vec![],
                input_artifacts: inputs,
            })
            .unwrap();
        let claim = coordinator
            .claim("worker", &BTreeSet::from(["local".to_string()]))
            .unwrap()
            .unwrap();
        let error = coordinator
            .finish(
                &claim.lease,
                AttemptResult {
                    outcome: "succeeded".into(),
                    outputs: vec![ArtifactInput {
                        name: "undeclared".into(),
                        artifact_type: "builtin:task@1".into(),
                        payload: serde_json::json!({}),
                        trust: crate::run::TrustClass::Trusted,
                        sensitivity: crate::run::Sensitivity::Internal,
                    }],
                },
            )
            .unwrap_err();
        assert!(error.contains("undeclared output"));
        assert!(coordinator.heartbeat(&claim.lease).is_ok());
        std::fs::remove_file(path).unwrap();
    }
}
