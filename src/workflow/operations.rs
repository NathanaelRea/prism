use std::path::Path;

use crate::persistence::approvals::{ApprovalDecision as PersistedApprovalDecision, ApprovalStore};
use crate::persistence::control_plane::AsyncCoordinator;
use crate::persistence::effects::EffectBroker;
use crate::persistence::error::DatabaseError;
use crate::persistence::pools::WorkflowDatabase;
use crate::persistence::run_ledger::{
    MaterializedStep, RegisterDefinition, RunCommand as PersistedRunCommand, RunLedger, StartRun,
};
use crate::persistence::triggers::{RecordDispatch, TriggerStore};
use crate::persistence::wakeups::WakeupStore;
use crate::workflow::trigger::{
    AdmissionDecision as TriggerAdmissionDecision, OverlapPolicy, ProviderItemKind,
    ProviderPollAdapter, ProviderPollPage, ProviderPollRequest, TriggerOccurrenceStatus,
    TriggerRegistration, TriggerSchedule,
};

/// Deep async interface for generalized workflow commands and projections.
/// SQL, pools, transactions, idempotency, and row conversion remain private.
#[derive(Clone)]
pub struct WorkflowOperations {
    database: WorkflowDatabase,
    ledger: RunLedger,
    approvals: ApprovalStore,
    effects: EffectBroker,
    wakeups: WakeupStore,
    triggers: TriggerStore,
    execution: Option<super::engine::ExecutionControl>,
}

pub struct DefinitionSnapshot<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub revision: &'a str,
    pub source: &'a str,
    pub trusted: bool,
    pub body_json: &'a str,
    pub digest: &'a str,
    pub now_unix_ms: i64,
}

