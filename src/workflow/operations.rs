#![allow(dead_code)] // Generic operations are not exposed until the production cutover.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::coordinator::{BoundArtifact, Coordinator, PrepareAttempt};
use crate::definition::{ConditionExpr, DefinitionSnapshot, ObservedBool};
use crate::effect::{ChildIntentRequest, complete_child_intent, prepare_child_intent};
use crate::run::{
    ApprovalDecision, ApprovalMode, ApprovalRequest, ApprovalRequestId, ArtifactInput, AttemptId,
    RunId, RunLedger, RunProjection, StartRun, StartRunResult, StepId, now_ms, random_id,
    recompute_run_state,
};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GateStatus {
    Waiting,
    Satisfied,
    Unsatisfied,
    Unknown,
    Unavailable,
}

impl GateStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Satisfied => "satisfied",
            Self::Unsatisfied => "unsatisfied",
            Self::Unknown => "unknown",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EvidenceQuality {
    Current,
    Missing,
    Stale,
    Partial,
    Unknown,
    Unavailable,
}

#[derive(Clone, Debug)]
pub(crate) struct GateEvidence {
    pub subject_digest: String,
    pub subject_generation: String,
    pub evidence: Vec<String>,
    pub quality: EvidenceQuality,
    pub policy_revision: String,
    pub status: GateStatus,
    pub reason: String,
    pub expires_unix_ms: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RecordedGateResult {
    pub id: String,
    pub status: GateStatus,
}

impl GateEvidence {
    pub(crate) fn authorizes_mutation(&self, subject_generation: &str) -> bool {
        self.status == GateStatus::Satisfied
            && self.quality == EvidenceQuality::Current
            && self.subject_generation == subject_generation
            && self
                .expires_unix_ms
                .is_none_or(|expires| expires > now_ms())
    }
}

#[derive(Clone, Debug)]
pub(crate) enum WorkflowCommand {
    Launch(Box<StartRun>),
    Pause(RunId),
    Resume(RunId),
    Cancel(RunId),
    Retry {
        step_id: StepId,
        attempt_id: AttemptId,
        input_digest: String,
    },
    Recover {
        attempt_id: AttemptId,
        retry: bool,
    },
    Approve {
        request_id: ApprovalRequestId,
        input_digest: String,
        evidence_digest: String,
        actor: String,
        reason: Option<String>,
    },
    Reject {
        request_id: ApprovalRequestId,
        input_digest: String,
        evidence_digest: String,
        actor: String,
        reason: Option<String>,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct ChildCall {
    pub parent_run: RunId,
    pub parent_step: StepId,
    pub call_key: String,
    pub purpose: String,
    pub propagation: String,
    /// Dependency-blocked fan-out children are durably created but own no
    /// worker slot until the parent scheduler activates them.
    pub start_paused: bool,
    pub snapshot: DefinitionSnapshot,
    pub inputs: Vec<ArtifactInput>,
}

struct ParentCallContext {
    budget_id: String,
    grant_id: String,
    capabilities_json: String,
    grant_expiry: Option<i64>,
    lineage_depth: i64,
    max_child_depth: i64,
    step_class: String,
    step_state: String,
    step_capabilities_json: String,
    control: String,
    repository_id: Option<crate::run::RepositoryId>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkflowCommandReceipt {
    Launched(StartRunResult),
    Controlled {
        run_id: RunId,
        control: String,
    },
    Retried {
        previous_attempt_id: AttemptId,
        new_attempt_id: AttemptId,
    },
    Recovered {
        attempt_id: AttemptId,
        retry: bool,
    },
    Decided(ApprovalDecision),
}

#[derive(Clone)]
pub(crate) struct WorkflowOperations {
    ledger: RunLedger,
    coordinator: Coordinator,
}

impl WorkflowOperations {
    pub(crate) fn launch_named(
        repository: &std::path::Path,
        selector: &str,
        inputs: Vec<ArtifactInput>,
        actor: String,
    ) -> Result<RunId, String> {
        let ledger = RunLedger::user()?;
        if !ledger.cutover_complete()? {
            return Err("Workflow execution requires `prism config migrate-workflows`".to_string());
        }
        let snapshot = crate::definition::DefinitionCatalog::discover(Some(repository))
            .resolve(selector)
            .map_err(|diagnostics| {
                diagnostics
                    .into_iter()
                    .map(|diagnostic| diagnostic.message)
                    .collect::<Vec<_>>()
                    .join("; ")
            })?;
        let repository_id = ledger.repository_id(repository)?;
        let result = ledger.start(StartRun {
            actor_capabilities: snapshot.content.transitive_capabilities.clone(),
            snapshot,
            repository_id: Some(repository_id),
            inputs,
            idempotency_key: None,
            actor,
        })?;
        Ok(result.run_id)
    }

    pub(crate) fn new(ledger: RunLedger) -> Self {
        Self {
            coordinator: Coordinator::new(ledger.clone()),
            ledger,
        }
    }

    pub(crate) fn query(&self, run_id: &RunId) -> Result<RunProjection, String> {
        self.ledger.inspect(run_id)
    }

    pub(crate) fn history(
        &self,
        run_id: &RunId,
        after: i64,
        limit: usize,
    ) -> Result<Vec<crate::run::RunEvent>, String> {
        self.ledger.history(run_id, after, limit)
    }

    pub(crate) fn decide_pending(
        &self,
        request_id: ApprovalRequestId,
        approved: bool,
        actor: String,
        reason: Option<String>,
    ) -> Result<WorkflowCommandReceipt, String> {
        let conn = self.ledger.connection()?;
        let (input_digest, evidence_digest): (String, String) = conn
            .query_row(
                "select input_digest,evidence_digest from approval_request where id=?1 and state='pending'",
                [request_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(sql_error)?
            .ok_or_else(|| format!("pending Approval Request '{}' was not found", request_id.as_str()))?;
        drop(conn);
        self.execute(if approved {
            WorkflowCommand::Approve {
                request_id,
                input_digest,
                evidence_digest,
                actor,
                reason,
            }
        } else {
            WorkflowCommand::Reject {
                request_id,
                input_digest,
                evidence_digest,
                actor,
                reason,
            }
        })
    }

    pub(crate) fn retry_attempt(
        &self,
        attempt_id: AttemptId,
    ) -> Result<WorkflowCommandReceipt, String> {
        let conn = self.ledger.connection()?;
        let (step_id, input_digest): (String, String) = conn
            .query_row(
                "select step_id,input_digest from step_attempt where id=?1 and state in ('failed','recovery_required')",
                [attempt_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(sql_error)?
            .ok_or_else(|| format!("retryable Attempt '{}' was not found", attempt_id.as_str()))?;
        drop(conn);
        self.execute(WorkflowCommand::Retry {
            step_id: StepId(step_id),
            attempt_id,
            input_digest,
        })
    }

    pub(crate) fn execute(
        &self,
        command: WorkflowCommand,
    ) -> Result<WorkflowCommandReceipt, String> {
        match command {
            WorkflowCommand::Launch(request) => self
                .ledger
                .start(*request)
                .map(WorkflowCommandReceipt::Launched),
            WorkflowCommand::Pause(run_id) => {
                self.ledger.set_control(&run_id, "pause_requested")?;
                Ok(WorkflowCommandReceipt::Controlled {
                    run_id,
                    control: "pause_requested".to_string(),
                })
            }
            WorkflowCommand::Resume(run_id) => {
                self.ledger.set_control(&run_id, "running")?;
                Ok(WorkflowCommandReceipt::Controlled {
                    run_id,
                    control: "running".to_string(),
                })
            }
            WorkflowCommand::Cancel(run_id) => {
                self.ledger.set_control(&run_id, "cancel_requested")?;
                Ok(WorkflowCommandReceipt::Controlled {
                    run_id,
                    control: "cancel_requested".to_string(),
                })
            }
            WorkflowCommand::Retry {
                step_id,
                attempt_id,
                input_digest,
            } => {
                let conn = self.ledger.connection()?;
                let (run_id, prior_digest, target_id, workspace_id, claims): (RunId, String, String, Option<String>, String) = conn.query_row("select run_id,input_digest,target_id,workspace_id,requested_claims_json from step_attempt where id=?1 and step_id=?2 and state in ('failed','recovery_required')", params![attempt_id.as_str(), step_id.as_str()], |row| Ok((RunId(row.get(0)?),row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?))).map_err(sql_error)?;
                if prior_digest != input_digest {
                    return Err("retry input digest does not match the prior Attempt".to_string());
                }
                let input_artifacts = load_attempt_inputs(&conn, &attempt_id)?;
                drop(conn);
                let new_attempt_id = self.coordinator.prepare(PrepareAttempt {
                    run_id,
                    step_id,
                    input_digest,
                    target_id,
                    workspace: workspace_id.map(crate::run::ExecutionWorkspaceId),
                    resource_claims: serde_json::from_str(&claims)
                        .map_err(|error| error.to_string())?,
                    input_artifacts,
                })?;
                Ok(WorkflowCommandReceipt::Retried {
                    previous_attempt_id: attempt_id,
                    new_attempt_id,
                })
            }
            WorkflowCommand::Recover { attempt_id, retry } => {
                self.coordinator.recover(&attempt_id, retry)?;
                Ok(WorkflowCommandReceipt::Recovered { attempt_id, retry })
            }
            WorkflowCommand::Approve {
                request_id,
                input_digest,
                evidence_digest,
                actor,
                reason,
            } => self
                .ledger
                .decide_approval(
                    &request_id,
                    &input_digest,
                    &evidence_digest,
                    true,
                    &actor,
                    reason.as_deref(),
                )
                .map(WorkflowCommandReceipt::Decided),
            WorkflowCommand::Reject {
                request_id,
                input_digest,
                evidence_digest,
                actor,
                reason,
            } => self
                .ledger
                .decide_approval(
                    &request_id,
                    &input_digest,
                    &evidence_digest,
                    false,
                    &actor,
                    reason.as_deref(),
                )
                .map(WorkflowCommandReceipt::Decided),
        }
    }

    pub(crate) fn schedule(&self, run_id: &RunId) -> Result<Vec<StepId>, String> {
        let mut conn = self.ledger.connection()?;
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let control: String = transaction
            .query_row(
                "select control from workflow_run where id=?1",
                [run_id.as_str()],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        if control != "running" {
            return Ok(Vec::new());
        }
        project_child_completion(&transaction, run_id)?;
        let steps = load_schedule_steps(&transaction, run_id)?;
        let outcomes = steps
            .iter()
            .map(|step| {
                (
                    step.definition_id.clone(),
                    (step.state.clone(), step.outcome.clone()),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let mut runnable = Vec::new();
        let now = now_ms();
        for step in &steps {
            if !matches!(step.state.as_str(), "pending" | "blocked" | "waiting") {
                continue;
            }
            if step.state == "waiting" {
                if step.wake_unix_ms.is_none_or(|wake| wake > now) {
                    continue;
                }
                let gate = transaction.execute(
                    "update workflow_step set state='runnable',blocker=null,wake_unix_ms=null,updated_unix_ms=?2 where id=?1 and class='gate'",
                    params![step.id.as_str(), now],
                ).map_err(sql_error)?;
                if gate == 1 {
                    runnable.push(step.id.clone());
                    continue;
                }
                transaction.execute("update workflow_step set state='completed',outcome='deadline_elapsed',blocker=null,wake_unix_ms=null,updated_unix_ms=?2 where id=?1 and class='wait'",params![step.id.as_str(),now]).map_err(sql_error)?;
                continue;
            }
            let dependencies = serde_json::from_str::<Vec<String>>(&step.dependencies)
                .map_err(|error| error.to_string())?;
            let failed_dependency = dependencies.iter().any(|dependency| {
                outcomes.get(dependency).is_some_and(|(state, _)| {
                    matches!(state.as_str(), "failed" | "cancelled" | "recovery_required")
                })
            });
            if failed_dependency {
                transaction.execute("update workflow_step set state='skipped',outcome='dependency_failed',blocker=null,updated_unix_ms=?2 where id=?1", params![step.id.as_str(),now]).map_err(sql_error)?;
                continue;
            }
            let dependencies_ready = dependencies.iter().all(|dependency| {
                outcomes
                    .get(dependency)
                    .is_some_and(|(state, _)| matches!(state.as_str(), "completed" | "skipped"))
            });
            if !dependencies_ready {
                transaction.execute("update workflow_step set state='blocked',blocker='waiting for dependencies',updated_unix_ms=?2 where id=?1", params![step.id.as_str(),now]).map_err(sql_error)?;
                continue;
            }
            if let Some(condition) = step.condition.as_deref() {
                let expression: ConditionExpr =
                    serde_json::from_str(condition).map_err(|error| error.to_string())?;
                let condition_values = load_condition_values(&transaction, run_id, &expression)?;
                match expression.evaluate(&condition_values) {
                    ObservedBool::Value(true) => {}
                    ObservedBool::Value(false) => {
                        transaction.execute("update workflow_step set state='skipped',outcome='condition_false',blocker=null,updated_unix_ms=?2 where id=?1", params![step.id.as_str(),now]).map_err(sql_error)?;
                        continue;
                    }
                    other => {
                        transaction.execute("update workflow_step set state='waiting',blocker=?2,updated_unix_ms=?3 where id=?1", params![step.id.as_str(),format!("condition input is {other:?}"),now]).map_err(sql_error)?;
                        continue;
                    }
                }
            }
            transaction.execute("update workflow_step set state='runnable',blocker=null,wake_unix_ms=null,updated_unix_ms=?2 where id=?1", params![step.id.as_str(),now]).map_err(sql_error)?;
            runnable.push(step.id.clone());
        }
        recompute_run_state(&transaction, run_id, now)?;
        transaction.commit().map_err(sql_error)?;
        Ok(runnable)
    }

    pub(crate) fn step_inputs(
        &self,
        run_id: &RunId,
        step_id: &StepId,
    ) -> Result<Vec<ArtifactInput>, String> {
        let conn = self.ledger.connection()?;
        let bindings_json: String = conn
            .query_row(
                "select input_bindings_json from workflow_step where id=?1 and run_id=?2 and state in ('runnable','waiting')",
                params![step_id.as_str(), run_id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_error)?
            .ok_or_else(|| "Workflow Step is missing or not runnable".to_string())?;
        let bindings: BTreeMap<String, crate::definition::InputBinding> =
            serde_json::from_str(&bindings_json).map_err(|error| error.to_string())?;
        bindings
            .into_iter()
            .map(|(port, binding)| {
                let (source_step, source_port) = binding.from.split_once('.').ok_or_else(|| {
                    format!("invalid persisted input binding '{}'", binding.from)
                })?;
                let row: Option<(String, Vec<u8>, String, String)> = if source_step == "run" {
                    conn.query_row(
                        "select artifact_type,payload_inline,trust,sensitivity from artifact where run_id=?1 and producer_attempt_id is null and port=?2 order by revision desc limit 1",
                        params![run_id.as_str(), source_port],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    )
                    .optional()
                    .map_err(sql_error)?
                } else {
                    conn.query_row(
                        "select artifact.artifact_type,artifact.payload_inline,artifact.trust,artifact.sensitivity from artifact join step_attempt attempt on attempt.id=artifact.producer_attempt_id join workflow_step step on step.id=attempt.step_id where artifact.run_id=?1 and step.definition_step_id=?2 and artifact.port=?3 and attempt.state='completed' order by artifact.created_unix_ms desc,artifact.revision desc limit 1",
                        params![run_id.as_str(), source_step, source_port],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    )
                    .optional()
                    .map_err(sql_error)?
                };
                let (artifact_type, payload, trust, sensitivity) = row.ok_or_else(|| {
                    format!("input '{port}' is not available from '{}'", binding.from)
                })?;
                if artifact_type != binding.artifact_type {
                    return Err(format!(
                        "input '{port}' expected '{}' but persisted Artifact is '{}'",
                        binding.artifact_type, artifact_type
                    ));
                }
                Ok(ArtifactInput {
                    name: port,
                    artifact_type,
                    payload: serde_json::from_slice(&payload)
                        .map_err(|error| error.to_string())?,
                    trust: parse_trust_class(&trust)?,
                    sensitivity: parse_sensitivity(&sensitivity)?,
                })
            })
            .collect()
    }

    pub(crate) fn step_input_artifact_identity(
        &self,
        run_id: &RunId,
        step_id: &StepId,
        port: &str,
    ) -> Result<(String, i64), String> {
        let conn = self.ledger.connection()?;
        let bindings_json: String = conn
            .query_row(
                "select input_bindings_json from workflow_step where id=?1 and run_id=?2",
                params![step_id.as_str(), run_id.as_str()],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        let bindings: BTreeMap<String, crate::definition::InputBinding> =
            serde_json::from_str(&bindings_json).map_err(|error| error.to_string())?;
        let binding = bindings
            .get(port)
            .ok_or_else(|| format!("Workflow Step has no input port '{port}'"))?;
        let (source_step, source_port) = binding
            .from
            .split_once('.')
            .ok_or_else(|| format!("invalid persisted input binding '{}'", binding.from))?;
        let identity = if source_step == "run" {
            conn.query_row(
                "select digest,revision from artifact where run_id=?1 and producer_attempt_id is null and port=?2 order by revision desc limit 1",
                params![run_id.as_str(), source_port],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
        } else {
            conn.query_row(
                "select artifact.digest,artifact.revision from artifact join step_attempt attempt on attempt.id=artifact.producer_attempt_id join workflow_step step on step.id=attempt.step_id where artifact.run_id=?1 and step.definition_step_id=?2 and artifact.port=?3 and attempt.state='completed' order by artifact.created_unix_ms desc,artifact.revision desc limit 1",
                params![run_id.as_str(), source_step, source_port],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
        };
        identity.map_err(sql_error)
    }

    pub(crate) fn prepare_action(
        &self,
        run_id: &RunId,
        step_id: &StepId,
        target_id: String,
        workspace: Option<crate::run::ExecutionWorkspaceId>,
        resource_claims: Vec<crate::coordinator::ResourceClaimSpec>,
    ) -> Result<AttemptId, String> {
        let conn = self.ledger.connection()?;
        let bindings_json: String = conn
            .query_row(
                "select input_bindings_json from workflow_step where id=?1 and run_id=?2 and class='action' and state='runnable'",
                params![step_id.as_str(), run_id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_error)?
            .ok_or_else(|| "Action Step is missing or not runnable".to_string())?;
        let bindings: BTreeMap<String, crate::definition::InputBinding> =
            serde_json::from_str(&bindings_json).map_err(|error| error.to_string())?;
        let mut input_artifacts = Vec::new();
        for (port, binding) in bindings {
            let (source_step, source_port) = binding
                .from
                .split_once('.')
                .ok_or_else(|| format!("invalid persisted input binding '{}'", binding.from))?;
            let row: Option<(String, u32, String, String, Vec<u8>)> = if source_step == "run" {
                conn.query_row(
                    "select id,revision,digest,artifact_type,payload_inline from artifact where run_id=?1 and producer_attempt_id is null and port=?2 order by revision desc limit 1",
                    params![run_id.as_str(), source_port],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
                )
                .optional()
                .map_err(sql_error)?
            } else {
                conn.query_row(
                    "select artifact.id,artifact.revision,artifact.digest,artifact.artifact_type,artifact.payload_inline from artifact join step_attempt attempt on attempt.id=artifact.producer_attempt_id join workflow_step step on step.id=attempt.step_id where artifact.run_id=?1 and step.definition_step_id=?2 and artifact.port=?3 and attempt.state='completed' order by artifact.created_unix_ms desc,artifact.revision desc limit 1",
                    params![run_id.as_str(), source_step, source_port],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
                )
                .optional()
                .map_err(sql_error)?
            };
            let (id, revision, digest, artifact_type, payload) = row.ok_or_else(|| {
                format!("input '{port}' is not available from '{}'", binding.from)
            })?;
            if artifact_type != binding.artifact_type {
                return Err(format!(
                    "input '{port}' expected '{}' but persisted Artifact is '{}'",
                    binding.artifact_type, artifact_type
                ));
            }
            input_artifacts.push(BoundArtifact {
                port,
                artifact: crate::run::ArtifactRef {
                    id: crate::run::ArtifactId(id),
                    revision,
                    digest,
                    artifact_type,
                },
                payload: serde_json::from_slice(&payload).map_err(|error| error.to_string())?,
            });
        }
        let input_digest = crate::run::sha256(
            &serde_json::to_vec(
                &input_artifacts
                    .iter()
                    .map(|bound| (&bound.port, &bound.artifact))
                    .collect::<BTreeMap<_, _>>(),
            )
            .map_err(|error| error.to_string())?,
        );
        drop(conn);
        self.coordinator.prepare(PrepareAttempt {
            run_id: run_id.clone(),
            step_id: step_id.clone(),
            input_digest,
            target_id,
            workspace,
            resource_claims,
            input_artifacts,
        })
    }

    pub(crate) fn request_approval(
        &self,
        run_id: &RunId,
        step_id: &StepId,
        mode: ApprovalMode,
        input_digest: &str,
        evidence_digest: &str,
        expires_unix_ms: Option<i64>,
    ) -> Result<ApprovalRequest, String> {
        self.ledger.create_approval(
            run_id,
            step_id,
            mode,
            input_digest,
            evidence_digest,
            expires_unix_ms,
        )
    }

    pub(crate) fn wait_until(
        &self,
        run_id: &RunId,
        step_id: &StepId,
        wake_unix_ms: i64,
    ) -> Result<(), String> {
        let conn = self.ledger.connection()?;
        let changed = conn.execute("update workflow_step set state='waiting',wake_unix_ms=?3,blocker='waiting for durable deadline',updated_unix_ms=?4 where id=?1 and run_id=?2 and class='wait'", params![step_id.as_str(),run_id.as_str(),wake_unix_ms,now_ms()]).map_err(sql_error)?;
        if changed == 0 {
            return Err("Wait Step was not found or has the wrong primitive class".to_string());
        }
        recompute_run_state(&conn, run_id, now_ms())
    }

    pub(crate) fn record_gate(
        &self,
        run_id: &RunId,
        step_id: &StepId,
        evidence: &GateEvidence,
    ) -> Result<RecordedGateResult, String> {
        let mut conn = self.ledger.connection()?;
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let attempt_id = AttemptId(random_id(&transaction)?);
        let now = now_ms();
        let inserted = transaction.execute("insert into step_attempt(id,run_id,step_id,ordinal,state,input_digest,implementation_id,implementation_revision,created_unix_ms,updated_unix_ms) select ?1,?2,?3,attempt_count+1,'waiting',?4,implementation_id,implementation_revision,?5,?5 from workflow_step where id=?3 and run_id=?2 and class='gate' and state in ('runnable','waiting')",params![attempt_id.as_str(),run_id.as_str(),step_id.as_str(),evidence.subject_digest,now]).map_err(sql_error)?;
        if inserted != 1 {
            return Err(
                "Gate Step is missing, not runnable/waiting, or has the wrong primitive class"
                    .to_string(),
            );
        }
        let mut status = evidence.status;
        if status == GateStatus::Satisfied && evidence.quality != EvidenceQuality::Current {
            status = GateStatus::Unknown;
        }
        let result_id = random_id(&transaction)?;
        transaction.execute("insert into gate_result(id,run_id,step_id,attempt_id,subject_digest,subject_generation,evidence_json,evidence_quality,policy_revision,status,reason,expires_unix_ms,created_unix_ms) values(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",params![result_id,run_id.as_str(),step_id.as_str(),attempt_id.as_str(),evidence.subject_digest,evidence.subject_generation,serde_json::to_string(&evidence.evidence).unwrap(),enum_text(&evidence.quality)?,evidence.policy_revision,status.label(),evidence.reason,evidence.expires_unix_ms,now]).map_err(sql_error)?;
        let (step_state, attempt_state) = match status {
            GateStatus::Satisfied | GateStatus::Unsatisfied => ("completed", "completed"),
            _ => ("waiting", "waiting"),
        };
        transaction.execute("update step_attempt set state=?2,terminal_reason=?3,updated_unix_ms=?4 where id=?1",params![attempt_id.as_str(),attempt_state,status.label(),now]).map_err(sql_error)?;
        transaction.execute("update workflow_step set state=?2,outcome=?3,attempt_count=attempt_count+1,blocker=?4,wake_unix_ms=?5,updated_unix_ms=?6 where id=?1",params![step_id.as_str(),step_state,status.label(),if step_state=="waiting" {Some(evidence.reason.as_str())} else {None},if step_state=="waiting" {Some(now.saturating_add(5_000))} else {None},now]).map_err(sql_error)?;
        recompute_run_state(&transaction, run_id, now)?;
        transaction.commit().map_err(sql_error)?;
        Ok(RecordedGateResult {
            id: result_id,
            status,
        })
    }

    pub(crate) fn notify(
        &self,
        run_id: &RunId,
        step_id: &StepId,
        category: &str,
        message: &str,
    ) -> Result<String, String> {
        let mut conn = self.ledger.connection()?;
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let id = random_id(&transaction)?;
        let now = now_ms();
        let valid: bool = transaction.query_row("select exists(select 1 from workflow_step where id=?1 and run_id=?2 and class='notification' and state='runnable')",params![step_id.as_str(),run_id.as_str()],|row|row.get(0)).map_err(sql_error)?;
        if !valid {
            return Err(
                "Notification Step is missing, not runnable, or has the wrong primitive class"
                    .to_string(),
            );
        }
        transaction.execute("insert into notification_delivery(id,run_id,step_id,category,message,state,created_unix_ms,updated_unix_ms) values(?1,?2,?3,?4,?5,'pending',?6,?6)",params![id,run_id.as_str(),step_id.as_str(),category,message,now]).map_err(sql_error)?;
        transaction.execute("update workflow_step set state='completed',outcome='delivery_queued',updated_unix_ms=?2 where id=?1",params![step_id.as_str(),now]).map_err(sql_error)?;
        recompute_run_state(&transaction, run_id, now)?;
        transaction.commit().map_err(sql_error)?;
        Ok(id)
    }

    pub(crate) fn record_notification_failure(
        &self,
        delivery_id: &str,
        error: &str,
    ) -> Result<(), String> {
        let conn = self.ledger.connection()?;
        conn.execute("update notification_delivery set state='failed',error=?2,updated_unix_ms=?3 where id=?1",params![delivery_id,error,now_ms()]).map_err(sql_error)?;
        Ok(())
    }

    pub(crate) fn call_child(&self, call: ChildCall) -> Result<RunId, String> {
        if !matches!(
            call.propagation.as_str(),
            "parent_only" | "lineage" | "detached"
        ) {
            return Err("invalid child control propagation".to_string());
        }
        let input_digest = {
            let ordered = call
                .inputs
                .iter()
                .map(|input| (&input.name, input))
                .collect::<BTreeMap<_, _>>();
            crate::run::sha256(&serde_json::to_vec(&ordered).map_err(|error| error.to_string())?)
        };
        let mut conn = self.ledger.connection()?;
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        if let Some(existing)=transaction.query_row("select child_run_id from workflow_call_reservation where parent_run_id=?1 and parent_step_id=?2 and call_key=?3 and child_snapshot_digest=?4 and input_digest=?5 and purpose=?6 and child_run_id is not null",params![call.parent_run.as_str(),call.parent_step.as_str(),call.call_key,call.snapshot.digest,input_digest,call.purpose],|row|row.get::<_,String>(0)).optional().map_err(sql_error)? { return Ok(RunId(existing)); }
        let reserved = transaction.execute("insert or ignore into workflow_call_reservation(parent_run_id,parent_step_id,call_key,child_snapshot_digest,input_digest,purpose,propagation,created_unix_ms) values(?1,?2,?3,?4,?5,?6,?7,?8)",params![call.parent_run.as_str(),call.parent_step.as_str(),call.call_key,call.snapshot.digest,input_digest,call.purpose,call.propagation,now_ms()]).map_err(sql_error)?;
        let parent: ParentCallContext = transaction.query_row("select r.budget_id,r.authority_grant_id,g.capabilities_json,g.expires_unix_ms,r.lineage_depth,b.remaining_child_depth,s.class,s.state,s.capabilities_json,r.control,r.repository_id from workflow_run r join authority_grant g on g.id=r.authority_grant_id join workflow_budget b on b.id=r.budget_id join workflow_step s on s.run_id=r.id and s.id=?2 where r.id=?1",params![call.parent_run.as_str(),call.parent_step.as_str()],|row|Ok(ParentCallContext{budget_id:row.get(0)?,grant_id:row.get(1)?,capabilities_json:row.get(2)?,grant_expiry:row.get(3)?,lineage_depth:row.get(4)?,max_child_depth:row.get(5)?,step_class:row.get(6)?,step_state:row.get(7)?,step_capabilities_json:row.get(8)?,control:row.get(9)?,repository_id:row.get::<_,Option<String>>(10)?.map(crate::run::RepositoryId)})).optional().map_err(sql_error)?.ok_or_else(||"Workflow Call Step does not belong to the parent Run".to_string())?;
        validate_pinned_child(
            &transaction,
            &call.parent_run,
            &call.parent_step,
            &call.snapshot,
        )?;
        if parent.step_class != "workflow_call"
            || !matches!(parent.step_state.as_str(), "runnable" | "waiting")
            || parent.control != "running"
        {
            return Err(
                "Workflow Call Step is not runnable, its Run is controlled, or it has the wrong primitive class".to_string(),
            );
        }
        if parent
            .grant_expiry
            .is_some_and(|expires| expires <= now_ms())
        {
            return Err("parent Authority Grant expired".to_string());
        }
        let parent_capabilities: BTreeSet<crate::definition::Capability> =
            serde_json::from_str(&parent.capabilities_json).map_err(|error| error.to_string())?;
        if !parent_capabilities.contains(&crate::definition::Capability::ChildWorkflowCreate) {
            return Err(
                "parent Authority Grant does not include child workflow creation".to_string(),
            );
        }
        let step_capabilities: BTreeSet<crate::definition::Capability> =
            serde_json::from_str(&parent.step_capabilities_json)
                .map_err(|error| error.to_string())?;
        if !step_capabilities.contains(&crate::definition::Capability::ChildWorkflowCreate) {
            return Err("Workflow Call Step does not declare child workflow creation".to_string());
        }
        if parent.lineage_depth + 1 > parent.max_child_depth {
            return Err("shared child-depth budget is exhausted".to_string());
        }
        let reconciliation_key = format!(
            "child:{}:{}:{}:{}",
            call.parent_run.as_str(),
            call.parent_step.as_str(),
            call.call_key,
            input_digest
        );
        let effect_intent_id = if reserved == 1 {
            let changed = transaction.execute("update workflow_budget set remaining_fan_out=remaining_fan_out-1,updated_unix_ms=?2 where id=?1 and remaining_fan_out>0",params![parent.budget_id,now_ms()]).map_err(sql_error)?;
            if changed == 0 {
                return Err("shared fan-out budget is exhausted".to_string());
            }
            transaction.execute("update workflow_run set remaining_fan_out=max(remaining_fan_out-1,0),revision=revision+1,updated_unix_ms=?2 where id=?1",params![call.parent_run.as_str(),now_ms()]).map_err(sql_error)?;
            let intent = prepare_child_intent(
                &transaction,
                ChildIntentRequest {
                    run_id: &call.parent_run,
                    step_id: &call.parent_step,
                    authority_grant_id: &crate::run::AuthorityGrantId(parent.grant_id.clone()),
                    reconciliation_key: &reconciliation_key,
                    child_snapshot_digest: &call.snapshot.digest,
                    input_digest: &input_digest,
                },
            )?;
            transaction.execute("update workflow_call_reservation set effect_intent_id=?7 where parent_run_id=?1 and parent_step_id=?2 and call_key=?3 and child_snapshot_digest=?4 and input_digest=?5 and purpose=?6",params![call.parent_run.as_str(),call.parent_step.as_str(),call.call_key,call.snapshot.digest,input_digest,call.purpose,intent.as_str()]).map_err(sql_error)?;
            intent
        } else {
            crate::run::EffectIntentId(transaction.query_row("select effect_intent_id from workflow_call_reservation where parent_run_id=?1 and parent_step_id=?2 and call_key=?3 and child_snapshot_digest=?4 and input_digest=?5 and purpose=?6",params![call.parent_run.as_str(),call.parent_step.as_str(),call.call_key,call.snapshot.digest,input_digest,call.purpose],|row|row.get(0)).map_err(sql_error)?)
        };
        transaction.commit().map_err(sql_error)?;
        let child = self
            .ledger
            .start_quarantined(StartRun {
                snapshot: call.snapshot.clone(),
                repository_id: parent.repository_id,
                inputs: call.inputs,
                idempotency_key: Some(format!(
                    "child:{}:{}:{}:{}",
                    call.parent_run.as_str(),
                    call.parent_step.as_str(),
                    call.call_key,
                    input_digest
                )),
                actor: format!("delegated:{}", call.parent_run.as_str()),
                actor_capabilities: parent_capabilities,
            })?
            .run_id;
        let mut conn = self.ledger.connection()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let child_budget_id: String = tx
            .query_row(
                "select budget_id from workflow_run where id=?1",
                [child.as_str()],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        tx.execute("update workflow_run set budget_id=?2,lineage_depth=?3,control=?4,remaining_attempts=(select remaining_attempts from workflow_budget where id=?2),remaining_fan_out=(select remaining_fan_out from workflow_budget where id=?2),remaining_child_depth=(select remaining_child_depth from workflow_budget where id=?2),remaining_mutations=(select remaining_mutations from workflow_budget where id=?2),updated_unix_ms=?5 where id=?1",params![child.as_str(),parent.budget_id,parent.lineage_depth+1,if call.start_paused { "pause_requested" } else { "running" },now_ms()]).map_err(sql_error)?;
        tx.execute("update authority_grant set parent_grant_id=?2,basis='delegated',expires_unix_ms=?3 where id=(select authority_grant_id from workflow_run where id=?1)",params![child.as_str(),parent.grant_id,parent.grant_expiry]).map_err(sql_error)?;
        tx.execute("insert or ignore into workflow_run_link(parent_run_id,parent_step_id,child_run_id,call_key,child_snapshot_digest,input_digest,purpose,propagation) values(?1,?2,?3,?4,?5,?6,?7,?8)",params![call.parent_run.as_str(),call.parent_step.as_str(),child.as_str(),call.call_key,call.snapshot.digest,input_digest,call.purpose,call.propagation]).map_err(sql_error)?;
        tx.execute("update workflow_call_reservation set child_run_id=?7 where parent_run_id=?1 and parent_step_id=?2 and call_key=?3 and child_snapshot_digest=?4 and input_digest=?5 and purpose=?6",params![call.parent_run.as_str(),call.parent_step.as_str(),call.call_key,call.snapshot.digest,input_digest,call.purpose,child.as_str()]).map_err(sql_error)?;
        complete_child_intent(&tx, &effect_intent_id, &child)?;
        tx.execute("update workflow_step set state='waiting',blocker='waiting for child workflow',updated_unix_ms=?2 where id=?1",params![call.parent_step.as_str(),now_ms()]).map_err(sql_error)?;
        tx.execute("delete from workflow_budget where id=?1 and not exists(select 1 from workflow_run where budget_id=?1)",[child_budget_id]).map_err(sql_error)?;
        tx.commit().map_err(sql_error)?;
        Ok(child)
    }
}

fn load_attempt_inputs(
    conn: &rusqlite::Connection,
    attempt_id: &AttemptId,
) -> Result<Vec<BoundArtifact>, String> {
    let mut statement = conn.prepare("select i.port,a.id,a.revision,a.digest,a.artifact_type,a.payload_inline from attempt_input i join artifact a on a.id=i.artifact_id and a.revision=i.artifact_revision where i.attempt_id=?1 order by i.port").map_err(sql_error)?;
    statement
        .query_map([attempt_id.as_str()], |row| {
            Ok(BoundArtifact {
                port: row.get(0)?,
                artifact: crate::run::ArtifactRef {
                    id: crate::run::ArtifactId(row.get(1)?),
                    revision: row.get(2)?,
                    digest: row.get(3)?,
                    artifact_type: row.get(4)?,
                },
                payload: serde_json::from_slice(&row.get::<_, Vec<u8>>(5)?).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        5,
                        rusqlite::types::Type::Blob,
                        Box::new(error),
                    )
                })?,
            })
        })
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)
}

fn validate_pinned_child(
    conn: &rusqlite::Connection,
    parent_run: &RunId,
    parent_step: &StepId,
    child: &DefinitionSnapshot,
) -> Result<(), String> {
    let (bytes, definition_step_id): (Vec<u8>, String) = conn
        .query_row(
            "select snapshot.canonical_bytes,step.definition_step_id from workflow_run run join definition_snapshot snapshot on snapshot.digest=run.snapshot_digest join workflow_step step on step.run_id=run.id where run.id=?1 and step.id=?2",
            params![parent_run.as_str(), parent_step.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(sql_error)?;
    let snapshot: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    let pinned = snapshot
        .get("steps")
        .and_then(serde_json::Value::as_array)
        .and_then(|steps| {
            steps.iter().find(|step| {
                step.get("id").and_then(serde_json::Value::as_str)
                    == Some(definition_step_id.as_str())
            })
        })
        .and_then(|step| step.get("child_workflow"))
        .ok_or_else(|| "Workflow Call Step has no parent-pinned child Workflow".to_string())?;
    let matches = pinned
        .get("qualified_name")
        .and_then(serde_json::Value::as_str)
        == Some(child.content.qualified_name.as_str())
        && pinned.get("revision").and_then(serde_json::Value::as_str)
            == Some(child.content.source_revision.as_str())
        && pinned.get("digest").and_then(serde_json::Value::as_str) == Some(child.digest.as_str());
    if !matches {
        return Err("child Workflow does not match the parent-pinned snapshot".to_string());
    }
    Ok(())
}

fn project_child_completion(conn: &rusqlite::Connection, parent_run: &RunId) -> Result<(), String> {
    let links = {
        let mut statement = conn.prepare("select link.parent_step_id,child.state from workflow_run_link link join workflow_run child on child.id=link.child_run_id where link.parent_run_id=?1").map_err(sql_error)?;
        statement
            .query_map([parent_run.as_str()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?
    };
    let mut by_step = BTreeMap::<String, Vec<String>>::new();
    for (step, child_state) in links {
        by_step.entry(step).or_default().push(child_state);
    }
    for (step, child_states) in by_step {
        let now = now_ms();
        if child_states
            .iter()
            .any(|state| state == "recovery_required")
        {
            conn.execute("update workflow_step set state='recovery_required',blocker='child Run requires recovery',updated_unix_ms=?2 where id=?1 and state='waiting'",params![step,now]).map_err(sql_error)?;
        } else if let Some(failed) = child_states
            .iter()
            .find(|state| matches!(state.as_str(), "failed" | "cancelled"))
        {
            conn.execute("update workflow_step set state='failed',outcome='child_failed',blocker=?2,updated_unix_ms=?3 where id=?1 and state='waiting'",params![step,format!("child Run is {failed}"),now]).map_err(sql_error)?;
        } else if child_states.iter().all(|state| state == "completed") {
            conn.execute("update workflow_step set state='completed',outcome='child_completed',blocker=null,updated_unix_ms=?2 where id=?1 and state='waiting'",params![step,now]).map_err(sql_error)?;
        }
    }
    Ok(())
}

fn load_condition_values(
    conn: &rusqlite::Connection,
    run_id: &RunId,
    expression: &ConditionExpr,
) -> Result<BTreeMap<String, ObservedBool>, String> {
    let mut references = BTreeSet::new();
    collect_condition_references(expression, &mut references);
    references
        .into_iter()
        .filter(|reference| !matches!(reference.as_str(), "true" | "false"))
        .map(|reference| {
            let parts = reference.split('.').collect::<Vec<_>>();
            if let ["step", definition_step_id, "outcome"] = parts.as_slice() {
                    let outcome: Option<String> = conn
                        .query_row(
                            "select outcome from workflow_step where run_id=?1 and definition_step_id=?2 and state in ('completed','failed','skipped','cancelled')",
                            params![run_id.as_str(), definition_step_id],
                            |row| row.get(0),
                        )
                        .optional()
                        .map_err(sql_error)?
                        .flatten();
                    let observation = outcome.map_or(ObservedBool::Missing, |outcome| {
                        ObservedBool::Value(matches!(
                            outcome.as_str(),
                            "succeeded" | "approved" | "satisfied" | "delivery_queued"
                        ))
                    });
                    return Ok((reference, observation));
            }
            let (payload, field_path): (Option<Vec<u8>>, &[&str]) = match parts.as_slice() {
                ["run", port, fields @ ..] => (conn
                    .query_row(
                        "select payload_inline from artifact where run_id=?1 and producer_attempt_id is null and port=?2 order by revision desc limit 1",
                        params![run_id.as_str(), port],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(sql_error)?, fields),
                ["step", definition_step_id, port, fields @ ..] => (conn
                    .query_row(
                        "select a.payload_inline from artifact a join step_attempt attempt on attempt.id=a.producer_attempt_id join workflow_step step on step.id=attempt.step_id where a.run_id=?1 and step.definition_step_id=?2 and a.port=?3 and attempt.state='completed' order by a.created_unix_ms desc,a.revision desc limit 1",
                        params![run_id.as_str(), definition_step_id, port],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(sql_error)?, fields),
                _ => (None, &[]),
            };
            let observation = match payload {
                None => ObservedBool::Missing,
                Some(payload) => condition_observation(&payload, field_path),
            };
            Ok((reference, observation))
        })
        .collect()
}

fn condition_observation(payload: &[u8], field_path: &[&str]) -> ObservedBool {
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(payload) else {
        return ObservedBool::Unknown;
    };
    if let Some(quality) = value.get("quality").and_then(serde_json::Value::as_str) {
        match quality {
            "missing" => return ObservedBool::Missing,
            "stale" => return ObservedBool::Stale,
            "unknown" => return ObservedBool::Unknown,
            "unavailable" => return ObservedBool::Unavailable,
            "current" => {
                if let Some(observed) = value.get("value") {
                    value = observed.clone();
                }
            }
            _ => return ObservedBool::Unknown,
        }
    }
    for field in field_path {
        let Some(next) = value.get(*field) else {
            return ObservedBool::Missing;
        };
        value = next.clone();
    }
    value
        .as_bool()
        .map(ObservedBool::Value)
        .unwrap_or(ObservedBool::Unknown)
}

fn collect_condition_references(expression: &ConditionExpr, output: &mut BTreeSet<String>) {
    match expression {
        ConditionExpr::Literal(_) => {}
        ConditionExpr::Reference(reference) => {
            output.insert(reference.clone());
        }
        ConditionExpr::Not(expression) => collect_condition_references(expression, output),
        ConditionExpr::All(expressions) | ConditionExpr::Any(expressions) => {
            for expression in expressions {
                collect_condition_references(expression, output);
            }
        }
        ConditionExpr::Equal { left, right } | ConditionExpr::NotEqual { left, right } => {
            output.insert(left.clone());
            output.insert(right.clone());
        }
    }
}

struct ScheduleStep {
    id: StepId,
    definition_id: String,
    state: String,
    outcome: Option<String>,
    dependencies: String,
    condition: Option<String>,
    wake_unix_ms: Option<i64>,
}
fn load_schedule_steps(
    conn: &rusqlite::Connection,
    run_id: &RunId,
) -> Result<Vec<ScheduleStep>, String> {
    let mut statement=conn.prepare("select id,definition_step_id,state,outcome,dependencies_json,condition_json,wake_unix_ms from workflow_step where run_id=?1 order by rowid").map_err(sql_error)?;
    statement
        .query_map([run_id.as_str()], |row| {
            Ok(ScheduleStep {
                id: StepId(row.get(0)?),
                definition_id: row.get(1)?,
                state: row.get(2)?,
                outcome: row.get(3)?,
                dependencies: row.get(4)?,
                condition: row.get(5)?,
                wake_unix_ms: row.get(6)?,
            })
        })
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)
}

fn enum_text(value: &impl Serialize) -> Result<String, String> {
    serde_json::to_value(value)
        .map_err(|error| error.to_string())?
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "enum did not serialize as text".to_string())
}
fn parse_trust_class(value: &str) -> Result<crate::run::TrustClass, String> {
    match value {
        "trusted" => Ok(crate::run::TrustClass::Trusted),
        "untrusted" => Ok(crate::run::TrustClass::Untrusted),
        "derived_untrusted" => Ok(crate::run::TrustClass::DerivedUntrusted),
        value => Err(format!("invalid persisted Artifact trust class '{value}'")),
    }
}

fn parse_sensitivity(value: &str) -> Result<crate::run::Sensitivity, String> {
    match value {
        "public" => Ok(crate::run::Sensitivity::Public),
        "internal" => Ok(crate::run::Sensitivity::Internal),
        "sensitive" => Ok(crate::run::Sensitivity::Sensitive),
        value => Err(format!("invalid persisted Artifact sensitivity '{value}'")),
    }
}

fn sql_error(error: rusqlite::Error) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::DefinitionCatalog;
    use crate::run::{Sensitivity, TrustClass};
    use std::collections::BTreeSet;

    fn setup(
        selector: &str,
    ) -> (
        WorkflowOperations,
        RunId,
        Vec<StepId>,
        std::path::PathBuf,
        String,
    ) {
        let path = std::env::temp_dir().join(format!(
            "prism-operations-{}-{}-{:?}.db",
            std::process::id(),
            now_ms(),
            std::thread::current().id()
        ));
        let ledger = RunLedger::open(path.clone()).unwrap();
        let snapshot = DefinitionCatalog::discover(None).resolve(selector).unwrap();
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
                actor_capabilities: BTreeSet::new(),
            })
            .unwrap();
        let steps: Vec<StepId> = ledger
            .inspect(&run.run_id)
            .unwrap()
            .steps
            .into_iter()
            .map(|step| step.id)
            .collect();
        let input_digest = ledger.step_input_digest(&run.run_id, &steps[0]).unwrap();
        (
            WorkflowOperations::new(ledger),
            run.run_id,
            steps,
            path,
            input_digest,
        )
    }

    #[test]
    fn approval_waits_without_lease_and_resume_does_not_approve() {
        let (ops, run, steps, path, digest) = setup("builtin:approval");
        ops.schedule(&run).unwrap();
        let request = ops
            .request_approval(
                &run,
                &steps[0],
                ApprovalMode::ArtifactAcceptance,
                &digest,
                "evidence",
                None,
            )
            .unwrap();
        ops.execute(WorkflowCommand::Resume(run.clone())).unwrap();
        assert_eq!(
            ops.query(&run).unwrap().run.state,
            crate::run::RunState::InputRequired
        );
        ops.execute(WorkflowCommand::Approve {
            request_id: request.id,
            input_digest: digest,
            evidence_digest: "evidence".into(),
            actor: "test".into(),
            reason: None,
        })
        .unwrap();
        assert_eq!(
            ops.query(&run).unwrap().run.state,
            crate::run::RunState::Completed
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn stale_gate_evidence_cannot_satisfy_mutation_gate() {
        let evidence = GateEvidence {
            subject_digest: "head".into(),
            subject_generation: "1".into(),
            evidence: vec![],
            quality: EvidenceQuality::Stale,
            policy_revision: "1".into(),
            status: GateStatus::Satisfied,
            reason: "cached".into(),
            expires_unix_ms: None,
        };
        assert!(!evidence.authorizes_mutation("1"));
    }

    #[test]
    fn scheduler_reads_condition_values_from_persisted_artifacts() {
        let (ops, run, steps, path, _) = setup("builtin:approval");
        let conn = ops.ledger.connection().unwrap();
        conn.execute(
            "update workflow_step set condition_json='{\"Reference\":\"run.task\"}' where id=?1",
            [steps[0].as_str()],
        )
        .unwrap();
        conn.execute("update artifact set payload_inline=?2 where run_id=?1 and producer_attempt_id is null and port='task'",params![run.as_str(),serde_json::to_vec(&true).unwrap()]).unwrap();
        drop(conn);
        assert_eq!(ops.schedule(&run).unwrap(), vec![steps[0].clone()]);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn due_wait_completes_without_a_worker_lease() {
        let (ops, run, steps, path, _) = setup("builtin:approval");
        let conn = ops.ledger.connection().unwrap();
        conn.execute(
            "update workflow_step set class='wait',state='runnable' where id=?1",
            [steps[0].as_str()],
        )
        .unwrap();
        drop(conn);
        ops.wait_until(&run, &steps[0], now_ms() - 1).unwrap();
        assert!(ops.schedule(&run).unwrap().is_empty());
        assert_eq!(
            ops.query(&run).unwrap().steps[0].state,
            crate::run::StepState::Completed
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn notification_failure_does_not_change_run_outcome() {
        let (ops, run, steps, path, _) = setup("builtin:approval");
        let conn = ops.ledger.connection().unwrap();
        conn.execute(
            "update workflow_step set class='notification',state='runnable' where id=?1",
            [steps[0].as_str()],
        )
        .unwrap();
        drop(conn);
        let delivery = ops.notify(&run, &steps[0], "completed", "done").unwrap();
        assert_eq!(
            ops.query(&run).unwrap().run.state,
            crate::run::RunState::Completed
        );
        ops.record_notification_failure(&delivery, "offline")
            .unwrap();
        assert_eq!(
            ops.query(&run).unwrap().run.state,
            crate::run::RunState::Completed
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn child_call_is_idempotent_and_shares_parent_budget_and_authority() {
        let (ops, run, steps, path, _) = setup("builtin:approval");
        let conn = ops.ledger.connection().unwrap();
        conn.execute(
            "update workflow_step set class='workflow_call',state='runnable',capabilities_json='[\"child_workflow_create\"]' where id=?1",
            [steps[0].as_str()],
        )
        .unwrap();
        conn.execute("update authority_grant set capabilities_json='[\"child_workflow_create\"]' where id=(select authority_grant_id from workflow_run where id=?1)",[run.as_str()]).unwrap();
        let expiry = now_ms() + 60_000;
        conn.execute("update authority_grant set expires_unix_ms=?2 where id=(select authority_grant_id from workflow_run where id=?1)",params![run.as_str(),expiry]).unwrap();
        let child_snapshot = DefinitionCatalog::discover(None)
            .resolve("builtin:approval")
            .unwrap();
        let (snapshot_digest, bytes): (String, Vec<u8>) = conn.query_row("select r.snapshot_digest,s.canonical_bytes from workflow_run r join definition_snapshot s on s.digest=r.snapshot_digest where r.id=?1",[run.as_str()],|row|Ok((row.get(0)?,row.get(1)?))).unwrap();
        let mut snapshot: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        snapshot["steps"][0]["child_workflow"] = serde_json::json!({
            "qualified_name":child_snapshot.content.qualified_name.clone(),
            "revision":child_snapshot.content.source_revision.clone(),
            "digest":child_snapshot.digest.clone(),
            "capabilities":[]
        });
        conn.execute(
            "update definition_snapshot set canonical_bytes=?2 where digest=?1",
            params![snapshot_digest, serde_json::to_vec(&snapshot).unwrap()],
        )
        .unwrap();
        drop(conn);
        let make_call = || ChildCall {
            parent_run: run.clone(),
            parent_step: steps[0].clone(),
            call_key: "phase".into(),
            purpose: "test".into(),
            propagation: "lineage".into(),
            start_paused: false,
            snapshot: child_snapshot.clone(),
            inputs: vec![ArtifactInput {
                name: "task".into(),
                artifact_type: "builtin:task@1".into(),
                payload: serde_json::json!({"task":"child"}),
                trust: TrustClass::Trusted,
                sensitivity: Sensitivity::Internal,
            }],
        };
        let child = ops.call_child(make_call()).unwrap();
        assert_eq!(ops.call_child(make_call()).unwrap(), child);
        let conn = ops.ledger.connection().unwrap();
        let (parent_budget,child_budget):(String,String)=conn.query_row("select p.budget_id,c.budget_id from workflow_run p join workflow_run c on c.id=?2 where p.id=?1",params![run.as_str(),child.as_str()],|row|Ok((row.get(0)?,row.get(1)?))).unwrap();
        assert_eq!(parent_budget, child_budget);
        let remaining: i64 = conn
            .query_row(
                "select remaining_fan_out from workflow_budget where id=?1",
                [parent_budget],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 0);
        let delegated:i64=conn.query_row("select count(*) from authority_grant where id=(select authority_grant_id from workflow_run where id=?1) and parent_grant_id is not null and basis='delegated'",[child.as_str()],|row|row.get(0)).unwrap();
        assert_eq!(delegated, 1);
        let child_expiry: i64 = conn.query_row("select expires_unix_ms from authority_grant where id=(select authority_grant_id from workflow_run where id=?1)",[child.as_str()],|row|row.get(0)).unwrap();
        assert_eq!(child_expiry, expiry);
        drop(conn);
        ops.execute(WorkflowCommand::Pause(run.clone())).unwrap();
        assert_eq!(ops.query(&child).unwrap().run.control, "pause_requested");
        ops.execute(WorkflowCommand::Resume(run.clone())).unwrap();
        let conn = ops.ledger.connection().unwrap();
        conn.execute(
            "update workflow_step set state='completed',outcome='approved' where run_id=?1",
            [child.as_str()],
        )
        .unwrap();
        crate::run::recompute_run_state(&conn, &child, now_ms()).unwrap();
        drop(conn);
        ops.schedule(&run).unwrap();
        assert_eq!(
            ops.query(&run).unwrap().steps[0].state,
            crate::run::StepState::Completed
        );
        std::fs::remove_file(path).unwrap();
    }
}
