use std::path::Path;

use crate::persistence::approvals::{ApprovalDecision as PersistedApprovalDecision, ApprovalStore};
use crate::persistence::control_plane::AsyncCoordinator;
use crate::persistence::effects::EffectBroker;
use crate::persistence::error::DatabaseError;
use crate::persistence::import::import_legacy_repository;
use crate::persistence::pools::WorkflowDatabase;
use crate::persistence::run_ledger::{
    MaterializedStep, RegisterDefinition, RunCommand as PersistedRunCommand, RunLedger, StartRun,
};
use crate::persistence::wakeups::WakeupStore;

/// Deep async interface for generalized workflow commands and projections.
/// SQL, pools, transactions, idempotency, and row conversion remain private.
#[derive(Clone)]
pub struct WorkflowOperations {
    database: WorkflowDatabase,
    ledger: RunLedger,
    approvals: ApprovalStore,
    effects: EffectBroker,
    wakeups: WakeupStore,
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
pub enum ApprovalDecision {
    Approve,
    Reject,
}

pub struct LaunchWorkflow<'a> {
    pub run_id: &'a str,
    pub definition_snapshot_id: &'a str,
    pub repository: Option<&'a str>,
    pub idempotency_key: &'a str,
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
    pub steps: Vec<WorkflowStepProjection>,
    pub attempts: Vec<WorkflowAttemptProjection>,
    pub artifacts: Vec<WorkflowArtifactProjection>,
    pub approvals: Vec<WorkflowApprovalProjection>,
    pub effects: Vec<WorkflowEffectProjection>,
    pub gates: Vec<WorkflowGateProjection>,
    pub events: Vec<WorkflowAuditEvent>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct WorkflowStepProjection {
    pub id: String,
    pub key: String,
    pub implementation: String,
    pub target_id: String,
    pub status: String,
    pub input_json: String,
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
    pub output: Vec<WorkflowOutputProjection>,
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
pub struct LegacyImportSummary {
    pub imported: usize,
    pub already_imported: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ControlPlaneMetric {
    pub name: String,
    pub value: i64,
    pub labels_json: String,
    pub time_unix_ms: i64,
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
        self.ledger
            .start(start_command(&command))
            .await
            .map_err(Into::into)
    }

    pub async fn launch_materialized(
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
        self.wakeups
            .register_trigger(
                id,
                definition_snapshot_id,
                overlap_policy,
                config_json,
                enabled,
            )
            .await
            .map_err(Into::into)
    }

    pub async fn record_trigger_occurrence(
        &self,
        id: &str,
        trigger_id: &str,
        deduplication_key: &str,
        due_unix_ms: i64,
    ) -> Result<bool, WorkflowOperationError> {
        self.wakeups
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

    pub async fn import_legacy_repository(
        &self,
        source_path: &Path,
        importer_revision: &str,
        now_unix_ms: i64,
    ) -> Result<LegacyImportSummary, WorkflowOperationError> {
        // Adopt and validate the repository database before opening the importer's read-only
        // snapshot. This preserves released pre-SQLx databases instead of rejecting them merely
        // because they do not yet carry SQLx's migration journal.
        let source = crate::persistence::pools::RepositoryDatabase::open(source_path).await?;
        source.close().await;
        import_legacy_repository(&self.database, source_path, importer_revision, now_unix_ms)
            .await
            .map(|summary| LegacyImportSummary {
                imported: summary.imported,
                already_imported: summary.already_imported,
            })
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

fn workflow_projection(run: crate::persistence::run_ledger::RunProjection) -> WorkflowProjection {
    WorkflowProjection {
        id: run.id,
        definition_name: run.definition_name,
        status: run.status,
        repository: run.repository,
        created_unix_ms: run.created_unix_ms,
        updated_unix_ms: run.updated_unix_ms,
        completed_unix_ms: run.completed_unix_ms,
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
    }
}

fn start_command<'a>(command: &'a LaunchWorkflow<'a>) -> StartRun<'a> {
    StartRun {
        run_id: command.run_id,
        definition_snapshot_id: command.definition_snapshot_id,
        repository: command.repository,
        idempotency_key: command.idempotency_key,
        now_unix_ms: command.now_unix_ms,
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

    static SEQUENCE: AtomicU64 = AtomicU64::new(1);

    fn path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "prism-workflow-operations-{label}-{}-{}.db",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
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
                    .launch_materialized(
                        LaunchWorkflow {
                            run_id: "run",
                            definition_snapshot_id: "definition",
                            repository: None,
                            idempotency_key: "run",
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
    fn legacy_import_is_resumable_and_preserves_provenance() {
        let source_path = path("legacy-source");
        let workflow_path = path("legacy-target");
        std::fs::copy("tests/fixtures/database/repository-v1.db", &source_path).unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let source = crate::persistence::pools::RepositoryDatabase::open(&source_path)
                    .await
                    .unwrap();
                source
                    .write_immediate(|connection| {
                        Box::pin(async move {
                            sqlx::query("insert into plan_run (id, repo_root, scope_path, plan_path, plan_display, step_name, start_step, total_steps, mode, status, selected_step, created_unix_ms, updated_unix_ms) values ('plan-1', '/repo', '/repo', '/repo/plan.md', 'plan.md', 'phase', 1, 1, 'execute', 'completed', 1, 10, 20)")
                                .execute(&mut *connection)
                                .await
                                .map_err(DatabaseError::Query)?;
                            sqlx::query("insert into plan_step_run (run_id, step, prompt, status, started_unix_ms, finished_unix_ms, summary) values ('plan-1', 1, 'implement phase one', 'completed', 11, 19, 'done')")
                                .execute(connection)
                                .await
                                .map_err(DatabaseError::Query)?;
                            Ok(())
                        })
                    })
                    .await
                    .unwrap();
                source.close().await;
                assert!(source_path.with_extension("db.pre-sqlx-backup").exists());

                let operations = WorkflowOperations::open(&workflow_path).await.unwrap();
                assert_eq!(
                    operations
                        .import_legacy_repository(&source_path, "test-importer", 30)
                        .await
                        .unwrap(),
                    LegacyImportSummary {
                        imported: 1,
                        already_imported: 0
                    }
                );
                assert_eq!(
                    operations
                        .import_legacy_repository(&source_path, "test-importer", 31)
                        .await
                        .unwrap(),
                    LegacyImportSummary {
                        imported: 0,
                        already_imported: 1
                    }
                );
                let source_key = format!(
                    "{:016x}",
                    crate::util::stable_hash(&std::fs::canonicalize(&source_path).unwrap())
                );
                let imported = operations
                    .inspect(&format!("legacy:{source_key}:plan:plan-1"))
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(imported.status, "succeeded");
                assert_eq!(imported.repository.as_deref(), Some("/repo"));
                assert_eq!(imported.steps.len(), 1);
                assert_eq!(imported.steps[0].key, "phase-1");
                assert_eq!(imported.steps[0].status, "succeeded");
                assert_eq!(imported.steps[0].implementation, "legacy-history");
            });
        let _ = std::fs::remove_file(source_path.with_extension("db.pre-sqlx-backup"));
        let _ = std::fs::remove_file(source_path);
        let _ = std::fs::remove_file(workflow_path);
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
                operations
                    .register_definition(DefinitionSnapshot {
                        id: "definition",
                        name: "triggered",
                        revision: "1",
                        source: "test",
                        trusted: true,
                        body_json: r#"{"steps":[{"key":"work","implementation":"not-installed","input":{"value":1}}]}"#,
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
}