#[derive(Clone, Debug)]
pub struct WorkflowStep {
    pub id: String,
    pub key: String,
    pub implementation: String,
    pub target_id: String,
    pub input_json: String,
    pub dependencies: Vec<String>,
    pub resources: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowCommand {
    Pause,
    Resume,
    Cancel,
    Retry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowControlScope {
    Run,
    Parent,
    Lineage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    Approve,
    Reject,
}

pub struct EvidenceBoundApproval<'a> {
    pub id: &'a str,
    pub run_id: &'a str,
    pub step_id: &'a str,
    pub subject_json: &'a str,
    pub evidence_json: &'a str,
    pub policy_json: &'a str,
    pub now_unix_ms: i64,
}

pub struct LaunchWorkflow<'a> {
    pub run_id: &'a str,
    pub definition_snapshot_id: &'a str,
    pub repository: Option<&'a str>,
    pub idempotency_key: &'a str,
    /// Typed run inputs keyed by the snapshot's declared input port names.
    pub input_json: &'a str,
    pub now_unix_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct WorkflowProjection {
    pub id: String,
    pub definition_name: String,
    pub status: String,
    pub repository: Option<String>,
    pub created_unix_ms: i64,
    pub updated_unix_ms: i64,
    pub completed_unix_ms: Option<i64>,
    #[serde(default)]
    pub parent_run_id: Option<String>,
    #[serde(default)]
    pub lineage_root_id: Option<String>,
    #[serde(default)]
    pub archived_unix_ms: Option<i64>,
    #[serde(default)]
    pub detached: bool,
    #[serde(default)]
    pub attempt_budget: Option<i64>,
    #[serde(default)]
    pub attempts_consumed: i64,
    pub steps: Vec<WorkflowStepProjection>,
    pub attempts: Vec<WorkflowAttemptProjection>,
    pub artifacts: Vec<WorkflowArtifactProjection>,
    pub approvals: Vec<WorkflowApprovalProjection>,
    pub effects: Vec<WorkflowEffectProjection>,
    pub gates: Vec<WorkflowGateProjection>,
    pub events: Vec<WorkflowAuditEvent>,
    #[serde(default)]
    pub children: Vec<WorkflowChildProjection>,
    #[serde(default)]
    pub authority: Vec<WorkflowAuthorityProjection>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct WorkflowChildProjection {
    pub step_id: String,
    pub iteration: i64,
    pub run_id: String,
    pub status: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct WorkflowAuthorityProjection {
    pub scope: String,
    pub granted_by: String,
    pub granted_unix_ms: i64,
    pub expires_unix_ms: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct WorkflowStepProjection {
    pub id: String,
    pub key: String,
    pub implementation: String,
    pub target_id: String,
    pub status: String,
    pub input_json: String,
    #[serde(default = "default_action_class")]
    pub class: String,
    #[serde(default = "default_effect_boundary")]
    pub effect_boundary: String,
    #[serde(default)]
    pub skippable: bool,
    #[serde(default)]
    pub dependencies: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct WorkflowAttemptProjection {
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
    #[serde(default)]
    pub input_revisions_json: String,
    #[serde(default)]
    pub bindings: Vec<WorkflowAttemptBindingProjection>,
    pub output: Vec<WorkflowOutputProjection>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct WorkflowAttemptBindingProjection {
    pub name: String,
    pub schema_id: String,
    pub value_json: String,
    pub artifact_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct WorkflowOutputProjection {
    pub sequence: i64,
    pub stream: String,
    pub body: Vec<u8>,
    pub time_unix_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct WorkflowArtifactProjection {
    pub id: String,
    pub producing_attempt_id: Option<String>,
    pub revision: i64,
    pub digest: String,
    pub size_bytes: i64,
    pub sensitivity: String,
    pub inline_body: Option<Vec<u8>>,
    pub file_path: Option<String>,
    pub created_unix_ms: i64,
    pub provider_item_id: Option<String>,
    pub observation_revision: Option<String>,
    pub trigger_occurrence_id: Option<String>,
    pub admission_decision_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct WorkflowApprovalProjection {
    pub id: String,
    pub step_id: Option<String>,
    pub status: String,
    pub requested_unix_ms: i64,
    pub decided_unix_ms: Option<i64>,
    pub decided_by: Option<String>,
    pub decision_note: Option<String>,
    #[serde(default)]
    pub subject_json: Option<String>,
    #[serde(default)]
    pub evidence_json: Option<String>,
    #[serde(default)]
    pub policy_json: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct WorkflowEffectProjection {
    pub id: String,
    pub attempt_id: String,
    pub effect_kind: String,
    pub idempotency_key: String,
    pub status: String,
    pub request_json: String,
    pub result_json: Option<String>,
    pub created_unix_ms: i64,
    pub updated_unix_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct WorkflowGateProjection {
    pub step_id: String,
    pub gate_kind: String,
    pub due_unix_ms: i64,
    pub checkpoint_json: String,
    pub poll_count: i64,
    #[serde(default)]
    pub subject_json: Option<String>,
    #[serde(default)]
    pub evidence_json: Option<String>,
    #[serde(default)]
    pub policy_json: Option<String>,
}

fn default_action_class() -> String {
    "action".into()
}

fn default_effect_boundary() -> String {
    "none".into()
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct WorkflowAuditEvent {
    pub sequence: i64,
    pub step_id: Option<String>,
    pub attempt_id: Option<String>,
    pub kind: String,
    pub time_unix_ms: i64,
    pub data_json: String,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ControlPlaneMetric {
    pub name: String,
    pub value: i64,
    pub labels_json: String,
    pub time_unix_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct TriggerProjection {
    pub id: String,
    pub definition_snapshot_id: String,
    pub overlap_policy: OverlapPolicy,
    pub schedule: TriggerSchedule,
    pub config: serde_json::Value,
    pub admission_purpose: String,
    pub enabled: bool,
    pub checkpoint: Option<serde_json::Value>,
    pub checkpoint_unix_ms: Option<i64>,
    pub consecutive_poll_failures: Option<i64>,
    pub retry_after_unix_ms: Option<i64>,
    pub poll_diagnostic: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct TriggerHistoryProjection {
    pub id: String,
    pub trigger_id: String,
    pub deduplication_key: String,
    pub due_unix_ms: i64,
    pub status: TriggerOccurrenceStatus,
    pub run_id: Option<String>,
    pub provider_item_id: Option<String>,
    pub observation_revision: Option<String>,
    pub diagnostic: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct ProviderObservationProjection {
    pub provider_item_id: String,
    pub item_kind: ProviderItemKind,
    pub observation_revision: String,
    pub observation: serde_json::Value,
    pub observed_unix_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct TriggerDoctorDiagnostic {
    pub trigger_id: String,
    pub severity: String,
    pub message: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct WorkflowHealthReport {
    pub orphaned_definition_snapshots: i64,
    pub quarantined_workspaces: i64,
    pub indeterminate_effects: i64,
    pub recovery_required_runs: i64,
    pub invalid_child_links: i64,
    pub artifact_integrity_failures: Vec<ArtifactIntegrityFailure>,
}

impl WorkflowHealthReport {
    pub fn healthy(&self) -> bool {
        self.quarantined_workspaces == 0
            && self.indeterminate_effects == 0
            && self.recovery_required_runs == 0
            && self.invalid_child_links == 0
            && self.artifact_integrity_failures.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct ArtifactIntegrityFailure {
    pub artifact_id: String,
    pub reason: String,
}

pub struct LaunchAdmittedImplementation<'a> {
    pub provider_item_id: &'a str,
    pub observation_revision: &'a str,
    pub purpose: &'a str,
    pub intake_run_id: &'a str,
    pub child_run_id: &'a str,
    pub definition_snapshot_id: &'a str,
    pub repository: Option<&'a str>,
    pub input_json: &'a str,
    pub now_unix_ms: i64,
}

impl WorkflowOperations {
    pub async fn open(path: &Path) -> Result<Self, WorkflowOperationError> {
        let database = WorkflowDatabase::open(path).await?;
        Ok(Self::from_database(database))
    }

    pub async fn open_default() -> Result<Self, WorkflowOperationError> {
        let database = WorkflowDatabase::open_default().await?;
        Ok(Self::from_database(database))
    }

    pub(crate) fn from_database(database: WorkflowDatabase) -> Self {
        Self::from_database_with_execution(database, None)
    }

    pub(crate) fn from_database_with_execution(
        database: WorkflowDatabase,
        execution: Option<super::engine::ExecutionControl>,
    ) -> Self {
        Self {
            ledger: RunLedger::new(database.clone()),
            approvals: ApprovalStore::new(database.clone()),
            effects: EffectBroker::new(database.clone()),
            wakeups: WakeupStore::new(database.clone()),
            triggers: TriggerStore::new(database.clone()),
            execution,
            database,
        }
    }

    pub fn database_path(&self) -> &Path {
        self.database.path()
    }

    pub async fn register_definition(
        &self,
        definition: DefinitionSnapshot<'_>,
    ) -> Result<(), WorkflowOperationError> {
        self.ledger
            .register_definition(RegisterDefinition {
                id: definition.id,
                name: definition.name,
                revision: definition.revision,
                source: definition.source,
                trusted: definition.trusted,
                body_json: definition.body_json,
                digest: definition.digest,
                now_unix_ms: definition.now_unix_ms,
            })
            .await
            .map_err(Into::into)
    }

    pub async fn launch(
        &self,
        command: LaunchWorkflow<'_>,
    ) -> Result<String, WorkflowOperationError> {
        let now = command.now_unix_ms;
        let run_id = self
            .ledger
            .start(start_command(&command))
            .await
            .map_err(WorkflowOperationError::from)?;
        AsyncCoordinator::new(self.database.clone())
            .refresh_readiness(now)
            .await
            .map_err(WorkflowOperationError::from)?;
        Ok(run_id)
    }

    /// Launch a run from the immutable steps resolved from its definition snapshot.
    pub async fn launch_definition(
        &self,
        command: LaunchWorkflow<'_>,
        steps: Vec<WorkflowStep>,
    ) -> Result<String, WorkflowOperationError> {
        self.ledger
            .start_materialized(
                start_command(&command),
                steps
                    .into_iter()
                    .map(|step| MaterializedStep {
                        id: step.id,
                        key: step.key,
                        implementation: step.implementation,
                        target_id: step.target_id,
                        input_json: step.input_json,
                        dependencies: step.dependencies,
                        resources: step.resources,
                    })
                    .collect(),
            )
            .await
            .map_err(Into::into)
    }

    pub async fn command(
        &self,
        run_id: &str,
        command: WorkflowCommand,
        now_unix_ms: i64,
    ) -> Result<(), WorkflowOperationError> {
        self.ledger
            .command(
                run_id,
                match command {
                    WorkflowCommand::Pause => PersistedRunCommand::Pause,
                    WorkflowCommand::Resume => PersistedRunCommand::Resume,
                    WorkflowCommand::Cancel => PersistedRunCommand::Cancel,
                    WorkflowCommand::Retry => PersistedRunCommand::Retry,
                },
                now_unix_ms,
            )
            .await
            .map_err(WorkflowOperationError::from)?;
        if command == WorkflowCommand::Cancel
            && let Some(execution) = &self.execution
        {
            execution.cancel_run(run_id);
        }
        Ok(())
    }

    pub async fn command_scoped(
        &self,
        run_id: &str,
        scope: WorkflowControlScope,
        command: WorkflowCommand,
        now_unix_ms: i64,
    ) -> Result<(), WorkflowOperationError> {
        let target = self.ledger.control_target(run_id, scope).await?;
        self.command(&target, command, now_unix_ms).await
    }

    pub async fn detach_child(
        &self,
        child_run_id: &str,
        detached: bool,
        now_unix_ms: i64,
    ) -> Result<(), WorkflowOperationError> {
        self.ledger
            .set_detached(child_run_id, detached, now_unix_ms)
            .await
            .map_err(Into::into)
    }

    pub async fn restart_from_step(
        &self,
        run_id: &str,
        step_key: &str,
        now_unix_ms: i64,
    ) -> Result<(), WorkflowOperationError> {
        self.ledger
            .restart_from_step(run_id, step_key, now_unix_ms)
            .await
            .map_err(WorkflowOperationError::from)?;
        AsyncCoordinator::new(self.database.clone())
            .refresh_readiness(now_unix_ms)
            .await
            .map_err(Into::into)
    }

    pub async fn skip_step(
        &self,
        run_id: &str,
        step_key: &str,
        now_unix_ms: i64,
    ) -> Result<(), WorkflowOperationError> {
        self.ledger
            .skip_step(run_id, step_key, now_unix_ms)
            .await
            .map_err(Into::into)
    }

    pub async fn archive(
        &self,
        run_id: &str,
        now_unix_ms: i64,
    ) -> Result<(), WorkflowOperationError> {
        self.ledger
            .archive(run_id, now_unix_ms)
            .await
            .map_err(Into::into)
    }

    pub async fn quarantine_workspace(
        &self,
        workspace_id: &str,
        reason: &str,
        now_unix_ms: i64,
    ) -> Result<(), WorkflowOperationError> {
        self.ledger
            .quarantine_workspace(workspace_id, reason, now_unix_ms)
            .await
            .map_err(Into::into)
    }

    pub async fn resolve_input_required(
        &self,
        run_id: &str,
        additional_attempts: u32,
        now_unix_ms: i64,
    ) -> Result<(), WorkflowOperationError> {
        self.ledger
            .add_attempt_budget(run_id, additional_attempts, now_unix_ms)
            .await
            .map_err(WorkflowOperationError::from)?;
        AsyncCoordinator::new(self.database.clone())
            .refresh_readiness(now_unix_ms)
            .await
            .map_err(Into::into)
    }

    pub async fn request_approval(
        &self,
        id: &str,
        run_id: &str,
        step_id: &str,
        now_unix_ms: i64,
    ) -> Result<(), WorkflowOperationError> {
        self.approvals
            .request(id, run_id, step_id, now_unix_ms)
            .await
            .map_err(Into::into)
    }

    pub async fn request_evidence_bound_approval(
        &self,
        request: EvidenceBoundApproval<'_>,
    ) -> Result<(), WorkflowOperationError> {
        self.approvals
            .request_evidence(crate::persistence::approvals::EvidenceRequest {
                id: request.id,
                run_id: request.run_id,
                step_id: request.step_id,
                subject_json: request.subject_json,
                evidence_json: request.evidence_json,
                policy_json: request.policy_json,
                now_unix_ms: request.now_unix_ms,
            })
            .await
            .map_err(Into::into)
    }

    pub async fn decide_approval(
        &self,
        id: &str,
        decision: ApprovalDecision,
        decided_by: &str,
        note: Option<&str>,
        now_unix_ms: i64,
    ) -> Result<(), WorkflowOperationError> {
        self.approvals
            .decide(
                id,
                match decision {
                    ApprovalDecision::Approve => PersistedApprovalDecision::Approve,
                    ApprovalDecision::Reject => PersistedApprovalDecision::Reject,
                },
                decided_by,
                note,
                now_unix_ms,
            )
            .await
            .map_err(Into::into)
    }

    pub async fn grant_authority(
        &self,
        id: &str,
        run_id: &str,
        scope: &str,
        granted_by: &str,
        now_unix_ms: i64,
        expires_unix_ms: Option<i64>,
    ) -> Result<(), WorkflowOperationError> {
        self.effects
            .grant_authority(id, run_id, scope, granted_by, now_unix_ms, expires_unix_ms)
            .await
            .map_err(Into::into)
    }

    pub async fn register_trigger(
        &self,
        id: &str,
        definition_snapshot_id: &str,
        overlap_policy: &str,
        config_json: &str,
        enabled: bool,
    ) -> Result<(), WorkflowOperationError> {
        let config: serde_json::Value =
            serde_json::from_str(config_json).map_err(|error| DatabaseError::InvalidValue {
                field: "trigger configuration",
                value: error.to_string(),
            })?;
        let overlap_policy = OverlapPolicy::from_persisted(overlap_policy).ok_or_else(|| {
            DatabaseError::InvalidValue {
                field: "trigger overlap policy",
                value: overlap_policy.to_string(),
            }
        })?;
        self.triggers
            .configure(
                &TriggerRegistration {
                    id: id.into(),
                    definition_snapshot_id: definition_snapshot_id.into(),
                    schedule: TriggerSchedule::Manual,
                    overlap_policy,
                    admission_purpose: "workflow-launch".into(),
                    inputs: config
                        .get("inputs")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({})),
                    repository: config
                        .get("repository")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                    enabled,
                },
                0,
            )
            .await
            .map_err(Into::into)
    }

    pub async fn configure_trigger(
        &self,
        registration: &TriggerRegistration,
        now_unix_ms: i64,
    ) -> Result<(), WorkflowOperationError> {
        self.triggers
            .configure(registration, now_unix_ms)
            .await
            .map_err(Into::into)
    }

    pub async fn list_triggers(&self) -> Result<Vec<TriggerProjection>, WorkflowOperationError> {
        self.triggers
            .list()
            .await?
            .into_iter()
            .map(trigger_projection)
            .collect()
    }

    pub async fn show_trigger(
        &self,
        id: &str,
    ) -> Result<Option<TriggerProjection>, WorkflowOperationError> {
        self.triggers
            .show(id)
            .await?
            .map(trigger_projection)
            .transpose()
    }

    pub async fn set_trigger_enabled(
        &self,
        id: &str,
        enabled: bool,
        now_unix_ms: i64,
    ) -> Result<(), WorkflowOperationError> {
        self.triggers
            .set_enabled(id, enabled, now_unix_ms)
            .await
            .map_err(Into::into)
    }

    pub async fn run_trigger_now(
        &self,
        trigger_id: &str,
        occurrence_id: &str,
        now_unix_ms: i64,
    ) -> Result<bool, WorkflowOperationError> {
        self.triggers
            .run_now(trigger_id, occurrence_id, now_unix_ms)
            .await
            .map_err(Into::into)
    }

    pub async fn trigger_history(
        &self,
        trigger_id: &str,
        limit: usize,
    ) -> Result<Vec<TriggerHistoryProjection>, WorkflowOperationError> {
        Ok(self
            .triggers
            .history(trigger_id, limit)
            .await?
            .into_iter()
            .map(|row| {
                let status =
                    TriggerOccurrenceStatus::from_persisted(&row.status).ok_or_else(|| {
                        DatabaseError::InvalidValue {
                            field: "trigger occurrence status",
                            value: row.status,
                        }
                    })?;
                Ok::<_, DatabaseError>(TriggerHistoryProjection {
                    id: row.id,
                    trigger_id: row.trigger_id,
                    deduplication_key: row.deduplication_key,
                    due_unix_ms: row.due_unix_ms,
                    status,
                    run_id: row.run_id,
                    provider_item_id: row.provider_item_id,
                    observation_revision: row.observation_revision,
                    diagnostic: row.diagnostic,
                })
            })
            .collect::<Result<Vec<_>, _>>()?)
    }

    pub async fn materialize_due_triggers(
        &self,
        now_unix_ms: i64,
        limit: usize,
    ) -> Result<usize, WorkflowOperationError> {
        self.triggers
            .materialize_due(now_unix_ms, limit)
            .await
            .map_err(Into::into)
    }

    pub async fn record_startup_triggers(
        &self,
        worker_instance_id: &str,
        now_unix_ms: i64,
    ) -> Result<usize, WorkflowOperationError> {
        self.triggers
            .record_startup(worker_instance_id, now_unix_ms)
            .await
            .map_err(Into::into)
    }

    pub async fn record_provider_poll_page(
        &self,
        page: &ProviderPollPage,
    ) -> Result<usize, WorkflowOperationError> {
        self.triggers
            .record_provider_page(page)
            .await
            .map_err(Into::into)
    }

    pub async fn record_provider_poll_failure(
        &self,
        trigger_id: &str,
        safe_diagnostic: &str,
        now_unix_ms: i64,
        provider_retry_after_unix_ms: Option<i64>,
    ) -> Result<i64, WorkflowOperationError> {
        self.triggers
            .record_poll_failure(
                trigger_id,
                safe_diagnostic,
                now_unix_ms,
                provider_retry_after_unix_ms,
            )
            .await
            .map_err(Into::into)
    }

    pub async fn poll_provider_once(
        &self,
        trigger_id: &str,
        occurrence_id: &str,
        adapter: &dyn ProviderPollAdapter,
        max_items: usize,
        now_unix_ms: i64,
    ) -> Result<usize, WorkflowOperationError> {
        let trigger = self
            .show_trigger(trigger_id)
            .await?
            .ok_or(DatabaseError::Conflict {
                operation: "poll missing provider Trigger",
            })?;
        if !matches!(trigger.schedule, TriggerSchedule::ProviderPoll { .. }) {
            return Err(DatabaseError::Conflict {
                operation: "invoke provider adapter for non-provider Trigger",
            }
            .into());
        }
        match adapter
            .poll(ProviderPollRequest {
                checkpoint: trigger.checkpoint,
                max_items,
            })
            .await
        {
            Ok(batch) => {
                self.record_provider_poll_page(&ProviderPollPage {
                    trigger_id: trigger_id.into(),
                    occurrence_id: occurrence_id.into(),
                    items: batch.items,
                    checkpoint: batch.checkpoint,
                    observed_unix_ms: now_unix_ms,
                })
                .await
            }
            Err(error) => {
                let retry_after = match &error {
                    crate::workflow::trigger::ProviderPollError::Retryable {
                        retry_after_unix_ms,
                        ..
                    } => *retry_after_unix_ms,
                    crate::workflow::trigger::ProviderPollError::Unsupported { .. }
                    | crate::workflow::trigger::ProviderPollError::Failed { .. } => None,
                };
                self.record_provider_poll_failure(
                    trigger_id,
                    &error.to_string(),
                    now_unix_ms,
                    retry_after,
                )
                .await?;
                Err(DatabaseError::InvalidValue {
                    field: "provider poll",
                    value: error.to_string(),
                }
                .into())
            }
        }
    }

    pub async fn latest_provider_observation(
        &self,
        provider_item_id: &str,
    ) -> Result<Option<ProviderObservationProjection>, WorkflowOperationError> {
        self.triggers
            .latest_observation(provider_item_id)
            .await?
            .map(|row| {
                Ok::<_, DatabaseError>(ProviderObservationProjection {
                    provider_item_id: row.provider_item_id,
                    item_kind: ProviderItemKind::from_persisted(&row.item_kind).ok_or_else(
                        || DatabaseError::InvalidValue {
                            field: "provider item kind",
                            value: row.item_kind,
                        },
                    )?,
                    observation_revision: row.observation_revision,
                    observation: serde_json::from_str(&row.observation_json).map_err(|error| {
                        DatabaseError::InvalidValue {
                            field: "provider observation",
                            value: error.to_string(),
                        }
                    })?,
                    observed_unix_ms: row.observed_unix_ms,
                })
            })
            .transpose()
            .map_err(Into::into)
    }

    pub async fn decide_trigger_admission(
        &self,
        decision: &TriggerAdmissionDecision,
    ) -> Result<(), WorkflowOperationError> {
        self.triggers
            .decide_admission(decision)
            .await
            .map_err(Into::into)
    }

    pub async fn evaluate_deterministic_admission(
        &self,
        decision_id: &str,
        provider_item_id: &str,
        purpose: &str,
        policy: &crate::workflow::trigger::AdmissionPolicy,
        now_unix_ms: i64,
    ) -> Result<crate::workflow::trigger::AdmissionEvaluation, WorkflowOperationError> {
        let observation = self
            .latest_provider_observation(provider_item_id)
            .await?
            .ok_or(DatabaseError::Conflict {
                operation: "evaluate admission without a Provider Item observation",
            })?;
        let item: crate::workflow::trigger::ProviderItemObservation =
            serde_json::from_value(observation.observation.clone()).map_err(|error| {
                DatabaseError::InvalidValue {
                    field: "provider observation",
                    value: error.to_string(),
                }
            })?;
        let evaluation = policy.evaluate(&item);
        if let crate::workflow::trigger::AdmissionEvaluation::DeterministicallyAdmit { authority } =
            &evaluation
        {
            self.decide_trigger_admission(&TriggerAdmissionDecision {
                id: decision_id.into(),
                provider_item_id: provider_item_id.into(),
                observation_revision: observation.observation_revision,
                purpose: purpose.into(),
                outcome: crate::workflow::trigger::AdmissionOutcome::Admitted,
                authority: authority.clone(),
                evidence: serde_json::json!({"kind":"deterministic_policy","policy":policy}),
                decided_by: "deterministic-admission-policy".into(),
                decided_unix_ms: now_unix_ms,
            })
            .await?;
        }
        Ok(evaluation)
    }

    pub async fn launch_admitted_implementation(
        &self,
        command: LaunchAdmittedImplementation<'_>,
    ) -> Result<String, WorkflowOperationError> {
        let authority_json = self
            .triggers
            .admitted_authority(
                command.provider_item_id,
                command.observation_revision,
                command.purpose,
            )
            .await?
            .ok_or(DatabaseError::Conflict {
                operation: "launch implementation without admission",
            })?;
        if let Some(active) = self
            .triggers
            .active_dispatch(command.provider_item_id, command.purpose)
            .await?
        {
            return Ok(active);
        }
        let authority: Vec<String> =
            serde_json::from_str(&authority_json).map_err(|error| DatabaseError::InvalidValue {
                field: "admission authority",
                value: error.to_string(),
            })?;
        let idempotency_key = format!(
            "admitted:{}:{}:{}:{}",
            command.provider_item_id,
            command.observation_revision,
            command.definition_snapshot_id,
            command.purpose
        );
        let child_run_id = self
            .ledger
            .start(StartRun {
                run_id: command.child_run_id,
                definition_snapshot_id: command.definition_snapshot_id,
                repository: command.repository,
                idempotency_key: &idempotency_key,
                input_json: command.input_json,
                now_unix_ms: command.now_unix_ms,
                paused: true,
            })
            .await?;
        self.triggers
            .attach_input_provenance(
                &child_run_id,
                command.provider_item_id,
                command.observation_revision,
                command.purpose,
            )
            .await?;
        let selected = self
            .triggers
            .dispatch(RecordDispatch {
                item_id: command.provider_item_id,
                observation_revision: command.observation_revision,
                snapshot_id: command.definition_snapshot_id,
                purpose: command.purpose,
                intake_run_id: command.intake_run_id,
                child_run_id: &child_run_id,
                now_unix_ms: command.now_unix_ms,
            })
            .await?;
        if selected != child_run_id {
            self.ledger
                .command(
                    &child_run_id,
                    PersistedRunCommand::Cancel,
                    command.now_unix_ms,
                )
                .await?;
        }
        for (index, scope) in authority.iter().enumerate() {
            self.effects
                .grant_authority(
                    &format!("admission:{}:{index}", selected),
                    &selected,
                    scope,
                    "admission-decision",
                    command.now_unix_ms,
                    None,
                )
                .await?;
        }
        self.ledger
            .activate_paused(&selected, command.now_unix_ms)
            .await?;
        AsyncCoordinator::new(self.database.clone())
            .refresh_readiness(command.now_unix_ms)
            .await?;
        Ok(selected)
    }

    pub async fn trigger_doctor(
        &self,
        now_unix_ms: i64,
    ) -> Result<Vec<TriggerDoctorDiagnostic>, WorkflowOperationError> {
        let triggers = self.list_triggers().await?;
        let mut diagnostics = Vec::new();
        for trigger in triggers {
            if !trigger.enabled {
                diagnostics.push(TriggerDoctorDiagnostic {
                    trigger_id: trigger.id.clone(),
                    severity: "info".into(),
                    message: "Trigger is disabled".into(),
                });
            }
            if trigger.enabled
                && trigger
                    .checkpoint_unix_ms
                    .is_some_and(|updated| now_unix_ms.saturating_sub(updated) > 86_400_000)
            {
                diagnostics.push(TriggerDoctorDiagnostic {
                    trigger_id: trigger.id.clone(),
                    severity: "warning".into(),
                    message: "Trigger checkpoint has not advanced for more than 24 hours".into(),
                });
            }
            if let Some(retry_after) = trigger.retry_after_unix_ms {
                diagnostics.push(TriggerDoctorDiagnostic {
                    trigger_id: trigger.id,
                    severity: "warning".into(),
                    message: format!(
                        "Provider poll is backing off until {retry_after}: {}",
                        trigger
                            .poll_diagnostic
                            .unwrap_or_else(|| "provider request failed".into())
                    ),
                });
            }
        }
        Ok(diagnostics)
    }

    /// Read-only operational facts for retained run state. This deliberately reports recovery
    /// conditions without attempting repair, deletion, or reconciliation.
    pub async fn doctor_health(&self) -> Result<WorkflowHealthReport, WorkflowOperationError> {
        let health = self.ledger.health().await?;
        Ok(WorkflowHealthReport {
            orphaned_definition_snapshots: health.orphaned_definition_snapshots,
            quarantined_workspaces: health.quarantined_workspaces,
            indeterminate_effects: health.indeterminate_effects,
            recovery_required_runs: health.recovery_required_runs,
            invalid_child_links: health.invalid_child_links,
            artifact_integrity_failures: health
                .artifact_integrity_failures
                .into_iter()
                .map(|failure| ArtifactIntegrityFailure {
                    artifact_id: failure.artifact_id,
                    reason: failure.reason,
                })
                .collect(),
        })
    }

    pub async fn record_trigger_occurrence(
        &self,
        id: &str,
        trigger_id: &str,
        deduplication_key: &str,
        due_unix_ms: i64,
    ) -> Result<bool, WorkflowOperationError> {
        self.triggers
            .record_occurrence(id, trigger_id, deduplication_key, due_unix_ms)
            .await
            .map_err(Into::into)
    }

    pub async fn complete_trigger(
        &self,
        occurrence_id: &str,
        run_id: &str,
        checkpoint_json: &str,
        now_unix_ms: i64,
    ) -> Result<(), WorkflowOperationError> {
        self.wakeups
            .complete_trigger(occurrence_id, run_id, checkpoint_json, now_unix_ms)
            .await
            .map_err(Into::into)
    }

    pub async fn wait_on_gate(
        &self,
        step_id: &str,
        gate_kind: &str,
        due_unix_ms: i64,
        checkpoint_json: &str,
        now_unix_ms: i64,
    ) -> Result<(), WorkflowOperationError> {
        self.wakeups
            .wait_on_gate(
                step_id,
                gate_kind,
                due_unix_ms,
                checkpoint_json,
                now_unix_ms,
            )
            .await
            .map_err(Into::into)
    }

    pub async fn control_plane_metrics(
        &self,
    ) -> Result<Vec<ControlPlaneMetric>, WorkflowOperationError> {
        AsyncCoordinator::new(self.database.clone())
            .latest_metrics()
            .await
            .map(|metrics| {
                metrics
                    .into_iter()
                    .map(|metric| ControlPlaneMetric {
                        name: metric.name,
                        value: metric.value,
                        labels_json: metric.labels_json,
                        time_unix_ms: metric.time_unix_ms,
                    })
                    .collect()
            })
            .map_err(Into::into)
    }

    pub async fn list(
        &self,
        repository: Option<&str>,
        limit: usize,
    ) -> Result<Vec<WorkflowProjection>, WorkflowOperationError> {
        self.ledger
            .list(repository, limit)
            .await
            .map(|runs| runs.into_iter().map(workflow_projection).collect())
            .map_err(Into::into)
    }

    /// Returns a metadata-only page. Output and Artifact bodies are loaded only by `inspect`.
    pub async fn list_page(
        &self,
        repository: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<WorkflowProjection>, WorkflowOperationError> {
        self.ledger
            .list_page(repository, offset, limit)
            .await
            .map(|runs| runs.into_iter().map(workflow_projection).collect())
            .map_err(Into::into)
    }

    pub async fn inspect(
        &self,
        run_id: &str,
    ) -> Result<Option<WorkflowProjection>, WorkflowOperationError> {
        self.ledger
            .inspect(run_id)
            .await
            .map(|projection| projection.map(workflow_projection))
            .map_err(Into::into)
    }
}

fn trigger_projection(
    row: crate::persistence::triggers::TriggerRow,
) -> Result<TriggerProjection, WorkflowOperationError> {
    let schedule =
        serde_json::from_str(&row.schedule_json).map_err(|error| DatabaseError::InvalidValue {
            field: "trigger schedule",
            value: error.to_string(),
        })?;
    let config =
        serde_json::from_str(&row.config_json).map_err(|error| DatabaseError::InvalidValue {
            field: "trigger configuration",
            value: error.to_string(),
        })?;
    let checkpoint = row
        .checkpoint_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(|error| DatabaseError::InvalidValue {
            field: "trigger checkpoint",
            value: error.to_string(),
        })?;
    Ok(TriggerProjection {
        id: row.id,
        definition_snapshot_id: row.definition_snapshot_id,
        overlap_policy: OverlapPolicy::from_persisted(&row.overlap_policy).ok_or_else(|| {
            DatabaseError::InvalidValue {
                field: "trigger overlap policy",
                value: row.overlap_policy,
            }
        })?,
        schedule,
        config,
        admission_purpose: row.admission_purpose,
        enabled: row.enabled,
        checkpoint,
        checkpoint_unix_ms: row.checkpoint_unix_ms,
        consecutive_poll_failures: row.consecutive_failures,
        retry_after_unix_ms: row.retry_after_unix_ms,
        poll_diagnostic: row.poll_diagnostic,
    })
}

fn workflow_projection(run: crate::persistence::run_ledger::RunProjection) -> WorkflowProjection {
    WorkflowProjection {
        id: run.id,
        definition_name: run.definition_name,
        status: run.status,
        repository: run.repository,
        created_unix_ms: run.created_unix_ms,
        updated_unix_ms: run.updated_unix_ms,
        completed_unix_ms: run.completed_unix_ms,
        parent_run_id: run.parent_run_id,
        lineage_root_id: run.lineage_root_id,
        archived_unix_ms: run.archived_unix_ms,
        detached: run.detached,
        attempt_budget: run.attempt_budget,
        attempts_consumed: run.attempts_consumed,
        steps: run
            .steps
            .into_iter()
            .map(|step| WorkflowStepProjection {
                id: step.id,
                key: step.key,
                implementation: step.implementation,
                target_id: step.target_id,
                status: step.status,
                input_json: step.input_json,
                class: step.class,
                effect_boundary: step.effect_boundary,
                skippable: step.skippable,
                dependencies: step.dependencies,
            })
            .collect(),
        attempts: run
            .attempts
            .into_iter()
            .map(|attempt| WorkflowAttemptProjection {
                id: attempt.id,
                step_id: attempt.step_id,
                status: attempt.status,
                worker_id: attempt.worker_id,
                target_id: attempt.target_id,
                fencing_token: attempt.fencing_token,
                process_id: attempt.process_id,
                process_start_time_ticks: attempt.process_start_time_ticks,
                started_unix_ms: attempt.started_unix_ms,
                finished_unix_ms: attempt.finished_unix_ms,
                input_revisions_json: attempt.input_revisions_json,
                bindings: attempt
                    .bindings
                    .into_iter()
                    .map(|binding| WorkflowAttemptBindingProjection {
                        name: binding.name,
                        schema_id: binding.schema_id,
                        value_json: binding.value_json,
                        artifact_id: binding.artifact_id,
                    })
                    .collect(),
                output: attempt
                    .output
                    .into_iter()
                    .map(|output| WorkflowOutputProjection {
                        sequence: output.sequence,
                        stream: output.stream,
                        body: output.body,
                        time_unix_ms: output.time_unix_ms,
                    })
                    .collect(),
            })
            .collect(),
        artifacts: run
            .artifacts
            .into_iter()
            .map(|artifact| WorkflowArtifactProjection {
                id: artifact.id,
                producing_attempt_id: artifact.producing_attempt_id,
                revision: artifact.revision,
                digest: artifact.digest,
                size_bytes: artifact.size_bytes,
                sensitivity: artifact.sensitivity,
                inline_body: artifact.inline_body,
                file_path: artifact.file_path,
                created_unix_ms: artifact.created_unix_ms,
                provider_item_id: artifact.provider_item_id,
                observation_revision: artifact.observation_revision,
                trigger_occurrence_id: artifact.trigger_occurrence_id,
                admission_decision_id: artifact.admission_decision_id,
            })
            .collect(),
        approvals: run
            .approvals
            .into_iter()
            .map(|approval| WorkflowApprovalProjection {
                id: approval.id,
                step_id: approval.step_id,
                status: approval.status,
                requested_unix_ms: approval.requested_unix_ms,
                decided_unix_ms: approval.decided_unix_ms,
                decided_by: approval.decided_by,
                decision_note: approval.decision_note,
                subject_json: approval.subject_json,
                evidence_json: approval.evidence_json,
                policy_json: approval.policy_json,
            })
            .collect(),
        effects: run
            .effects
            .into_iter()
            .map(|effect| WorkflowEffectProjection {
                id: effect.id,
                attempt_id: effect.attempt_id,
                effect_kind: effect.effect_kind,
                idempotency_key: effect.idempotency_key,
                status: effect.status,
                request_json: effect.request_json,
                result_json: effect.result_json,
                created_unix_ms: effect.created_unix_ms,
                updated_unix_ms: effect.updated_unix_ms,
            })
            .collect(),
        gates: run
            .gates
            .into_iter()
            .map(|gate| WorkflowGateProjection {
                step_id: gate.step_id,
                gate_kind: gate.gate_kind,
                due_unix_ms: gate.due_unix_ms,
                checkpoint_json: gate.checkpoint_json,
                poll_count: gate.poll_count,
                subject_json: gate.subject_json,
                evidence_json: gate.evidence_json,
                policy_json: gate.policy_json,
            })
            .collect(),
        events: run
            .events
            .into_iter()
            .map(|event| WorkflowAuditEvent {
                sequence: event.sequence,
                step_id: event.step_id,
                attempt_id: event.attempt_id,
                kind: event.kind,
                time_unix_ms: event.time_unix_ms,
                data_json: event.data_json,
            })
            .collect(),
        children: run
            .children
            .into_iter()
            .map(|child| WorkflowChildProjection {
                step_id: child.step_id,
                iteration: child.iteration,
                run_id: child.run_id,
                status: child.status,
            })
            .collect(),
        authority: run
            .authority
            .into_iter()
            .map(|grant| WorkflowAuthorityProjection {
                scope: grant.scope,
                granted_by: grant.granted_by,
                granted_unix_ms: grant.granted_unix_ms,
                expires_unix_ms: grant.expires_unix_ms,
            })
            .collect(),
    }
}

fn start_command<'a>(command: &'a LaunchWorkflow<'a>) -> StartRun<'a> {
    StartRun {
        run_id: command.run_id,
        definition_snapshot_id: command.definition_snapshot_id,
        repository: command.repository,
        idempotency_key: command.idempotency_key,
        input_json: command.input_json,
        now_unix_ms: command.now_unix_ms,
        paused: false,
    }
}

#[derive(Debug)]
pub struct WorkflowOperationError(DatabaseError);

impl std::fmt::Display for WorkflowOperationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::error::Error for WorkflowOperationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

impl From<DatabaseError> for WorkflowOperationError {
    fn from(error: DatabaseError) -> Self {
        Self(error)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use tokio::sync::watch;

    use super::*;
    use crate::workflow::engine::{WorkerConfig, WorkflowWorker};
    use prism_extension_protocol::{ExtensionDescriptor, ImplementationDescriptor, StepClass};

    static SEQUENCE: AtomicU64 = AtomicU64::new(1);

    fn path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "prism-workflow-operations-{label}-{}-{}.db",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn doctor_health_reports_recovery_and_retained_artifact_corruption_read_only() {
        let path = path("doctor-health");
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let operations = WorkflowOperations::open(&path).await.unwrap();
                operations
                    .register_definition(DefinitionSnapshot {
                        id: "definition",
                        name: "health",
                        revision: "1",
                        source: "test",
                        trusted: true,
                        body_json: "{}",
                        digest: "sha256:definition",
                        now_unix_ms: 1,
                    })
                    .await
                    .unwrap();
                operations
                    .database
                    .write_immediate(|connection| {
                        Box::pin(async move {
                            sqlx::query("insert into workflow_run (id,definition_snapshot_id,status,created_unix_ms,updated_unix_ms,input_json,runtime_status) values ('run','definition','recovery_required',2,2,'{}','recovery_required')")
                                .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                            sqlx::query("insert into artifact (id,run_id,revision,digest,size_bytes,sensitivity,inline_body,created_unix_ms) values ('artifact','run',1,'sha256:wrong',4,'internal',x'626f6479',3)")
                                .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                            Ok(())
                        })
                    })
                    .await
                    .unwrap();

                let first = operations.doctor_health().await.unwrap();
                let second = operations.doctor_health().await.unwrap();
                assert_eq!(first, second);
                assert!(!first.healthy());
                assert_eq!(first.recovery_required_runs, 1);
                assert_eq!(first.artifact_integrity_failures.len(), 1);
                assert_eq!(first.artifact_integrity_failures[0].artifact_id, "artifact");
            });
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn launch_materializes_the_compiled_snapshot_dag() {
        let path = path("snapshot-runtime");
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                use prism_extension_protocol::{ArtifactSchemaDescriptor, PortDescriptor};
                let mut registry = crate::extension::registry::DescriptorRegistry::default();
                registry
                    .register(&ExtensionDescriptor {
                        artifact_schemas: vec![ArtifactSchemaDescriptor {
                            id: "acme.test/text".into(),
                            schema: serde_json::json!({"type":"string"}),
                        }],
                        implementations: vec![ImplementationDescriptor {
                            id: "acme.test/action".into(),
                            class: StepClass::Action,
                            inputs: vec![PortDescriptor {
                                name: "subject".into(),
                                schema: "acme.test/text".into(),
                                required: true,
                            }],
                            outputs: vec![],
                            capabilities: vec![],
                            targets: vec!["local".into()],
                            effect_boundary:
                                prism_extension_protocol::EffectBoundary::Unbrokered,
                        }],
                        ..ExtensionDescriptor::default()
                    })
                    .unwrap();
                let source = r#"schema_version=2
id='acme.test/runtime'
name='runtime'
launch=['manual']
[inputs.subject]
type='acme.test/text'
required=true
[[steps]]
id='first'
class='action'
use='acme.test/action'
skippable=false
[steps.inputs]
subject='inputs.subject'
[[steps]]
id='second'
class='action'
use='acme.test/action'
skippable=true
[steps.inputs]
subject='inputs.subject'
"#;
                let catalog = crate::workflow::definition::DefinitionCatalog::from_sources(
                    [("runtime.toml".into(), source.into())],
                    registry,
                )
                .unwrap();
                let snapshot = catalog.compile("acme.test/runtime").unwrap();
                assert_eq!(
                    snapshot.definition.steps[0].effect_boundary,
                    prism_extension_protocol::EffectBoundary::Unbrokered
                );
                let body = serde_json::to_string(&snapshot).unwrap();
                let operations = WorkflowOperations::open(&path).await.unwrap();
                operations
                    .register_definition(DefinitionSnapshot {
                        id: &snapshot.digest,
                        name: &snapshot.definition.name,
                        revision: "1",
                        source: "test",
                        trusted: true,
                        body_json: &body,
                        digest: &snapshot.digest,
                        now_unix_ms: 1,
                    })
                    .await
                    .unwrap();
                operations
                    .launch(LaunchWorkflow {
                        run_id: "run",
                        definition_snapshot_id: &snapshot.digest,
                        repository: None,
                        idempotency_key: "run",
                        input_json: r#"{"subject":"hello"}"#,
                        now_unix_ms: 2,
                    })
                    .await
                    .unwrap();
                let run = operations.inspect("run").await.unwrap().unwrap();
                assert_eq!(run.steps.len(), 2);
                assert_eq!(run.steps[0].key, "first");
                assert_eq!(run.steps[0].status, "runnable");
                assert_eq!(run.steps[0].effect_boundary, "unbrokered");
                let revisions: String = sqlx::query_scalar("select resolved_input_revisions_json from workflow_step where run_id='run' and step_key='first'")
                    .fetch_one(operations.database.readers()).await.unwrap();
                assert_eq!(serde_json::from_str::<serde_json::Value>(&revisions).unwrap()["subject"]["artifact_id"], "run:input:subject");
                assert_eq!(run.steps[1].key, "second");
                assert_eq!(run.steps[1].status, "waiting");
                operations.database.write_immediate(|connection| Box::pin(async move {
                    sqlx::query("update workflow_step set status='succeeded', runtime_status='succeeded' where run_id='run' and step_key='first'").execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                    let bindings = serde_json::to_string(&std::collections::BTreeMap::from([("missing".to_string(), crate::workflow::definition::Binding::Reference { reference: "steps.first.outputs.absent".into(), schema: "acme.test/missing".into() })])).unwrap();
                    sqlx::query("update workflow_step set bindings_json=? where run_id='run' and step_key='second'").bind(bindings).execute(connection).await.map_err(DatabaseError::Query)?;
                    Ok(())
                })).await.unwrap();
                crate::persistence::control_plane::AsyncCoordinator::new(operations.database.clone()).refresh_readiness(3).await.unwrap();
                assert_eq!(operations.inspect("run").await.unwrap().unwrap().steps[1].status, "waiting");
                operations.skip_step("run", "second", 3).await.unwrap();
            });
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn workflow_call_repeat_creates_distinct_child_iterations_and_binds_outputs() {
        let path = path("repeat-runtime");
        tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap().block_on(async {
            use prism_extension_protocol::{ArtifactSchemaDescriptor, PortDescriptor};
            let mut registry = crate::extension::registry::DescriptorRegistry::default();
            registry.register(&ExtensionDescriptor {
                artifact_schemas: vec![ArtifactSchemaDescriptor { id: "acme.test/ready".into(), schema: serde_json::json!({"type":"boolean"}) }],
                implementations: vec![ImplementationDescriptor { id: "acme.test/observe".into(), class: StepClass::Action, inputs: vec![], outputs: vec![PortDescriptor { name: "ready".into(), schema: "acme.test/ready".into(), required: true }], capabilities: vec![], targets: vec!["local".into()], effect_boundary: Default::default() }],
                ..ExtensionDescriptor::default()
            }).unwrap();
            let child = "schema_version=2\nid='acme.test/child'\nname='child'\nlaunch=['child']\n[outputs.ready]\ntype='acme.test/ready'\nfrom='steps.observe.outputs.ready'\n[[steps]]\nid='observe'\nclass='action'\nuse='acme.test/observe'\nskippable=false\n";
            let parent = "schema_version=2\nid='acme.test/parent'\nname='parent'\nlaunch=['manual']\n[[steps]]\nid='call'\nclass='workflow_call'\nworkflow='acme.test/child'\nskippable=false\n[steps.repeat]\nuntil='steps.call.outputs.ready == true'\nmax_iterations=3\non_exhausted='input_required'\n";
            let catalog = crate::workflow::definition::DefinitionCatalog::from_sources([("child.toml".into(), child.into()), ("parent.toml".into(), parent.into())], registry).unwrap();
            let snapshot = catalog.compile("acme.test/parent").unwrap();
            let body = serde_json::to_string(&snapshot).unwrap();
            let operations = WorkflowOperations::open(&path).await.unwrap();
            operations.register_definition(DefinitionSnapshot { id: &snapshot.digest, name: "parent", revision: "1", source: "test", trusted: true, body_json: &body, digest: &snapshot.digest, now_unix_ms: 1 }).await.unwrap();
            operations.launch(LaunchWorkflow { run_id: "parent", definition_snapshot_id: &snapshot.digest, repository: None, idempotency_key: "parent", input_json: "{}", now_unix_ms: 2 }).await.unwrap();
            crate::persistence::control_plane::AsyncCoordinator::new(operations.database.clone()).refresh_readiness(3).await.unwrap();
            operations.ledger.advance_children(4).await.unwrap();
            for (iteration, ready) in [(1, false), (2, true)] {
                let child_run = format!("parent:step:call:child:{iteration}");
                let child_step = format!("{child_run}:step:observe");
                let attempt = format!("{child_step}:fixture");
                operations.database.write_immediate(|connection| Box::pin(async move {
                    sqlx::query("update workflow_step set status='succeeded', runtime_status='succeeded' where id=?").bind(&child_step).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                    sqlx::query("insert into step_attempt (id,step_id,attempt_number,status,worker_id,target_id,fencing_token,lease_expires_unix_ms,started_unix_ms,finished_unix_ms,result_json) values (?, ?, 1, 'succeeded','fixture','local',1,99,5,6,?)").bind(&attempt).bind(&child_step).bind(serde_json::json!({"ready":ready}).to_string()).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                    let artifact_id = format!("{attempt}:output:ready");
                    let body = ready.to_string().into_bytes();
                    sqlx::query("insert into artifact (id,run_id,producing_attempt_id,revision,digest,size_bytes,sensitivity,inline_body,created_unix_ms) values (?,?,?,1,'fixture',?,'normal',?,6)")
                        .bind(&artifact_id).bind(&child_run).bind(&attempt).bind(i64::try_from(body.len()).unwrap()).bind(body).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                    sqlx::query("insert into attempt_output_binding (attempt_id,name,schema_id,value_json,artifact_id) values (?,'ready','acme.test/ready',?,?)").bind(&attempt).bind(ready.to_string()).bind(artifact_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                    sqlx::query("update workflow_run set status='succeeded', runtime_status='succeeded', completed_unix_ms=6 where id=?").bind(&child_run).execute(connection).await.map_err(DatabaseError::Query)?;
                    Ok(())
                })).await.unwrap();
                operations.ledger.advance_children(7 + iteration).await.unwrap();
            }
            let projection = operations.inspect("parent").await.unwrap().unwrap();
            assert_eq!(projection.status, "succeeded");
            assert_eq!(projection.children.len(), 2);
            let artifact_id: Option<String> = sqlx::query_scalar("select artifact_id from step_output_binding where step_id='parent:step:call' and name='ready'")
                .fetch_one(operations.database.readers()).await.unwrap();
            assert_eq!(artifact_id.as_deref(), Some("parent:step:call:output:ready"));
            let lineage: i64 = sqlx::query_scalar("select count(*) from artifact_lineage where artifact_id='parent:step:call:output:ready'")
                .fetch_one(operations.database.readers()).await.unwrap();
            assert_eq!(lineage, 1);
        });
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn approvals_and_commands_are_durable_domain_operations() {
        let path = path("commands");
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let operations = WorkflowOperations::open(&path).await.unwrap();
                operations
                    .register_definition(DefinitionSnapshot {
                        id: "definition",
                        name: "approval",
                        revision: "1",
                        source: "test",
                        trusted: true,
                        body_json: "{}",
                        digest: "digest",
                        now_unix_ms: 1,
                    })
                    .await
                    .unwrap();
                operations
                    .launch_definition(
                        LaunchWorkflow {
                            run_id: "run",
                            definition_snapshot_id: "definition",
                            repository: None,
                            idempotency_key: "run",
                            input_json: "{}",
                            now_unix_ms: 2,
                        },
                        vec![WorkflowStep {
                            id: "step".into(),
                            key: "approval".into(),
                            implementation: "approval".into(),
                            target_id: "local".into(),
                            input_json: "{}".into(),
                            dependencies: vec![],
                            resources: vec![],
                        }],
                    )
                    .await
                    .unwrap();
                let listed = operations.list(None, 8).await.unwrap();
                assert_eq!(listed.len(), 1);
                assert_eq!(listed[0].id, "run");
                assert!(listed[0].steps.is_empty());
                assert!(operations.list_page(None, 1, 8).await.unwrap().is_empty());
                assert!(operations.list(Some("/other"), 8).await.unwrap().is_empty());
                assert!(operations.list(None, 0).await.is_err());
                operations
                    .request_approval("approval", "run", "step", 3)
                    .await
                    .unwrap();
                assert_eq!(
                    operations.inspect("run").await.unwrap().unwrap().status,
                    "waiting"
                );
                operations
                    .decide_approval("approval", ApprovalDecision::Approve, "user", None, 4)
                    .await
                    .unwrap();
                operations
                    .command("run", WorkflowCommand::Pause, 5)
                    .await
                    .unwrap();
                operations
                    .command("run", WorkflowCommand::Resume, 6)
                    .await
                    .unwrap();
                operations
                    .command("run", WorkflowCommand::Cancel, 7)
                    .await
                    .unwrap();
                assert_eq!(
                    operations.inspect("run").await.unwrap().unwrap().status,
                    "cancelled"
                );
            });
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn quarantined_workspace_moves_dependent_work_to_recovery() {
        let path = path("workspace-quarantine");
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let operations = WorkflowOperations::open(&path).await.unwrap();
                operations.register_definition(DefinitionSnapshot {
                    id: "definition", name: "workspace", revision: "1", source: "test",
                    trusted: true, body_json: "{}", digest: "digest", now_unix_ms: 1,
                }).await.unwrap();
                operations.launch_definition(LaunchWorkflow {
                    run_id: "run", definition_snapshot_id: "definition", repository: Some("repo"),
                    idempotency_key: "run", input_json: "{}", now_unix_ms: 2,
                }, vec![WorkflowStep {
                    id: "step".into(), key: "work".into(), implementation: "command".into(),
                    target_id: "local".into(), input_json: "{}".into(), dependencies: vec![],
                    resources: vec!["workspace:ws".into()],
                }]).await.unwrap();
                operations.database.write_immediate(|connection| Box::pin(async move {
                    sqlx::query("insert into execution_workspace (id,run_id,repository,path,state,updated_unix_ms) values ('ws','run','repo','/tmp/ws','active',2)")
                        .execute(connection).await.map_err(DatabaseError::Query)?;
                    Ok(())
                })).await.unwrap();
                operations.quarantine_workspace("ws", "dirty recovery state", 3).await.unwrap();
                let run = operations.inspect("run").await.unwrap().unwrap();
                assert_eq!(run.status, "recovery_required");
                assert_eq!(run.steps[0].status, "recovery_required");
            });
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn due_trigger_launches_idempotent_materialized_run() {
        let path = path("trigger");
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let operations = WorkflowOperations::open(&path).await.unwrap();
                let mut registry = crate::extension::registry::DescriptorRegistry::default();
                registry.register(&ExtensionDescriptor {
                    implementations: vec![ImplementationDescriptor {
                        id: "acme.test/not-installed".into(), class: StepClass::Action,
                        inputs: vec![], outputs: vec![], capabilities: vec![], targets: vec!["local".into()], effect_boundary: Default::default(),
                    }], ..ExtensionDescriptor::default()
                }).unwrap();
                let catalog = crate::workflow::definition::DefinitionCatalog::from_sources(
                    [("trigger.toml".into(), "schema_version=2\nid='acme.test/triggered'\nname='triggered'\nlaunch=['trigger']\n[[steps]]\nid='work'\nclass='action'\nuse='acme.test/not-installed'\nskippable=false\n".into())], registry,
                ).unwrap();
                let snapshot = catalog.compile("acme.test/triggered").unwrap();
                let body = serde_json::to_string(&snapshot).unwrap();
                operations
                    .register_definition(DefinitionSnapshot {
                        id: "definition",
                        name: "triggered",
                        revision: "1",
                        source: "test",
                        trusted: true,
                        body_json: &body,
                        digest: "digest",
                        now_unix_ms: 1,
                    })
                    .await
                    .unwrap();
                operations
                    .register_trigger("trigger", "definition", "serialize", "{}", true)
                    .await
                    .unwrap();
                assert!(operations.record_trigger_occurrence("occurrence", "trigger", "once", 1).await.unwrap());
                assert!(!operations.record_trigger_occurrence("duplicate", "trigger", "once", 1).await.unwrap());

                let worker = WorkflowWorker::open(
                    &path,
                    "worker",
                    WorkerConfig {
                        scheduler_interval: Duration::from_millis(5),
                        ..WorkerConfig::default()
                    },
                )
                .await
                .unwrap();
                let (shutdown, receiver) = watch::channel(false);
                let task = tokio::spawn(worker.run(receiver));
                let mut launched = None;
                for _ in 0..100 {
                    launched = operations.inspect("trigger:occurrence").await.unwrap();
                    if launched.is_some() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                assert_eq!(launched.unwrap().status, "runnable");
                shutdown.send(true).unwrap();
                task.await.unwrap().unwrap();
            });
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn scheduled_occurrences_survive_restart_and_overlap_is_explicit() {
        let path = path("durable-schedule");
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let operations = WorkflowOperations::open(&path).await.unwrap();
                operations
                    .register_definition(DefinitionSnapshot {
                        id: "definition",
                        name: "scheduled",
                        revision: "1",
                        source: "test",
                        trusted: true,
                        body_json: "{}",
                        digest: "digest",
                        now_unix_ms: 1,
                    })
                    .await
                    .unwrap();
                operations
                    .configure_trigger(
                        &TriggerRegistration {
                            id: "nightly".into(),
                            definition_snapshot_id: "definition".into(),
                            schedule: crate::workflow::trigger::TriggerSchedule::Once {
                                at_unix_ms: 10,
                            },
                            overlap_policy: crate::workflow::trigger::OverlapPolicy::Queue,
                            admission_purpose: "nightly-release".into(),
                            inputs: serde_json::json!({}),
                            repository: None,
                            enabled: true,
                        },
                        1,
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    operations.materialize_due_triggers(20, 10).await.unwrap(),
                    1
                );
                // The persisted occurrence is the restart boundary: replaying due-time
                // calculation must hit the same uniqueness key and create no second record.
                let restarted = operations.clone();
                assert_eq!(restarted.materialize_due_triggers(20, 10).await.unwrap(), 0);
                let history = restarted.trigger_history("nightly", 10).await.unwrap();
                assert_eq!(history.len(), 1);
                assert!(
                    history[0]
                        .deduplication_key
                        .contains("definition:definition:purpose:nightly-release")
                );

                restarted
                    .configure_trigger(
                        &TriggerRegistration {
                            id: "cron".into(),
                            definition_snapshot_id: "definition".into(),
                            schedule: crate::workflow::trigger::TriggerSchedule::Cron {
                                expression: "* * * * *".into(),
                                timezone: "UTC".into(),
                            },
                            overlap_policy: crate::workflow::trigger::OverlapPolicy::Queue,
                            admission_purpose: "cron".into(),
                            inputs: serde_json::json!({}),
                            repository: None,
                            enabled: true,
                        },
                        0,
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    restarted
                        .materialize_due_triggers(120_001, 10)
                        .await
                        .unwrap(),
                    3,
                    "a fresh cron starts at its persisted creation time"
                );

                restarted
                    .configure_trigger(
                        &TriggerRegistration {
                            id: "coalescing".into(),
                            definition_snapshot_id: "definition".into(),
                            schedule: crate::workflow::trigger::TriggerSchedule::Manual,
                            overlap_policy: crate::workflow::trigger::OverlapPolicy::Coalesce,
                            admission_purpose: "triage".into(),
                            inputs: serde_json::json!({}),
                            repository: None,
                            enabled: true,
                        },
                        2,
                    )
                    .await
                    .unwrap();
                assert!(
                    restarted
                        .run_trigger_now("coalescing", "first", 3)
                        .await
                        .unwrap()
                );
                assert!(
                    restarted
                        .run_trigger_now("coalescing", "second", 4)
                        .await
                        .unwrap()
                );
                let history = restarted.trigger_history("coalescing", 10).await.unwrap();
                assert_eq!(
                    history
                        .iter()
                        .filter(|item| item.status == TriggerOccurrenceStatus::Pending)
                        .count(),
                    1
                );
                assert_eq!(
                    history
                        .iter()
                        .filter(|item| item.status == TriggerOccurrenceStatus::Coalesced)
                        .count(),
                    1
                );
            });
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn provider_intake_requires_current_admission_and_reuses_one_child() {
        let path = path("provider-admission");
        tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap().block_on(async {
            let mut registry = crate::extension::registry::DescriptorRegistry::default();
            registry.register(&ExtensionDescriptor {
                implementations: vec![ImplementationDescriptor {
                    id: "acme.test/work".into(), class: StepClass::Action,
                    inputs: vec![], outputs: vec![], capabilities: vec![],
                    targets: vec!["local".into()], effect_boundary: Default::default(),
                }], ..ExtensionDescriptor::default()
            }).unwrap();
            let catalog = crate::workflow::definition::DefinitionCatalog::from_sources(
                [("work.toml".into(), "schema_version=2\nid='acme.test/work'\nname='work'\nlaunch=['manual','child','trigger']\n[[steps]]\nid='work'\nclass='action'\nuse='acme.test/work'\nskippable=false\n".into())],
                registry,
            ).unwrap();
            let snapshot = catalog.compile("acme.test/work").unwrap();
            let body = serde_json::to_string(&snapshot).unwrap();
            let operations = WorkflowOperations::open(&path).await.unwrap();
            operations.register_definition(DefinitionSnapshot {
                id: "definition", name: "work", revision: "1", source: "test",
                trusted: true, body_json: &body, digest: &snapshot.digest, now_unix_ms: 1,
            }).await.unwrap();
            operations.configure_trigger(&TriggerRegistration {
                id: "github-issues".into(), definition_snapshot_id: "definition".into(),
                schedule: crate::workflow::trigger::TriggerSchedule::ProviderPoll {
                    anchor_unix_ms: 0,
                    every_ms: 60_000,
                    item_kind: crate::workflow::trigger::ProviderItemKind::Issue,
                },
                overlap_policy: crate::workflow::trigger::OverlapPolicy::Queue,
                admission_purpose: "issue-implementation".into(), inputs: serde_json::json!({}),
                repository: None, enabled: true,
            }, 1).await.unwrap();
            operations.run_trigger_now("github-issues", "poll-1", 2).await.unwrap();
            assert_eq!(operations.record_provider_poll_failure("github-issues", "rate limited", 2, Some(50)).await.unwrap(), 50);
            assert_eq!(operations.show_trigger("github-issues").await.unwrap().unwrap().consecutive_poll_failures, Some(1));
            let item = crate::workflow::trigger::ProviderItemObservation {
                provider_item_id: "github:github.com:acme/prism:issue:77".into(),
                kind: crate::workflow::trigger::ProviderItemKind::Issue,
                title: "Fix it".into(), body: "Untrusted instructions".into(), lifecycle: "open".into(),
                author: "alice".into(), author_relationship: Some("member".into()),
                labels: Default::default(), assignees: Vec::new(), updated_at: Some("r1".into()),
            };
            let revision_1 = item.revision();
            operations.record_provider_poll_page(&ProviderPollPage {
                trigger_id: "github-issues".into(), occurrence_id: "poll-1".into(),
                items: vec![item.clone()], checkpoint: serde_json::json!({"cursor":"page-1"}),
                observed_unix_ms: 3,
            }).await.unwrap();
            assert_eq!(operations.show_trigger("github-issues").await.unwrap().unwrap().consecutive_poll_failures, None);
            let policy = crate::workflow::trigger::AdmissionPolicy {
                trusted_author_relationships: std::collections::BTreeSet::from(["member".into()]),
                required_labels: Default::default(),
                authority: std::collections::BTreeSet::from(["workspace:issue-77".into()]),
            };
            assert!(matches!(operations.evaluate_deterministic_admission(
                "decision-1", &item.provider_item_id, "issue-implementation", &policy, 4,
            ).await.unwrap(), crate::workflow::trigger::AdmissionEvaluation::DeterministicallyAdmit { .. }));

            let mut changed = item.clone();
            changed.updated_at = Some("r2".into());
            let revision_2 = changed.revision();
            operations.record_provider_poll_page(&ProviderPollPage {
                trigger_id: "github-issues".into(), occurrence_id: "poll-1".into(),
                items: vec![changed], checkpoint: serde_json::json!({"cursor":"page-2"}),
                observed_unix_ms: 5,
            }).await.unwrap();
            let stale = LaunchAdmittedImplementation {
                provider_item_id: &item.provider_item_id, observation_revision: &revision_1,
                purpose: "issue-implementation", intake_run_id: "intake",
                child_run_id: "implementation-stale", definition_snapshot_id: "definition",
                repository: Some("/repo"), input_json: "{}", now_unix_ms: 6,
            };
            assert!(operations.launch_admitted_implementation(stale).await.is_err());

            operations.launch(LaunchWorkflow {
                run_id: "intake", definition_snapshot_id: "definition", repository: None,
                idempotency_key: "intake", input_json: "{}", now_unix_ms: 6,
            }).await.unwrap();
            operations.decide_trigger_admission(&TriggerAdmissionDecision {
                id: "decision-2".into(), provider_item_id: item.provider_item_id.clone(),
                observation_revision: revision_2.clone(), purpose: "issue-implementation".into(),
                outcome: crate::workflow::trigger::AdmissionOutcome::Admitted,
                authority: std::collections::BTreeSet::from(["workspace:issue-77".into()]),
                evidence: serde_json::json!({"human":true}), decided_by: "operator".into(),
                decided_unix_ms: 7,
            }).await.unwrap();
            let command = |child_run_id| LaunchAdmittedImplementation {
                provider_item_id: &item.provider_item_id, observation_revision: &revision_2,
                purpose: "issue-implementation", intake_run_id: "intake", child_run_id,
                definition_snapshot_id: "definition", repository: Some("/repo"),
                input_json: "{}", now_unix_ms: 8,
            };
            assert_eq!(operations.launch_admitted_implementation(command("implementation-1")).await.unwrap(), "implementation-1");
            assert_eq!(operations.launch_admitted_implementation(command("implementation-2")).await.unwrap(), "implementation-1");
            let mut changed_again = item.clone();
            changed_again.updated_at = Some("r3".into());
            let revision_3 = changed_again.revision();
            operations.record_provider_poll_page(&ProviderPollPage {
                trigger_id: "github-issues".into(), occurrence_id: "poll-1".into(),
                items: vec![changed_again], checkpoint: serde_json::json!({"cursor":"page-3"}),
                observed_unix_ms: 9,
            }).await.unwrap();
            let unadmitted_current = LaunchAdmittedImplementation {
                provider_item_id: &item.provider_item_id, observation_revision: &revision_3,
                purpose: "issue-implementation", intake_run_id: "intake",
                child_run_id: "implementation-3", definition_snapshot_id: "definition",
                repository: Some("/repo"), input_json: "{}", now_unix_ms: 10,
            };
            assert!(operations.launch_admitted_implementation(unadmitted_current).await.is_err());
            let child = operations.inspect("implementation-1").await.unwrap().unwrap();
            assert_eq!(child.authority.len(), 1);
            assert!(child.artifacts.iter().any(|artifact| {
                artifact.provider_item_id.as_deref() == Some(item.provider_item_id.as_str())
                    && artifact.observation_revision.as_deref() == Some(revision_2.as_str())
                    && artifact.admission_decision_id.as_deref() == Some("decision-2")
            }));
            assert!(operations.inspect("implementation-2").await.unwrap().is_none());
            let trigger = operations.show_trigger("github-issues").await.unwrap().unwrap();
            assert_eq!(trigger.checkpoint, Some(serde_json::json!({"cursor":"page-3"})));
        });
        let _ = std::fs::remove_file(path);
    }
}
