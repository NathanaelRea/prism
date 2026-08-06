#![allow(dead_code)] // The global ledger is exercised behind tests before production cutover.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::definition::{Capability, DefinitionSnapshot, PrimitiveClass};
use crate::storage;
use crate::util::prism_config_dir;

const SCHEMA_VERSION: u32 = 3;
const MAX_INLINE_ARTIFACT_BYTES: usize = 64 * 1024;
const MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub(crate) struct $name(pub(crate) String);

        impl $name {
            pub(crate) fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}

id_type!(RepositoryId);
id_type!(RunId);
id_type!(StepId);
id_type!(AttemptId);
id_type!(ArtifactId);
id_type!(AuthorityGrantId);
id_type!(ApprovalRequestId);
id_type!(ApprovalDecisionId);
id_type!(ExecutionWorkspaceId);
id_type!(EffectIntentId);

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RunState {
    Queued,
    Active,
    Waiting,
    InputRequired,
    Paused,
    Cancelling,
    Completed,
    Failed,
    Cancelled,
    RecoveryRequired,
}

impl RunState {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Active => "active",
            Self::Waiting => "waiting",
            Self::InputRequired => "input_required",
            Self::Paused => "paused",
            Self::Cancelling => "cancelling",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::RecoveryRequired => "recovery_required",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StepState {
    Pending,
    Runnable,
    Active,
    Waiting,
    Blocked,
    InputRequired,
    Skipped,
    Completed,
    Failed,
    Cancelled,
    RecoveryRequired,
}

impl StepState {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Runnable => "runnable",
            Self::Active => "active",
            Self::Waiting => "waiting",
            Self::Blocked => "blocked",
            Self::InputRequired => "input_required",
            Self::Skipped => "skipped",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::RecoveryRequired => "recovery_required",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AttemptState {
    Prepared,
    Leased,
    Executing,
    Waiting,
    Completed,
    Failed,
    Cancelled,
    RecoveryRequired,
}

impl AttemptState {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Leased => "leased",
            Self::Executing => "executing",
            Self::Waiting => "waiting",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::RecoveryRequired => "recovery_required",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TrustClass {
    Trusted,
    Untrusted,
    DerivedUntrusted,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Sensitivity {
    Public,
    Internal,
    Sensitive,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct ArtifactInput {
    pub name: String,
    pub artifact_type: String,
    pub payload: serde_json::Value,
    pub trust: TrustClass,
    pub sensitivity: Sensitivity,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ArtifactRef {
    pub id: ArtifactId,
    pub revision: u32,
    pub digest: String,
    pub artifact_type: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct AuthorityGrant {
    pub id: AuthorityGrantId,
    pub capabilities: BTreeSet<Capability>,
    pub secret_handles: BTreeSet<String>,
    pub target_scope: BTreeSet<String>,
    pub expires_unix_ms: Option<i64>,
}

#[derive(Clone, Debug)]
pub(crate) struct StartRun {
    pub snapshot: DefinitionSnapshot,
    pub repository_id: Option<RepositoryId>,
    pub inputs: Vec<ArtifactInput>,
    pub idempotency_key: Option<String>,
    pub actor: String,
    pub actor_capabilities: BTreeSet<Capability>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct StartRunResult {
    pub run_id: RunId,
    pub created: bool,
    pub input_digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct RunSummary {
    pub id: RunId,
    pub definition: String,
    pub snapshot_digest: String,
    pub state: RunState,
    pub control: String,
    pub revision: i64,
    pub created_unix_ms: i64,
    pub updated_unix_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct StepSummary {
    pub id: StepId,
    pub definition_step_id: String,
    pub class: PrimitiveClass,
    pub implementation: String,
    pub state: StepState,
    pub attempt_count: u32,
    pub blocker: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct AttentionSummary {
    pub runs: Vec<RunSummary>,
    pub pending_approvals: u64,
    pub recovery_required_attempts: u64,
    pub quarantined_workspaces: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct RunProjection {
    pub schema_version: u32,
    pub run: RunSummary,
    pub steps: Vec<StepSummary>,
    pub determining_steps: Vec<StepId>,
    pub attempts: Vec<AttemptSummary>,
    pub artifacts: Vec<ArtifactSummary>,
    pub approvals: Vec<ApprovalProjection>,
    pub gates: Vec<GateResultSummary>,
    pub effects: Vec<EffectIntentSummary>,
    pub authority: AuthorityGrant,
    pub output: Vec<AttemptOutputSummary>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct AttemptSummary {
    pub id: AttemptId,
    pub step_id: StepId,
    pub ordinal: u32,
    pub state: String,
    pub target_id: Option<String>,
    pub workspace_id: Option<ExecutionWorkspaceId>,
    pub terminal_reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ArtifactLineageRef {
    pub id: ArtifactId,
    pub revision: u32,
    pub consumer_port: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ArtifactSummary {
    pub artifact: ArtifactRef,
    pub producer_attempt_id: Option<AttemptId>,
    pub port: String,
    pub trust: String,
    pub sensitivity: String,
    pub size: u64,
    pub sources: Vec<ArtifactLineageRef>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ApprovalProjection {
    pub request: ApprovalRequest,
    pub decision: Option<ApprovalDecision>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct GateResultSummary {
    pub id: String,
    pub step_id: StepId,
    pub attempt_id: AttemptId,
    pub subject_digest: String,
    pub subject_generation: String,
    pub evidence_quality: String,
    pub policy_revision: String,
    pub status: String,
    pub reason: String,
    pub expires_unix_ms: Option<i64>,
    pub created_unix_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct EffectIntentSummary {
    pub id: EffectIntentId,
    pub step_id: StepId,
    pub attempt_id: AttemptId,
    pub kind: String,
    pub state: String,
    pub exact_head: Option<String>,
    pub reconciliation_key: String,
    pub dispatch_generation: u32,
    pub result_json: Option<String>,
    pub created_unix_ms: i64,
    pub updated_unix_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct AttemptOutputSummary {
    pub attempt_id: AttemptId,
    pub sequence: u32,
    pub stream: String,
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct WorkflowHealth {
    pub integrity_ok: bool,
    pub problems: Vec<String>,
    pub active_leases: u64,
    pub dangling_claims: u64,
    pub quarantined_workspaces: u64,
    pub overdue_waits: u64,
    pub recovery_required_attempts: u64,
    pub unresolved_effects: u64,
    pub enabled_triggers: u64,
    pub orphaned_blobs: u64,
    pub target_descriptors: Vec<crate::target::ExecutionTargetDescriptor>,
}

/// Conservative defaults prune only replaceable diagnostic payloads. Durable
/// identity, snapshots, Artifacts, lineage, decisions, effects, and imported
/// history are never removed by retention.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RetentionPolicy {
    pub noisy_event_age_ms: i64,
    pub attempt_output_age_ms: i64,
    pub notification_age_ms: i64,
    pub provider_observation_age_ms: i64,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        const DAY: i64 = 24 * 60 * 60 * 1000;
        Self {
            noisy_event_age_ms: 30 * DAY,
            attempt_output_age_ms: 30 * DAY,
            notification_age_ms: 7 * DAY,
            provider_observation_age_ms: 30 * DAY,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct RetentionReport {
    pub events_deleted: u64,
    pub output_rows_deleted: u64,
    pub notifications_deleted: u64,
    pub provider_observations_deleted: u64,
    pub blobs_deleted: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct RunEvent {
    pub id: i64,
    pub run_id: RunId,
    pub step_id: Option<StepId>,
    pub kind: String,
    pub data_json: String,
    pub created_unix_ms: i64,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ApprovalMode {
    ArtifactAcceptance,
    CapabilityAuthorization,
    HumanAttestation,
    ExactMutation,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ApprovalRequest {
    pub id: ApprovalRequestId,
    pub run_id: RunId,
    pub step_id: StepId,
    pub attempt_id: AttemptId,
    pub mode: ApprovalMode,
    pub request_digest: String,
    pub input_digest: String,
    pub evidence_digest: String,
    pub expires_unix_ms: Option<i64>,
    pub state: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ApprovalDecision {
    pub id: ApprovalDecisionId,
    pub request_id: ApprovalRequestId,
    pub approved: bool,
    pub actor: String,
    pub reason: Option<String>,
    pub decided_unix_ms: i64,
}

#[derive(Clone, Debug)]
pub(crate) struct RunLedger {
    path: PathBuf,
    blob_dir: PathBuf,
}

impl RunLedger {
    pub(crate) fn user() -> Result<Self, String> {
        Self::open(prism_config_dir().join("workflow.db"))
    }

    pub(crate) fn open(path: PathBuf) -> Result<Self, String> {
        secure_parent(
            path.parent()
                .ok_or_else(|| "workflow database has no parent".to_string())?,
        )?;
        let blob_dir = if path.file_name().is_some_and(|name| name == "workflow.db") {
            path.with_file_name("workflow-blobs")
        } else {
            path.with_extension("blobs")
        };
        let ledger = Self { blob_dir, path };
        secure_parent(&ledger.blob_dir)?;
        let conn = ledger.connection()?;
        drop(conn);
        secure_file(&ledger.path)?;
        Ok(ledger)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn connection(&self) -> Result<Connection, String> {
        let _initialization_lock = InitializationLock::acquire(&self.path)?;
        let conn =
            storage::open_writable_connection(&self.path).map_err(|error| error.to_string())?;
        migrate(&conn, &self.path)?;
        Ok(conn)
    }

    pub(crate) fn tracked_repository_paths(&self) -> Result<Vec<PathBuf>, String> {
        let conn = self.connection()?;
        let mut statement = conn.prepare("select tracked_path from repository_identity where tracked_path is not null order by tracked_path").map_err(sql_error)?;
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)
            .map(|paths| paths.into_iter().map(PathBuf::from).collect())
    }

    pub(crate) fn repository_id(&self, path: &Path) -> Result<RepositoryId, String> {
        let canonical = path
            .canonicalize()
            .map_err(|error| format!("resolve repository path {}: {error}", path.display()))?;
        let tracked_path = canonical.to_string_lossy().into_owned();
        let conn = self.connection()?;
        if let Some(id) = conn
            .query_row(
                "select id from repository_identity where tracked_path=?1",
                [&tracked_path],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(sql_error)?
        {
            return Ok(RepositoryId(id));
        }
        let id = RepositoryId(random_id(&conn)?);
        conn.execute(
            "insert into repository_identity(id,tracked_path,created_unix_ms) values(?1,?2,?3)",
            params![id.as_str(), tracked_path, now_ms()],
        )
        .map_err(sql_error)?;
        Ok(id)
    }

    pub(crate) fn ensure_local_workspace(
        &self,
        run_id: &RunId,
    ) -> Result<(ExecutionWorkspaceId, PathBuf), String> {
        let mut conn = self.connection()?;
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        if let Some((id, path)) = transaction
            .query_row(
                "select workspace.id,repository.tracked_path from run_worktree_link link join execution_workspace workspace on workspace.id=link.workspace_id join repository_identity repository on repository.id=workspace.repository_id where link.run_id=?1 and workspace.target_id='local' and link.retired_unix_ms is null",
                [run_id.as_str()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(sql_error)?
        {
            transaction.commit().map_err(sql_error)?;
            return Ok((ExecutionWorkspaceId(id), PathBuf::from(path)));
        }
        let (repository_id, tracked_path): (String, String) = transaction
            .query_row(
                "select repository.id,repository.tracked_path from workflow_run run join repository_identity repository on repository.id=run.repository_id where run.id=?1",
                [run_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|error| format!("Run has no tracked local repository: {error}"))?;
        let path = PathBuf::from(&tracked_path);
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&path)
            .args(["rev-parse", "HEAD"])
            .output()
            .map_err(|error| format!("observe workspace base revision: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "observe workspace base revision: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let base_revision = String::from_utf8(output.stdout)
            .map_err(|_| "workspace base revision is not UTF-8".to_string())?
            .trim()
            .to_string();
        let workspace_id = ExecutionWorkspaceId(random_id(&transaction)?);
        let now = now_ms();
        transaction.execute(
            "insert into execution_workspace(id,repository_id,target_id,base_revision,generation,state,updated_unix_ms) values(?1,?2,'local',?3,1,'available',?4)",
            params![workspace_id.as_str(), repository_id, base_revision, now],
        ).map_err(sql_error)?;
        transaction
            .execute(
                "insert into run_worktree_link(run_id,workspace_id,incarnation) values(?1,?2,1)",
                params![run_id.as_str(), workspace_id.as_str()],
            )
            .map_err(sql_error)?;
        transaction.commit().map_err(sql_error)?;
        Ok((workspace_id, path))
    }

    pub(crate) fn local_workspace_path(
        &self,
        workspace_id: &ExecutionWorkspaceId,
    ) -> Result<PathBuf, String> {
        self.connection()?
            .query_row(
                "select repository.tracked_path from execution_workspace workspace join repository_identity repository on repository.id=workspace.repository_id where workspace.id=?1 and workspace.target_id='local'",
                [workspace_id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(sql_error)?
            .map(PathBuf::from)
            .ok_or_else(|| format!("local Execution Workspace '{}' was not found", workspace_id.as_str()))
    }

    pub(crate) fn has_nonterminal_runs(&self) -> Result<bool, String> {
        self.connection()?
            .query_row(
                "select exists(select 1 from workflow_run where state not in ('completed','failed','cancelled'))",
                [],
                |row| row.get(0),
            )
            .map_err(sql_error)
    }

    pub(crate) fn cutover_complete(&self) -> Result<bool, String> {
        self.connection()?
            .query_row(
                "select exists(select 1 from workflow_cutover where id=1)",
                [],
                |row| row.get(0),
            )
            .map_err(sql_error)
    }

    pub(crate) fn complete_cutover(&self) -> Result<(), String> {
        let conn = self.connection()?;
        let incomplete: bool = conn
            .query_row(
                "select exists(select 1 from legacy_migration_journal where state!='complete')",
                [],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        if incomplete {
            return Err(
                "legacy history import is incomplete; production cutover was not recorded"
                    .to_string(),
            );
        }
        conn.execute(
            "insert or replace into workflow_cutover(id,completed_unix_ms,imported_sources,imported_runs) values(1,?1,(select count(*) from legacy_migration_journal where state='complete'),(select count(*) from legacy_run_import))",
            [now_ms()],
        )
        .map_err(sql_error)?;
        Ok(())
    }

    pub(crate) fn trust_definition(&self, snapshot: &DefinitionSnapshot) -> Result<(), String> {
        let Some((namespace, name)) = snapshot.content.qualified_name.split_once(':') else {
            return Err("Definition Snapshot has an invalid qualified name".to_string());
        };
        if namespace != "repository" {
            return Err(
                "only repository Workflow sources require an execution trust record".to_string(),
            );
        }
        let capability_digest = sha256(
            &serde_json::to_vec(&snapshot.content.transitive_capabilities)
                .map_err(|error| error.to_string())?,
        );
        self.connection()?.execute("insert or replace into definition_source_trust(source_namespace,source_name,source_digest,capability_digest,trusted_unix_ms) values(?1,?2,?3,?4,?5)",params![namespace,name,snapshot.source_trust_digest,capability_digest,now_ms()]).map_err(sql_error)?;
        Ok(())
    }

    pub(crate) fn start(&self, request: StartRun) -> Result<StartRunResult, String> {
        self.start_with_control(request, "running", false)
    }

    pub(crate) fn start_quarantined(&self, request: StartRun) -> Result<StartRunResult, String> {
        self.start_with_control(request, "pause_requested", true)
    }

    /// Starts pre-admission classification. The caller must omit repository
    /// identity and restrict actor authority to provider reads and inference;
    /// this permits untrusted intake without granting workspace access.
    pub(crate) fn start_intake(&self, request: StartRun) -> Result<StartRunResult, String> {
        if request.repository_id.is_some() {
            return Err(
                "quarantined intake cannot be associated with a repository workspace".to_string(),
            );
        }
        let allowed = BTreeSet::from([
            Capability::ProviderRead,
            Capability::ProcessExecute,
            Capability::NetworkRead,
        ]);
        if !request.actor_capabilities.is_subset(&allowed) {
            return Err(
                "quarantined intake authority exceeds provider reads and confined inference"
                    .to_string(),
            );
        }
        self.start_with_control(request, "running", true)
    }

    fn start_with_control(
        &self,
        request: StartRun,
        initial_control: &str,
        allow_untrusted: bool,
    ) -> Result<StartRunResult, String> {
        validate_inputs(&request.snapshot, &request.inputs)?;
        if !allow_untrusted
            && initial_control == "running"
            && request
                .inputs
                .iter()
                .any(|input| input.trust != TrustClass::Trusted)
        {
            return Err(
                "manual invocation cannot admit untrusted or externally controlled input"
                    .to_string(),
            );
        }
        let input_bytes = canonical_input_bytes(&request.inputs)?;
        let input_digest = sha256(&input_bytes);
        let now = now_ms();
        let mut conn = self.connection()?;
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        if let Some(name) = request
            .snapshot
            .content
            .qualified_name
            .strip_prefix("repository:")
        {
            let capability_digest = sha256(
                &serde_json::to_vec(&request.snapshot.content.transitive_capabilities)
                    .map_err(|error| error.to_string())?,
            );
            let trusted: bool = transaction.query_row("select exists(select 1 from definition_source_trust where source_namespace='repository' and source_name=?1 and source_digest=?2 and capability_digest=?3)",params![name,request.snapshot.source_trust_digest,capability_digest],|row|row.get(0)).map_err(sql_error)?;
            if !trusted {
                return Err("repository Workflow source or capability envelope is not trusted for execution".to_string());
            }
        }
        if let Some(key) = request.idempotency_key.as_deref()
            && let Some((run_id, existing_snapshot, existing_input)) = transaction
                .query_row(
                    "select id, snapshot_digest, input_digest from workflow_run where invocation_key = ?1",
                    [key],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?)),
                )
                .optional()
                .map_err(sql_error)?
        {
            if existing_snapshot != request.snapshot.digest || existing_input != input_digest {
                return Err(format!("idempotency key '{key}' was already used with different inputs or definition"));
            }
            return Ok(StartRunResult { run_id: RunId(run_id), created: false, input_digest });
        }

        transaction.execute(
            "insert or ignore into definition_snapshot (digest, schema_version, canonical_bytes, definition_name, created_unix_ms) values (?1, ?2, ?3, ?4, ?5)",
            params![request.snapshot.digest, request.snapshot.content.schema_version, request.snapshot.canonical_bytes, request.snapshot.content.qualified_name, now],
        ).map_err(sql_error)?;
        persist_pinned_snapshots(&transaction, &request.snapshot.content, now)?;
        let run_id = RunId(random_id(&transaction)?);
        let grant_id = AuthorityGrantId(random_id(&transaction)?);
        let budget_id = random_id(&transaction)?;
        let policy_capabilities = request
            .snapshot
            .content
            .admission_policy
            .as_ref()
            .map_or_else(
                || request.snapshot.content.capabilities.clone(),
                |policy| {
                    request
                        .snapshot
                        .content
                        .capabilities
                        .intersection(&policy.capabilities)
                        .cloned()
                        .collect()
                },
            );
        let granted = policy_capabilities
            .intersection(&request.actor_capabilities)
            .cloned()
            .collect::<BTreeSet<_>>();
        transaction.execute(
            "insert into authority_grant (id, run_id, basis, snapshot_digest, input_digest, capabilities_json, secret_scope_json, target_scope_json, created_unix_ms) values (?1, ?2, 'manual_invocation', ?3, ?4, ?5, '[]', '[\"local\"]', ?6)",
            params![grant_id.as_str(), run_id.as_str(), request.snapshot.digest, input_digest, json(&granted)?, now],
        ).map_err(sql_error)?;
        transaction.execute(
            "insert into workflow_budget (id,remaining_attempts,remaining_fan_out,remaining_child_depth,remaining_mutations,created_unix_ms,updated_unix_ms) values(?1,?2,?3,?4,?5,?6,?6)",
            params![budget_id, request.snapshot.content.budgets.max_attempts, request.snapshot.content.budgets.max_fan_out, request.snapshot.content.budgets.max_child_depth, request.snapshot.content.budgets.max_mutations, now],
        ).map_err(sql_error)?;
        transaction.execute(
            "insert into workflow_run (id, snapshot_digest, definition_name, repository_id, input_digest, invocation_key, actor, authority_grant_id, budget_id, state, control, revision, remaining_attempts, remaining_fan_out, remaining_child_depth, remaining_mutations, created_unix_ms, updated_unix_ms) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'queued', ?10, 1, ?11, ?12, ?13, ?14, ?15, ?15)",
            params![run_id.as_str(), request.snapshot.digest, request.snapshot.content.qualified_name, request.repository_id.as_ref().map(RepositoryId::as_str), input_digest, request.idempotency_key, request.actor, grant_id.as_str(), budget_id, initial_control, request.snapshot.content.budgets.max_attempts, request.snapshot.content.budgets.max_fan_out, request.snapshot.content.budgets.max_child_depth, request.snapshot.content.budgets.max_mutations, now],
        ).map_err(sql_error)?;

        let mut step_ids = BTreeMap::new();
        for step in &request.snapshot.content.steps {
            let step_id = StepId(random_id(&transaction)?);
            transaction.execute(
                "insert into workflow_step (id, run_id, definition_step_id, class, implementation_id, implementation_revision, dependencies_json, input_bindings_json, outputs_json, condition_json, capabilities_json, state, attempt_count, created_unix_ms, updated_unix_ms) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'pending', 0, ?12, ?12)",
                params![step_id.as_str(), run_id.as_str(), step.id, enum_json(&step.class)?, step.implementation, step.implementation_revision, json(&step.dependencies)?, json(&step.inputs)?, json(&step.outputs)?, step.condition.as_ref().map(json).transpose()?, json(&step.capabilities)?, now],
            ).map_err(sql_error)?;
            step_ids.insert(step.id.clone(), step_id);
        }
        for input in &request.inputs {
            insert_artifact(&transaction, &run_id, None, input, now)?;
        }
        append_event(
            &transaction,
            &run_id,
            None,
            "run_created",
            &serde_json::json!({
                "snapshot_digest": request.snapshot.digest,
                "input_digest": input_digest,
                "authority_grant_id": grant_id,
                "steps": step_ids,
            }),
            now,
        )?;
        transaction.commit().map_err(sql_error)?;
        Ok(StartRunResult {
            run_id,
            created: true,
            input_digest,
        })
    }

    pub(crate) fn inspect(&self, run_id: &RunId) -> Result<RunProjection, String> {
        let conn = self.connection()?;
        let run = conn.query_row(
            "select id, definition_name, snapshot_digest, state, control, revision, created_unix_ms, updated_unix_ms from workflow_run where id = ?1",
            [run_id.as_str()],
            |row| Ok(RunSummary {
                id: RunId(row.get(0)?),
                definition: row.get(1)?,
                snapshot_digest: row.get(2)?,
                state: parse_run_state(&row.get::<_, String>(3)?)?,
                control: row.get(4)?,
                revision: row.get(5)?,
                created_unix_ms: row.get(6)?,
                updated_unix_ms: row.get(7)?,
            }),
        ).optional().map_err(sql_error)?.ok_or_else(|| format!("workflow run '{}' was not found", run_id.as_str()))?;
        let mut statement = conn.prepare("select id, definition_step_id, class, implementation_id, state, attempt_count, blocker from workflow_step where run_id = ?1 order by rowid").map_err(sql_error)?;
        let steps = statement
            .query_map([run_id.as_str()], |row| {
                Ok(StepSummary {
                    id: StepId(row.get(0)?),
                    definition_step_id: row.get(1)?,
                    class: parse_json(&row.get::<_, String>(2)?)?,
                    implementation: row.get(3)?,
                    state: parse_step_state(&row.get::<_, String>(4)?)?,
                    attempt_count: row.get(5)?,
                    blocker: row.get(6)?,
                })
            })
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?;
        let determining_steps = determining_steps(run.state, &steps);
        let attempts = load_attempt_summaries(&conn, run_id)?;
        let artifacts = load_artifact_summaries(&conn, run_id)?;
        let approvals = load_approval_projections(&conn, run_id)?;
        let gates = load_gate_results(&conn, run_id)?;
        let effects = load_effect_intents(&conn, run_id)?;
        let authority = load_run_authority(&conn, run_id)?;
        let output = load_bounded_output(&conn, run_id, 256 * 1024)?;
        Ok(RunProjection {
            schema_version: 1,
            run,
            steps,
            determining_steps,
            attempts,
            artifacts,
            approvals,
            gates,
            effects,
            authority,
            output,
        })
    }

    pub(crate) fn attention(&self, limit: usize) -> Result<AttentionSummary, String> {
        let conn = self.connection()?;
        let mut statement = conn
            .prepare("select id,definition_name,snapshot_digest,state,control,revision,created_unix_ms,updated_unix_ms from workflow_run where state in ('input_required','recovery_required','failed') order by updated_unix_ms desc,id limit ?1")
            .map_err(sql_error)?;
        let runs = statement
            .query_map([limit.min(i64::MAX as usize) as i64], run_summary_from_row)
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?;
        let pending_approvals = conn
            .query_row(
                "select count(*) from approval_request where state='pending'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(sql_error)?
            .max(0) as u64;
        let recovery_required_attempts = conn
            .query_row(
                "select count(*) from step_attempt where state='recovery_required'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(sql_error)?
            .max(0) as u64;
        let quarantined_workspaces = conn
            .query_row(
                "select count(*) from execution_workspace where state='quarantined'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(sql_error)?
            .max(0) as u64;
        Ok(AttentionSummary {
            runs,
            pending_approvals,
            recovery_required_attempts,
            quarantined_workspaces,
        })
    }

    pub(crate) fn list_for_repository(
        &self,
        repository: &Path,
        limit: usize,
    ) -> Result<Vec<RunSummary>, String> {
        let tracked = repository
            .canonicalize()
            .map_err(|error| format!("resolve repository path {}: {error}", repository.display()))?
            .display()
            .to_string();
        let conn = self.connection()?;
        let mut statement = conn
            .prepare("select run.id,run.definition_name,run.snapshot_digest,run.state,run.control,run.revision,run.created_unix_ms,run.updated_unix_ms from workflow_run run join repository_identity repository on repository.id=run.repository_id where repository.tracked_path=?1 order by run.updated_unix_ms desc,run.id limit ?2")
            .map_err(sql_error)?;
        statement
            .query_map(
                params![tracked, limit.min(1000) as i64],
                run_summary_from_row,
            )
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)
    }

    pub(crate) fn list(&self, limit: usize) -> Result<Vec<RunSummary>, String> {
        let conn = self.connection()?;
        let mut statement = conn.prepare("select id, definition_name, snapshot_digest, state, control, revision, created_unix_ms, updated_unix_ms from workflow_run order by updated_unix_ms desc limit ?1").map_err(sql_error)?;
        statement
            .query_map([limit.min(1000) as i64], |row| {
                Ok(RunSummary {
                    id: RunId(row.get(0)?),
                    definition: row.get(1)?,
                    snapshot_digest: row.get(2)?,
                    state: parse_run_state(&row.get::<_, String>(3)?)?,
                    control: row.get(4)?,
                    revision: row.get(5)?,
                    created_unix_ms: row.get(6)?,
                    updated_unix_ms: row.get(7)?,
                })
            })
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)
    }

    pub(crate) fn history(
        &self,
        run_id: &RunId,
        after: i64,
        limit: usize,
    ) -> Result<Vec<RunEvent>, String> {
        let conn = self.connection()?;
        let mut statement = conn.prepare("select id,run_id,step_id,kind,data_json,created_unix_ms from run_event where run_id=?1 and id>?2 order by id limit ?3").map_err(sql_error)?;
        statement
            .query_map(
                params![run_id.as_str(), after, limit.clamp(1, 1000) as i64],
                |row| {
                    Ok(RunEvent {
                        id: row.get(0)?,
                        run_id: RunId(row.get(1)?),
                        step_id: row.get::<_, Option<String>>(2)?.map(StepId),
                        kind: row.get(3)?,
                        data_json: row.get(4)?,
                        created_unix_ms: row.get(5)?,
                    })
                },
            )
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)
    }

    pub(crate) fn set_control(&self, run_id: &RunId, control: &str) -> Result<(), String> {
        if !matches!(control, "running" | "pause_requested" | "cancel_requested") {
            return Err(format!("unsupported run control '{control}'"));
        }
        let mut conn = self.connection()?;
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let now = now_ms();
        let root_is_controllable: bool = transaction
            .query_row(
                "select exists(select 1 from workflow_run where id=?1 and state not in ('completed','failed','cancelled'))",
                [run_id.as_str()],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        if !root_is_controllable {
            return Err(format!("run '{}' is missing or terminal", run_id.as_str()));
        }
        let affected = {
            let mut statement = transaction.prepare("with recursive affected(id) as (select ?1 union select l.child_run_id from workflow_run_link l join affected a on a.id=l.parent_run_id where l.propagation='lineage') select id from affected").map_err(sql_error)?;
            statement
                .query_map([run_id.as_str()], |row| row.get::<_, String>(0))
                .map_err(sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)?
        };
        let changed = transaction.execute("with recursive affected(id) as (select ?1 union select l.child_run_id from workflow_run_link l join affected a on a.id=l.parent_run_id where l.propagation='lineage') update workflow_run set control=?2,revision=revision+1,updated_unix_ms=?3 where id in (select id from affected) and state not in ('completed','failed','cancelled')", params![run_id.as_str(), control, now]).map_err(sql_error)?;
        if changed == 0 {
            return Err(format!("run '{}' is missing or terminal", run_id.as_str()));
        }
        append_event(
            &transaction,
            run_id,
            None,
            "run_control_changed",
            &serde_json::json!({"control":control}),
            now,
        )?;
        for affected_run in affected {
            recompute_run_state(&transaction, &RunId(affected_run), now)?;
        }
        transaction.commit().map_err(sql_error)
    }

    pub(crate) fn step_input_digest(
        &self,
        run_id: &RunId,
        step_id: &StepId,
    ) -> Result<String, String> {
        step_input_artifacts(&self.connection()?, run_id, step_id).map(|(digest, _)| digest)
    }

    pub(crate) fn create_approval(
        &self,
        run_id: &RunId,
        step_id: &StepId,
        mode: ApprovalMode,
        input_digest: &str,
        evidence_digest: &str,
        expires_unix_ms: Option<i64>,
    ) -> Result<ApprovalRequest, String> {
        let mut conn = self.connection()?;
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let attempt_id = AttemptId(random_id(&transaction)?);
        let request_id = ApprovalRequestId(random_id(&transaction)?);
        let mode_text = enum_json(&mode)?;
        let (current_input_digest, input_artifacts) =
            step_input_artifacts(&transaction, run_id, step_id)?;
        if current_input_digest != input_digest {
            return Err(
                "Approval input digest does not match the Step's exact Artifact bindings"
                    .to_string(),
            );
        }
        if evidence_digest.is_empty() {
            return Err("Approval evidence digest cannot be empty".to_string());
        }
        let request_digest = sha256(
            format!(
                "{}\0{}\0{}\0{}\0{}\0{:?}",
                run_id.as_str(),
                step_id.as_str(),
                mode_text,
                input_digest,
                evidence_digest,
                expires_unix_ms,
            )
            .as_bytes(),
        );
        let now = now_ms();
        let inserted = transaction.execute("insert into step_attempt (id, run_id, step_id, ordinal, state, input_digest, implementation_id, implementation_revision, created_unix_ms, updated_unix_ms) select ?1, ?2, ?3, attempt_count + 1, 'waiting', ?4, implementation_id, implementation_revision, ?5, ?5 from workflow_step where id = ?3 and run_id = ?2 and class='approval' and state='runnable'", params![attempt_id.as_str(), run_id.as_str(), step_id.as_str(), input_digest, now]).map_err(sql_error)?;
        if inserted != 1 {
            return Err(
                "Approval Step is missing, not runnable, or has the wrong primitive class"
                    .to_string(),
            );
        }
        for (port, artifact) in input_artifacts {
            transaction
                .execute(
                    "insert into attempt_input(attempt_id,port,artifact_id,artifact_revision) values(?1,?2,?3,?4)",
                    params![attempt_id.as_str(), port, artifact.id.as_str(), artifact.revision],
                )
                .map_err(sql_error)?;
        }
        transaction.execute("insert into approval_request (id, run_id, step_id, attempt_id, mode, request_digest, input_digest, evidence_digest, expires_unix_ms, state, created_unix_ms, updated_unix_ms) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'pending', ?10, ?10)", params![request_id.as_str(), run_id.as_str(), step_id.as_str(), attempt_id.as_str(), mode_text, request_digest, input_digest, evidence_digest, expires_unix_ms, now]).map_err(sql_error)?;
        let changed = transaction.execute("update workflow_step set state = 'input_required', attempt_count = attempt_count + 1, updated_unix_ms = ?2 where id = ?1 and state='runnable' and class='approval'", params![step_id.as_str(), now]).map_err(sql_error)?;
        if changed != 1 {
            return Err("Approval Step changed before its request was persisted".to_string());
        }
        recompute_run_state(&transaction, run_id, now)?;
        append_event(
            &transaction,
            run_id,
            Some(step_id),
            "approval_requested",
            &serde_json::json!({"request_id": request_id, "attempt_id": attempt_id, "request_digest": request_digest}),
            now,
        )?;
        transaction.commit().map_err(sql_error)?;
        Ok(ApprovalRequest {
            id: request_id,
            run_id: run_id.clone(),
            step_id: step_id.clone(),
            attempt_id,
            mode,
            request_digest,
            input_digest: input_digest.to_string(),
            evidence_digest: evidence_digest.to_string(),
            expires_unix_ms,
            state: "pending".to_string(),
        })
    }

    pub(crate) fn decide_approval(
        &self,
        request_id: &ApprovalRequestId,
        expected_input_digest: &str,
        expected_evidence_digest: &str,
        approved: bool,
        actor: &str,
        reason: Option<&str>,
    ) -> Result<ApprovalDecision, String> {
        let mut conn = self.connection()?;
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let request = transaction.query_row("select run_id, step_id, attempt_id, input_digest, evidence_digest, expires_unix_ms, state from approval_request where id = ?1", [request_id.as_str()], |row| Ok((RunId(row.get(0)?), StepId(row.get(1)?), AttemptId(row.get(2)?), row.get::<_, String>(3)?, row.get::<_, String>(4)?, row.get::<_, Option<i64>>(5)?, row.get::<_, String>(6)?))).optional().map_err(sql_error)?.ok_or_else(|| format!("approval request '{}' was not found", request_id.as_str()))?;
        let now = now_ms();
        if request.6 != "pending" {
            return Err(format!("approval request is {}", request.6));
        }
        if request.3 != expected_input_digest || request.4 != expected_evidence_digest {
            transaction.execute("update approval_request set state = 'invalidated', updated_unix_ms = ?2 where id = ?1", params![request_id.as_str(), now]).map_err(sql_error)?;
            transaction.execute("update step_attempt set state='failed',terminal_reason='approval evidence invalidated',updated_unix_ms=?2 where id=?1",params![request.2.as_str(),now]).map_err(sql_error)?;
            transaction.execute("update workflow_step set state='runnable',blocker='approval evidence changed',updated_unix_ms=?2 where id=?1",params![request.1.as_str(),now]).map_err(sql_error)?;
            recompute_run_state(&transaction, &request.0, now)?;
            transaction.commit().map_err(sql_error)?;
            return Err("approval request inputs or evidence changed".to_string());
        }
        let (current_input_digest, _) = step_input_artifacts(&transaction, &request.0, &request.1)?;
        if current_input_digest != request.3 {
            return Err("Approval Step inputs no longer match the request".to_string());
        }
        if request.5.is_some_and(|expires| expires <= now) {
            transaction.execute("update approval_request set state = 'expired', updated_unix_ms = ?2 where id = ?1", params![request_id.as_str(), now]).map_err(sql_error)?;
            transaction.execute("update step_attempt set state='failed',terminal_reason='approval expired',updated_unix_ms=?2 where id=?1",params![request.2.as_str(),now]).map_err(sql_error)?;
            transaction.execute("update workflow_step set state='runnable',blocker='approval expired',updated_unix_ms=?2 where id=?1",params![request.1.as_str(),now]).map_err(sql_error)?;
            recompute_run_state(&transaction, &request.0, now)?;
            transaction.commit().map_err(sql_error)?;
            return Err("approval request expired".to_string());
        }
        let decision_id = ApprovalDecisionId(random_id(&transaction)?);
        transaction.execute("insert into approval_decision (id, request_id, attempt_id, decision, input_digest, evidence_digest, actor, reason, decided_unix_ms) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)", params![decision_id.as_str(), request_id.as_str(), request.2.as_str(), if approved { "approve" } else { "reject" }, request.3, request.4, actor, reason, now]).map_err(sql_error)?;
        transaction
            .execute(
                "update approval_request set state = ?2, updated_unix_ms = ?3 where id = ?1",
                params![
                    request_id.as_str(),
                    if approved { "approved" } else { "rejected" },
                    now
                ],
            )
            .map_err(sql_error)?;
        transaction.execute("update step_attempt set state = 'completed', terminal_reason = ?2, updated_unix_ms = ?3 where id = ?1", params![request.2.as_str(), if approved { "approved" } else { "rejected" }, now]).map_err(sql_error)?;
        transaction.execute("update workflow_step set state = 'completed', outcome = ?2, updated_unix_ms = ?3 where id = ?1", params![request.1.as_str(), if approved { "approved" } else { "rejected" }, now]).map_err(sql_error)?;
        let outputs: BTreeMap<String, crate::definition::Port> = transaction
            .query_row(
                "select outputs_json from workflow_step where id=?1",
                [request.1.as_str()],
                |row| row.get::<_, String>(0),
            )
            .map_err(sql_error)
            .and_then(|value| serde_json::from_str(&value).map_err(|error| error.to_string()))?;
        let payload = serde_json::to_vec(
            &serde_json::json!({"approved":approved,"actor":actor,"reason":reason}),
        )
        .map_err(|error| error.to_string())?;
        let untrusted: bool = transaction.query_row("select exists(select 1 from attempt_input input join artifact artifact on artifact.id=input.artifact_id and artifact.revision=input.artifact_revision where input.attempt_id=?1 and artifact.trust!='trusted')",[request.2.as_str()],|row|row.get(0)).map_err(sql_error)?;
        for (port, declaration) in outputs {
            let artifact_id = random_id(&transaction)?;
            transaction.execute("insert into artifact(id,revision,run_id,producer_attempt_id,port,artifact_type,schema_revision,digest,trust,sensitivity,payload_inline,size,created_unix_ms) values(?1,1,?2,?3,?4,?5,1,?6,?7,'internal',?8,?9,?10)",params![artifact_id,request.0.as_str(),request.2.as_str(),port,declaration.artifact_type,sha256(&payload),if untrusted {"derived_untrusted"} else {"trusted"},payload,payload.len() as i64,now]).map_err(sql_error)?;
            transaction.execute("insert into artifact_lineage(artifact_id,artifact_revision,source_artifact_id,source_revision,consumer_port) select ?1,1,artifact_id,artifact_revision,port from attempt_input where attempt_id=?2",params![artifact_id,request.2.as_str()]).map_err(sql_error)?;
        }
        recompute_run_state(&transaction, &request.0, now)?;
        append_event(
            &transaction,
            &request.0,
            Some(&request.1),
            "approval_decided",
            &serde_json::json!({"request_id": request_id, "decision_id": decision_id, "approved": approved}),
            now,
        )?;
        transaction.commit().map_err(sql_error)?;
        Ok(ApprovalDecision {
            id: decision_id,
            request_id: request_id.clone(),
            approved,
            actor: actor.to_string(),
            reason: reason.map(str::to_string),
            decided_unix_ms: now,
        })
    }

    pub(crate) fn append_output(
        &self,
        attempt_id: &AttemptId,
        fencing_token: i64,
        stream: &str,
        bytes: &[u8],
    ) -> Result<(), String> {
        if !matches!(stream, "stdout" | "stderr" | "system") {
            return Err("invalid output stream".to_string());
        }
        let conn = self.connection()?;
        let existing: i64 = conn
            .query_row(
                "select coalesce(sum(length(bytes)), 0) from attempt_output where attempt_id = ?1",
                [attempt_id.as_str()],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        let remaining = MAX_OUTPUT_BYTES.saturating_sub(existing as usize);
        let bounded = &bytes[..bytes.len().min(remaining)];
        let changed = conn.execute("insert into attempt_output (attempt_id, sequence, stream, bytes, truncated, created_unix_ms) select ?1, coalesce(max(sequence), 0) + 1, ?3, ?4, ?5, ?6 from attempt_output where attempt_id = ?1 having exists(select 1 from attempt_lease where attempt_id = ?1 and fencing_token = ?2 and expires_unix_ms > ?6)", params![attempt_id.as_str(), fencing_token, stream, bounded, bytes.len() > bounded.len(), now_ms()]).map_err(sql_error)?;
        if changed == 0 {
            return Err("stale Attempt lease cannot append output".to_string());
        }
        Ok(())
    }

    /// Applies conservative retention in one transaction, then removes only
    /// content-addressed files that no Artifact references. Active work and
    /// audit facts needed to explain lineage or decisions are always retained.
    pub(crate) fn apply_retention(
        &self,
        policy: RetentionPolicy,
    ) -> Result<RetentionReport, String> {
        let now = now_ms();
        let mut conn = self.connection()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let deleted = |tx: &rusqlite::Transaction<'_>, sql: &str, cutoff: i64| {
            tx.execute(sql, [cutoff])
                .map(|count| count as u64)
                .map_err(sql_error)
        };
        let events_deleted = deleted(
            &tx,
            "delete from run_event where created_unix_ms<?1 and kind in ('attempt_output','step_changed','attempt_changed') and exists(select 1 from workflow_run run where run.id=run_event.run_id and run.state in ('completed','failed','cancelled'))",
            now.saturating_sub(policy.noisy_event_age_ms),
        )?;
        let output_rows_deleted = deleted(
            &tx,
            "delete from attempt_output where created_unix_ms<?1 and exists(select 1 from step_attempt attempt where attempt.id=attempt_output.attempt_id and attempt.state in ('completed','failed','cancelled'))",
            now.saturating_sub(policy.attempt_output_age_ms),
        )?;
        let notifications_deleted = deleted(
            &tx,
            "delete from notification_delivery where updated_unix_ms<?1 and state in ('delivered','failed')",
            now.saturating_sub(policy.notification_age_ms),
        )?;
        let provider_observations_deleted: u64 = if [
            "provider_item_observation",
            "provider_item_current",
            "trigger_occurrence_detail",
            "workflow_admission_record",
        ]
        .into_iter()
        .all(|table| {
            tx.query_row(
                "select exists(select 1 from sqlite_schema where type='table' and name=?1)",
                [table],
                |row| row.get::<_, bool>(0),
            )
            .unwrap_or(false)
        }) {
            deleted(
                &tx,
                "delete from provider_item_observation where observed_unix_ms<?1 and not exists(select 1 from provider_item_current current where current.provider_item=provider_item_observation.provider_item and current.observation_revision=provider_item_observation.observation_revision) and not exists(select 1 from trigger_occurrence_detail occurrence where occurrence.provider_item=provider_item_observation.provider_item and occurrence.observation_revision=provider_item_observation.observation_revision) and not exists(select 1 from workflow_admission_record admission where admission.provider_item=provider_item_observation.provider_item and admission.observation_revision=provider_item_observation.observation_revision)",
                now.saturating_sub(policy.provider_observation_age_ms),
            )?
        } else {
            0
        };
        tx.commit().map_err(sql_error)?;

        let referenced = self
            .connection()?
            .prepare("select relative_path from artifact_blob")
            .map_err(sql_error)?
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(sql_error)?
            .collect::<Result<BTreeSet<_>, _>>()
            .map_err(sql_error)?;
        let mut blobs_deleted = 0;
        if let Ok(entries) = fs::read_dir(&self.blob_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if entry.file_type().is_ok_and(|kind| kind.is_file()) && !referenced.contains(&name)
                {
                    fs::remove_file(entry.path()).map_err(|error| {
                        format!(
                            "remove orphaned Artifact blob {}: {error}",
                            entry.path().display()
                        )
                    })?;
                    blobs_deleted += 1;
                }
            }
        }
        Ok(RetentionReport {
            events_deleted,
            output_rows_deleted,
            notifications_deleted,
            provider_observations_deleted,
            blobs_deleted,
        })
    }

    /// Read-only workflow health and digest verification for doctor/debug.
    pub(crate) fn health(&self) -> Result<WorkflowHealth, String> {
        let conn = self.connection()?;
        let mut problems = Vec::new();
        let quick_check: String = conn
            .query_row("pragma quick_check(1)", [], |row| row.get(0))
            .map_err(sql_error)?;
        if quick_check != "ok" {
            problems.push(format!("SQLite quick_check failed: {quick_check}"));
        }
        let foreign_key_violations: i64 = conn
            .query_row("select count(*) from pragma_foreign_key_check", [], |row| {
                row.get(0)
            })
            .map_err(sql_error)?;
        if foreign_key_violations > 0 {
            problems.push(format!("{foreign_key_violations} foreign-key violation(s)"));
        }
        {
            let mut statement = conn
                .prepare("select digest,canonical_bytes from definition_snapshot")
                .map_err(sql_error)?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, rusqlite::types::Value>(1)?,
                    ))
                })
                .map_err(sql_error)?;
            for row in rows {
                let (digest, value) = row.map_err(sql_error)?;
                let bytes = match value {
                    rusqlite::types::Value::Blob(bytes) => bytes,
                    rusqlite::types::Value::Text(text) => text.into_bytes(),
                    _ => Vec::new(),
                };
                if sha256(&bytes) != digest {
                    problems.push(format!("Definition Snapshot digest mismatch: {digest}"));
                }
            }
        }
        {
            let mut statement = conn
                .prepare("select id,revision,digest,payload_inline,blob_digest from artifact")
                .map_err(sql_error)?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, u32>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<Vec<u8>>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                })
                .map_err(sql_error)?;
            for row in rows {
                let (id, revision, digest, inline, blob) = row.map_err(sql_error)?;
                if let Some(bytes) = inline {
                    if sha256(&bytes) != digest {
                        problems.push(format!("Artifact digest mismatch: {id}@{revision}"));
                    }
                } else if let Some(blob) = blob {
                    let exists: Option<String> = conn
                        .query_row(
                            "select relative_path from artifact_blob where digest=?1",
                            [&blob],
                            |row| row.get(0),
                        )
                        .optional()
                        .map_err(sql_error)?;
                    match exists {
                        Some(path) if self.blob_dir.join(&path).is_file() => {}
                        _ => problems.push(format!("Artifact blob is missing: {id}@{revision}")),
                    }
                } else {
                    problems.push(format!("Artifact has no payload: {id}@{revision}"));
                }
            }
        }
        let lineage_cycle: bool = conn.query_row("with recursive walk(root_id,root_rev,id,rev,path,cycle) as (select artifact_id,artifact_revision,source_artifact_id,source_revision,artifact_id||'@'||artifact_revision||'/',0 from artifact_lineage union all select walk.root_id,walk.root_rev,lineage.source_artifact_id,lineage.source_revision,walk.path||lineage.artifact_id||'@'||lineage.artifact_revision||'/',instr(walk.path,lineage.source_artifact_id||'@'||lineage.source_revision||'/')>0 from walk join artifact_lineage lineage on lineage.artifact_id=walk.id and lineage.artifact_revision=walk.rev where walk.cycle=0) select exists(select 1 from walk where cycle=1)",[],|row|row.get(0)).map_err(sql_error)?;
        if lineage_cycle {
            problems.push("Artifact lineage contains a cycle".to_string());
        }
        let dangling_lineage: bool = conn.query_row("select exists(select 1 from artifact_lineage lineage left join artifact produced on produced.id=lineage.artifact_id and produced.revision=lineage.artifact_revision left join artifact source on source.id=lineage.source_artifact_id and source.revision=lineage.source_revision where produced.id is null or source.id is null)", [], |row| row.get(0)).map_err(sql_error)?;
        if dangling_lineage {
            problems.push("Artifact lineage contains a dangling reference".to_string());
        }
        let child_cycle: bool = conn.query_row("with recursive descendants(root,id,path,cycle) as (select parent_run_id,child_run_id,parent_run_id||'/',0 from workflow_run_link union all select descendants.root,link.child_run_id,descendants.path||link.parent_run_id||'/',instr(descendants.path,link.child_run_id||'/')>0 from descendants join workflow_run_link link on link.parent_run_id=descendants.id where descendants.cycle=0) select exists(select 1 from descendants where cycle=1)", [], |row| row.get(0)).map_err(sql_error)?;
        if child_cycle {
            problems.push("Workflow child links contain a cycle".to_string());
        }
        let mut referenced_blob_paths = BTreeSet::new();
        let mut blob_rows = conn
            .prepare("select digest,relative_path from artifact_blob")
            .map_err(sql_error)?;
        for row in blob_rows
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(sql_error)?
        {
            let (digest, relative_path) = row.map_err(sql_error)?;
            referenced_blob_paths.insert(relative_path.clone());
            let relative = Path::new(&relative_path);
            if relative.components().count() != 1 {
                problems.push(format!("Artifact blob has unsafe relative path: {digest}"));
                continue;
            }
            let path = self.blob_dir.join(relative);
            match fs::read(&path) {
                Ok(bytes) if sha256(&bytes) == digest => {}
                Ok(_) => problems.push(format!("Artifact blob digest mismatch: {digest}")),
                Err(_) => problems.push(format!("Artifact blob is missing: {digest}")),
            }
        }
        let orphaned_blobs = fs::read_dir(&self.blob_dir).map_or(0, |entries| {
            entries
                .flatten()
                .filter(|entry| {
                    entry.file_type().is_ok_and(|kind| kind.is_file())
                        && !referenced_blob_paths
                            .contains(&entry.file_name().to_string_lossy().into_owned())
                })
                .count() as u64
        });
        if orphaned_blobs > 0 {
            problems.push(format!("{orphaned_blobs} orphaned Artifact blob(s)"));
        }
        let count = |sql: &str| {
            conn.query_row(sql, [], |row| row.get::<_, i64>(0))
                .map(|value| value.max(0) as u64)
                .map_err(sql_error)
        };
        let dangling_claims = count(
            "select count(*) from resource_claim claim left join attempt_lease lease on lease.attempt_id=claim.attempt_id where lease.attempt_id is null",
        )?;
        if dangling_claims > 0 {
            problems.push(format!("{dangling_claims} dangling resource claim(s)"));
        }
        let invalid_effects = count(
            "select count(*) from effect_intent effect left join step_attempt attempt on attempt.id=effect.attempt_id where attempt.id is null or effect.run_id!=attempt.run_id or effect.step_id!=attempt.step_id or not json_valid(effect.target_json) or not json_valid(effect.expected_pre_state_json) or not json_valid(effect.desired_post_state_json) or not json_valid(effect.input_revisions_json) or not json_valid(effect.gate_requirements_json) or not json_valid(effect.policy_revisions_json) or not json_valid(effect.resource_claims_json)",
        )?;
        if invalid_effects > 0 {
            problems.push(format!("{invalid_effects} invalid Effect Intent(s)"));
        }
        let health = WorkflowHealth {
            active_leases: count(
                "select count(*) from attempt_lease where expires_unix_ms > unixepoch('subsec')*1000",
            )?,
            dangling_claims,
            quarantined_workspaces: count(
                "select count(*) from execution_workspace where state='quarantined'",
            )?,
            overdue_waits: count(
                "select count(*) from workflow_step where state='waiting' and wake_unix_ms is not null and wake_unix_ms < unixepoch('subsec')*1000",
            )?,
            recovery_required_attempts: count(
                "select count(*) from step_attempt where state='recovery_required'",
            )?,
            unresolved_effects: count(
                "select count(*) from effect_intent where state in ('prepared','dispatched','indeterminate')",
            )?,
            enabled_triggers: count("select count(*) from trigger where enabled=1")?,
            orphaned_blobs,
            target_descriptors: vec![crate::target::local_descriptor()],
            integrity_ok: problems.is_empty(),
            problems,
        };
        Ok(health)
    }
}

struct InitializationLock(fs::File);

impl InitializationLock {
    fn acquire(database: &Path) -> Result<Self, String> {
        let lock_path = database.with_extension("init.lock");
        let mut options = fs::OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
            let file = options
                .open(&lock_path)
                .map_err(|error| format!("open {}: {error}", lock_path.display()))?;
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err(format!(
                    "lock {}: {}",
                    lock_path.display(),
                    std::io::Error::last_os_error()
                ));
            }
            return Ok(Self(file));
        }
        #[allow(unreachable_code)]
        Err("workflow initialization locking is unsupported".to_string())
    }
}

impl Drop for InitializationLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            unsafe {
                libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
            }
        }
    }
}

fn migrate(conn: &Connection, path: &Path) -> Result<(), String> {
    conn.execute_batch("begin immediate").map_err(sql_error)?;
    let version: u32 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|error| {
            let _ = conn.execute_batch("rollback");
            sql_error(error)
        })?;
    if version > SCHEMA_VERSION {
        let _ = conn.execute_batch("rollback");
        return Err(format!(
            "workflow database {} has future schema version {version}; this Prism supports up to {SCHEMA_VERSION}",
            path.display()
        ));
    }
    if version == SCHEMA_VERSION {
        if let Err(error) = validate_schema(conn) {
            let _ = conn.execute_batch("rollback");
            return Err(error);
        }
        conn.execute_batch("commit").map_err(sql_error)?;
        return Ok(());
    }
    let result = (|| -> rusqlite::Result<()> {
        if version == 0 {
            conn.execute_batch(SCHEMA)?;
        } else if version < 2 {
            conn.execute_batch(MIGRATION_V2)?;
        }
        conn.execute_batch(MIGRATION_V3)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)
    })();
    if let Err(error) = result {
        let _ = conn.execute_batch("rollback");
        return Err(sql_error(error));
    }
    conn.execute_batch("commit").map_err(sql_error)?;
    validate_schema(conn)
}

fn validate_schema(conn: &Connection) -> Result<(), String> {
    for table in [
        "definition_snapshot",
        "workflow_budget",
        "workflow_run",
        "workflow_step",
        "run_event",
        "step_attempt",
        "artifact",
        "authority_grant",
        "approval_request",
        "attempt_lease",
        "resource_claim",
        "effect_intent",
        "legacy_migration_journal",
        "legacy_run_import",
        "workflow_cutover",
    ] {
        let exists: bool = conn
            .query_row(
                "select exists(select 1 from sqlite_master where type = 'table' and name = ?1)",
                [table],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        if !exists {
            return Err(format!("workflow database schema is missing table {table}"));
        }
    }
    let foreign_key_errors = conn
        .prepare("pragma foreign_key_check")
        .and_then(|mut statement| statement.exists([]))
        .map_err(sql_error)?;
    if foreign_key_errors {
        return Err("workflow database contains foreign-key violations".to_string());
    }
    Ok(())
}

const SCHEMA: &str = r#"
create table definition_snapshot (
  digest text primary key, schema_version integer not null, canonical_bytes blob not null,
  definition_name text not null, created_unix_ms integer not null
);
create table definition_source_trust (
  source_namespace text not null, source_name text not null, source_digest text not null,
  capability_digest text not null, trusted_unix_ms integer not null,
  primary key(source_namespace, source_name, source_digest, capability_digest)
);
create table repository_identity (
  id text primary key, tracked_path text unique, provider_identity text unique, created_unix_ms integer not null
);
create table workflow_budget (
  id text primary key, remaining_attempts integer not null, remaining_fan_out integer not null,
  remaining_child_depth integer not null, remaining_mutations integer not null,
  created_unix_ms integer not null, updated_unix_ms integer not null
);
create table workflow_run (
  id text primary key, snapshot_digest text not null references definition_snapshot(digest),
  definition_name text not null, repository_id text, input_digest text not null, invocation_key text unique,
  actor text not null, authority_grant_id text not null, budget_id text not null references workflow_budget(id), lineage_depth integer not null default 0, state text not null,
  control text not null, revision integer not null, remaining_attempts integer not null,
  remaining_fan_out integer not null, remaining_child_depth integer not null,
  remaining_mutations integer not null, created_unix_ms integer not null, updated_unix_ms integer not null
);
create table workflow_step (
  id text primary key, run_id text not null references workflow_run(id), definition_step_id text not null,
  class text not null, implementation_id text not null, implementation_revision integer not null,
  dependencies_json text not null, input_bindings_json text not null, outputs_json text not null,
  condition_json text, capabilities_json text not null, state text not null, outcome text,
  attempt_count integer not null, blocker text, wake_unix_ms integer, created_unix_ms integer not null,
  updated_unix_ms integer not null, unique(run_id, definition_step_id)
);
create table workflow_run_link (
  parent_run_id text not null references workflow_run(id), parent_step_id text not null references workflow_step(id),
  child_run_id text not null unique references workflow_run(id), call_key text not null,
  child_snapshot_digest text not null, input_digest text not null, purpose text not null,
  propagation text not null, unique(parent_run_id, parent_step_id, call_key, child_snapshot_digest, input_digest, purpose)
);
create table workflow_call_reservation (
  parent_run_id text not null references workflow_run(id), parent_step_id text not null references workflow_step(id),
  call_key text not null, child_snapshot_digest text not null, input_digest text not null, purpose text not null,
  propagation text not null, effect_intent_id text, child_run_id text references workflow_run(id), created_unix_ms integer not null,
  primary key(parent_run_id,parent_step_id,call_key,child_snapshot_digest,input_digest,purpose)
);
create table run_worktree_link (
  run_id text not null references workflow_run(id), workspace_id text not null,
  worktree_session_id text, incarnation integer, retired_unix_ms integer,
  primary key(run_id, workspace_id)
);
create table run_event (
  id integer primary key autoincrement, run_id text not null references workflow_run(id),
  step_id text references workflow_step(id), kind text not null, data_json text not null,
  created_unix_ms integer not null
);
create index run_event_page on run_event(run_id, id);
create index workflow_run_history on workflow_run(updated_unix_ms desc, id);
create index workflow_run_attention on workflow_run(state, updated_unix_ms desc, id);
create table step_attempt (
  id text primary key, run_id text not null references workflow_run(id), step_id text not null references workflow_step(id),
  ordinal integer not null, state text not null, input_digest text not null, implementation_id text not null,
  implementation_revision integer not null, target_id text, workspace_id text, requested_claims_json text not null default '[]', fencing_generation integer not null default 0, terminal_reason text,
  created_unix_ms integer not null, updated_unix_ms integer not null, unique(step_id, ordinal)
);
create index step_attempt_run on step_attempt(run_id, created_unix_ms, id);
create index step_attempt_claim on step_attempt(state, target_id, created_unix_ms, id);
create table attempt_process (
  attempt_id text primary key references step_attempt(id), target_process_id text,
  pid integer, process_identity integer, state text not null, updated_unix_ms integer not null
);
create table attempt_input (
  attempt_id text not null references step_attempt(id), port text not null,
  artifact_id text not null, artifact_revision integer not null,
  primary key(attempt_id,port)
);
create table attempt_output (
  attempt_id text not null references step_attempt(id), sequence integer not null, stream text not null,
  bytes blob not null, truncated integer not null, created_unix_ms integer not null,
  primary key(attempt_id, sequence)
);
create index attempt_output_retention on attempt_output(created_unix_ms, attempt_id);
create table artifact (
  id text not null, revision integer not null, run_id text not null references workflow_run(id),
  producer_attempt_id text references step_attempt(id), port text not null, artifact_type text not null,
  schema_revision integer not null, digest text not null, trust text not null, sensitivity text not null,
  payload_inline blob, blob_digest text, size integer not null, created_unix_ms integer not null,
  primary key(id, revision), unique(producer_attempt_id, port, revision)
);
create index artifact_run on artifact(run_id, created_unix_ms, id, revision);
create table artifact_lineage (
  artifact_id text not null, artifact_revision integer not null, source_artifact_id text not null,
  source_revision integer not null, consumer_port text not null,
  primary key(artifact_id, artifact_revision, source_artifact_id, source_revision, consumer_port)
);
create table artifact_blob (digest text primary key, size integer not null, relative_path text not null);
create table authority_grant (
  id text primary key, run_id text, parent_grant_id text references authority_grant(id), basis text not null,
  snapshot_digest text not null, input_digest text not null, capabilities_json text not null,
  secret_scope_json text not null, target_scope_json text not null, expires_unix_ms integer,
  created_unix_ms integer not null
);
create table approval_request (
  id text primary key, run_id text not null references workflow_run(id), step_id text not null references workflow_step(id),
  attempt_id text not null unique references step_attempt(id), mode text not null, request_digest text not null,
  input_digest text not null, evidence_digest text not null, capabilities_json text not null default '[]',
  expires_unix_ms integer, state text not null, created_unix_ms integer not null, updated_unix_ms integer not null
);
create table approval_decision (
  id text primary key, request_id text not null unique references approval_request(id),
  attempt_id text not null references step_attempt(id), decision text not null, input_digest text not null,
  evidence_digest text not null, actor text not null, reason text, decided_unix_ms integer not null
);
create table gate_result (
  id text primary key, run_id text not null references workflow_run(id), step_id text not null references workflow_step(id),
  attempt_id text not null references step_attempt(id), subject_digest text not null, subject_generation text not null,
  evidence_json text not null, evidence_quality text not null, policy_revision text not null,
  status text not null, reason text not null, expires_unix_ms integer, created_unix_ms integer not null
);
create table notification_delivery (
  id text primary key, run_id text not null references workflow_run(id), step_id text not null references workflow_step(id),
  category text not null, message text not null, state text not null, error text, created_unix_ms integer not null,
  updated_unix_ms integer not null
);
create table admission_decision (
  id text primary key, run_id text not null references workflow_run(id), policy_revision text not null,
  observation_revision text not null, capabilities_json text not null, outcome text not null,
  expires_unix_ms integer, created_unix_ms integer not null
);
create table execution_workspace (
  id text primary key, repository_id text, target_id text not null, base_revision text not null,
  generation integer not null, worktree_session_id text, state text not null, quarantine_reason text,
  updated_unix_ms integer not null
);
create table attempt_lease (
  attempt_id text primary key references step_attempt(id), worker_id text not null, target_id text not null,
  fencing_token integer not null, expires_unix_ms integer not null, interruption_generation integer not null
);
create table resource_claim (
  attempt_id text not null references step_attempt(id), resource_key text not null, access text not null,
  expected_generation integer, acquired_unix_ms integer not null,
  primary key(attempt_id, resource_key)
);
create table resource_generation (
  resource_key text primary key, generation integer not null, updated_unix_ms integer not null
);
create index resource_claim_key on resource_claim(resource_key, access);
create table effect_intent (
  id text primary key, run_id text not null references workflow_run(id), step_id text not null references workflow_step(id),
  attempt_id text not null references step_attempt(id), kind text not null, state text not null,
  target_json text not null, expected_pre_state_json text not null, desired_post_state_json text not null,
  exact_head text, input_revisions_json text not null, gate_requirements_json text not null,
  policy_revisions_json text not null, authority_grant_id text not null references authority_grant(id),
  reconciliation_key text not null, resource_claims_json text not null, dispatch_generation integer not null,
  result_json text, created_unix_ms integer not null, updated_unix_ms integer not null,
  unique(kind, reconciliation_key)
);
create table trigger (id text primary key, definition_json text not null, enabled integer not null, created_unix_ms integer not null);
create table trigger_occurrence (id text primary key, trigger_id text not null references trigger(id), occurrence_key text not null, run_id text references workflow_run(id), created_unix_ms integer not null, unique(trigger_id, occurrence_key));
create index trigger_occurrence_page on trigger_occurrence(trigger_id, created_unix_ms, id);
create table trigger_checkpoint (trigger_id text primary key references trigger(id), checkpoint_json text not null, updated_unix_ms integer not null);
create table legacy_migration_journal (
  source_path text not null, source_schema_version integer not null, source_digest text not null,
  state text not null, expected_runs integer not null, imported_runs integer not null,
  error text, started_unix_ms integer not null, completed_unix_ms integer,
  primary key(source_path, source_schema_version)
);
create table legacy_run_import (
  source_path text not null, source_schema_version integer not null, legacy_kind text not null,
  legacy_run_id text not null, workflow_run_id text not null unique references workflow_run(id),
  record_digest text not null, imported_unix_ms integer not null,
  primary key(source_path, source_schema_version, legacy_kind, legacy_run_id)
);
create table workflow_cutover (
  id integer primary key check(id=1), completed_unix_ms integer not null,
  imported_sources integer not null, imported_runs integer not null
);
create trigger audit_step_update after update of state,outcome,blocker on workflow_step begin
  insert into run_event(run_id,step_id,kind,data_json,created_unix_ms)
  values(new.run_id,new.id,'step_changed',json_object('state',new.state,'outcome',new.outcome,'blocker',new.blocker),new.updated_unix_ms);
end;
create trigger audit_attempt_insert after insert on step_attempt begin
  insert into run_event(run_id,step_id,kind,data_json,created_unix_ms)
  values(new.run_id,new.step_id,'attempt_created',json_object('attempt_id',new.id,'ordinal',new.ordinal,'state',new.state),new.created_unix_ms);
end;
create trigger audit_attempt_update after update of state,terminal_reason on step_attempt begin
  insert into run_event(run_id,step_id,kind,data_json,created_unix_ms)
  values(new.run_id,new.step_id,'attempt_changed',json_object('attempt_id',new.id,'state',new.state,'reason',new.terminal_reason),new.updated_unix_ms);
end;
create trigger audit_artifact_insert after insert on artifact begin
  insert into run_event(run_id,step_id,kind,data_json,created_unix_ms)
  values(new.run_id,(select step_id from step_attempt where id=new.producer_attempt_id),'artifact_created',json_object('artifact_id',new.id,'revision',new.revision,'port',new.port,'digest',new.digest),new.created_unix_ms);
end;
create trigger audit_output_insert after insert on attempt_output begin
  insert into run_event(run_id,step_id,kind,data_json,created_unix_ms)
  select attempt.run_id,attempt.step_id,'attempt_output',json_object('attempt_id',new.attempt_id,'sequence',new.sequence,'stream',new.stream,'size',length(new.bytes),'truncated',new.truncated),new.created_unix_ms from step_attempt attempt where attempt.id=new.attempt_id;
end;
create trigger audit_gate_insert after insert on gate_result begin
  insert into run_event(run_id,step_id,kind,data_json,created_unix_ms)
  values(new.run_id,new.step_id,'gate_recorded',json_object('gate_result_id',new.id,'status',new.status,'subject_digest',new.subject_digest,'policy_revision',new.policy_revision),new.created_unix_ms);
end;
create trigger audit_effect_insert after insert on effect_intent begin
  insert into run_event(run_id,step_id,kind,data_json,created_unix_ms)
  values(new.run_id,new.step_id,'effect_prepared',json_object('effect_intent_id',new.id,'kind',new.kind,'state',new.state),new.created_unix_ms);
end;
create trigger audit_effect_update after update of state on effect_intent begin
  insert into run_event(run_id,step_id,kind,data_json,created_unix_ms)
  values(new.run_id,new.step_id,'effect_changed',json_object('effect_intent_id',new.id,'kind',new.kind,'state',new.state,'dispatch_generation',new.dispatch_generation),new.updated_unix_ms);
end;
create trigger audit_child_link_insert after insert on workflow_run_link begin
  insert into run_event(run_id,step_id,kind,data_json,created_unix_ms)
  values(new.parent_run_id,new.parent_step_id,'child_linked',json_object('child_run_id',new.child_run_id,'call_key',new.call_key,'propagation',new.propagation),unixepoch('subsec')*1000);
end;
"#;

const MIGRATION_V2: &str = r#"
create table if not exists legacy_migration_journal (
  source_path text not null, source_schema_version integer not null, source_digest text not null,
  state text not null, expected_runs integer not null, imported_runs integer not null,
  error text, started_unix_ms integer not null, completed_unix_ms integer,
  primary key(source_path, source_schema_version)
);
create table if not exists legacy_run_import (
  source_path text not null, source_schema_version integer not null, legacy_kind text not null,
  legacy_run_id text not null, workflow_run_id text not null unique references workflow_run(id),
  record_digest text not null, imported_unix_ms integer not null,
  primary key(source_path, source_schema_version, legacy_kind, legacy_run_id)
);
create table if not exists workflow_cutover (
  id integer primary key check(id=1), completed_unix_ms integer not null,
  imported_sources integer not null, imported_runs integer not null
);
"#;

const MIGRATION_V3: &str = r#"
create index if not exists workflow_run_history on workflow_run(updated_unix_ms desc, id);
create index if not exists workflow_run_attention on workflow_run(state, updated_unix_ms desc, id);
create index if not exists step_attempt_run on step_attempt(run_id, created_unix_ms, id);
create index if not exists step_attempt_claim on step_attempt(state, target_id, created_unix_ms, id);
create index if not exists attempt_output_retention on attempt_output(created_unix_ms, attempt_id);
create index if not exists artifact_run on artifact(run_id, created_unix_ms, id, revision);
create index if not exists approval_request_run on approval_request(run_id, state, updated_unix_ms, id);
create index if not exists effect_intent_recovery on effect_intent(state, updated_unix_ms, id);
create index if not exists workflow_step_run on workflow_step(run_id, state, id);
"#;

fn persist_pinned_snapshots(
    conn: &rusqlite::Connection,
    content: &crate::definition::SnapshotContent,
    now: i64,
) -> Result<(), String> {
    for (digest, child) in &content.pinned_snapshots {
        let bytes = serde_json::to_vec(child).map_err(|error| error.to_string())?;
        if sha256(&bytes) != *digest {
            return Err(format!(
                "pinned child Definition Snapshot '{}' failed digest verification",
                child.qualified_name
            ));
        }
        conn.execute(
            "insert or ignore into definition_snapshot (digest, schema_version, canonical_bytes, definition_name, created_unix_ms) values (?1, ?2, ?3, ?4, ?5)",
            params![digest, child.schema_version, bytes, child.qualified_name, now],
        )
        .map_err(sql_error)?;
        persist_pinned_snapshots(conn, child, now)?;
    }
    Ok(())
}

fn validate_inputs(snapshot: &DefinitionSnapshot, inputs: &[ArtifactInput]) -> Result<(), String> {
    let provided = inputs
        .iter()
        .map(|input| (input.name.as_str(), input))
        .collect::<BTreeMap<_, _>>();
    if provided.len() != inputs.len() {
        return Err("run inputs contain duplicate port names".to_string());
    }
    for (name, port) in &snapshot.content.inputs {
        match provided.get(name.as_str()) {
            None if port.required => return Err(format!("required run input '{name}' is missing")),
            Some(input) if input.artifact_type != port.artifact_type => {
                return Err(format!(
                    "run input '{name}' has type '{}', expected '{}'",
                    input.artifact_type, port.artifact_type
                ));
            }
            _ => {}
        }
    }
    for input in inputs {
        if !snapshot.content.inputs.contains_key(&input.name) {
            return Err(format!("run input '{}' is not declared", input.name));
        }
    }
    Ok(())
}

fn step_input_artifacts(
    conn: &rusqlite::Connection,
    run_id: &RunId,
    step_id: &StepId,
) -> Result<(String, Vec<(String, ArtifactRef)>), String> {
    let bindings_json: String = conn
        .query_row(
            "select input_bindings_json from workflow_step where id=?1 and run_id=?2",
            params![step_id.as_str(), run_id.as_str()],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    let bindings: BTreeMap<String, crate::definition::InputBinding> =
        serde_json::from_str(&bindings_json).map_err(|error| error.to_string())?;
    let mut artifacts = Vec::with_capacity(bindings.len());
    for (port, binding) in bindings {
        let (source_step, source_port) = binding
            .from
            .split_once('.')
            .ok_or_else(|| format!("invalid persisted input binding '{}'", binding.from))?;
        let row: Option<(String, u32, String, String)> = if source_step == "run" {
            conn.query_row(
                "select id,revision,digest,artifact_type from artifact where run_id=?1 and producer_attempt_id is null and port=?2 order by revision desc limit 1",
                params![run_id.as_str(), source_port],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .map_err(sql_error)?
        } else {
            conn.query_row(
                "select artifact.id,artifact.revision,artifact.digest,artifact.artifact_type from artifact join step_attempt attempt on attempt.id=artifact.producer_attempt_id join workflow_step step on step.id=attempt.step_id where artifact.run_id=?1 and step.definition_step_id=?2 and artifact.port=?3 and attempt.state='completed' order by artifact.created_unix_ms desc,artifact.revision desc limit 1",
                params![run_id.as_str(), source_step, source_port],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .map_err(sql_error)?
        };
        let (id, revision, digest, artifact_type) = row.ok_or_else(|| {
            format!(
                "Approval input '{port}' is not available from '{}'",
                binding.from
            )
        })?;
        if artifact_type != binding.artifact_type {
            return Err(format!(
                "Approval input '{port}' expected '{}' but persisted Artifact is '{artifact_type}'",
                binding.artifact_type
            ));
        }
        artifacts.push((
            port,
            ArtifactRef {
                id: ArtifactId(id),
                revision,
                digest,
                artifact_type,
            },
        ));
    }
    let digest = sha256(
        &serde_json::to_vec(
            &artifacts
                .iter()
                .map(|(port, artifact)| (port, artifact))
                .collect::<BTreeMap<_, _>>(),
        )
        .map_err(|error| error.to_string())?,
    );
    Ok((digest, artifacts))
}

pub(crate) fn input_digest(inputs: &[ArtifactInput]) -> Result<String, String> {
    canonical_input_bytes(inputs).map(|bytes| sha256(&bytes))
}

fn canonical_input_bytes(inputs: &[ArtifactInput]) -> Result<Vec<u8>, String> {
    let ordered = inputs
        .iter()
        .map(|input| (input.name.clone(), input))
        .collect::<BTreeMap<_, _>>();
    serde_json::to_vec(&ordered).map_err(|error| error.to_string())
}

fn insert_artifact(
    conn: &Connection,
    run_id: &RunId,
    attempt_id: Option<&AttemptId>,
    input: &ArtifactInput,
    now: i64,
) -> Result<ArtifactRef, String> {
    let bytes = serde_json::to_vec(&input.payload).map_err(|error| error.to_string())?;
    if bytes.len() > MAX_INLINE_ARTIFACT_BYTES {
        return Err(format!(
            "Artifact '{}' exceeds the inline payload limit",
            input.name
        ));
    }
    let id = ArtifactId(random_id(conn)?);
    let digest = sha256(&bytes);
    conn.execute("insert into artifact (id, revision, run_id, producer_attempt_id, port, artifact_type, schema_revision, digest, trust, sensitivity, payload_inline, size, created_unix_ms) values (?1, 1, ?2, ?3, ?4, ?5, 1, ?6, ?7, ?8, ?9, ?10, ?11)", params![id.as_str(), run_id.as_str(), attempt_id.map(AttemptId::as_str), input.name, input.artifact_type, digest, enum_json(&input.trust)?, enum_json(&input.sensitivity)?, bytes, bytes.len() as i64, now]).map_err(sql_error)?;
    Ok(ArtifactRef {
        id,
        revision: 1,
        digest,
        artifact_type: input.artifact_type.clone(),
    })
}

pub(crate) fn random_id(conn: &Connection) -> Result<String, String> {
    conn.query_row("select lower(hex(randomblob(16)))", [], |row| row.get(0))
        .map_err(sql_error)
}

fn load_attempt_summaries(
    conn: &Connection,
    run_id: &RunId,
) -> Result<Vec<AttemptSummary>, String> {
    let mut statement = conn.prepare("select id,step_id,ordinal,state,target_id,workspace_id,terminal_reason from step_attempt where run_id=?1 order by created_unix_ms,id").map_err(sql_error)?;
    statement
        .query_map([run_id.as_str()], |row| {
            Ok(AttemptSummary {
                id: AttemptId(row.get(0)?),
                step_id: StepId(row.get(1)?),
                ordinal: row.get(2)?,
                state: row.get(3)?,
                target_id: row.get(4)?,
                workspace_id: row.get::<_, Option<String>>(5)?.map(ExecutionWorkspaceId),
                terminal_reason: row.get(6)?,
            })
        })
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)
}

fn load_artifact_summaries(
    conn: &Connection,
    run_id: &RunId,
) -> Result<Vec<ArtifactSummary>, String> {
    let rows = {
        let mut statement = conn.prepare("select id,revision,digest,artifact_type,producer_attempt_id,port,trust,sensitivity,size from artifact where run_id=?1 order by created_unix_ms,id,revision").map_err(sql_error)?;
        statement
            .query_map([run_id.as_str()], |row| {
                Ok((
                    ArtifactRef {
                        id: ArtifactId(row.get(0)?),
                        revision: row.get(1)?,
                        digest: row.get(2)?,
                        artifact_type: row.get(3)?,
                    },
                    row.get::<_, Option<String>>(4)?.map(AttemptId),
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, i64>(8)?.max(0) as u64,
                ))
            })
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?
    };
    rows.into_iter()
        .map(|(artifact, producer_attempt_id, port, trust, sensitivity, size)| {
            let mut statement = conn.prepare("select source_artifact_id,source_revision,consumer_port from artifact_lineage where artifact_id=?1 and artifact_revision=?2 order by source_artifact_id,source_revision,consumer_port").map_err(sql_error)?;
            let sources = statement
                .query_map(params![artifact.id.as_str(), artifact.revision], |row| {
                    Ok(ArtifactLineageRef {
                        id: ArtifactId(row.get(0)?),
                        revision: row.get(1)?,
                        consumer_port: row.get(2)?,
                    })
                })
                .map_err(sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)?;
            Ok(ArtifactSummary {
                artifact,
                producer_attempt_id,
                port,
                trust,
                sensitivity,
                size,
                sources,
            })
        })
        .collect()
}

fn load_gate_results(conn: &Connection, run_id: &RunId) -> Result<Vec<GateResultSummary>, String> {
    let mut statement = conn
        .prepare("select id,step_id,attempt_id,subject_digest,subject_generation,evidence_quality,policy_revision,status,reason,expires_unix_ms,created_unix_ms from gate_result where run_id=?1 order by created_unix_ms,id")
        .map_err(sql_error)?;
    statement
        .query_map([run_id.as_str()], |row| {
            Ok(GateResultSummary {
                id: row.get(0)?,
                step_id: StepId(row.get(1)?),
                attempt_id: AttemptId(row.get(2)?),
                subject_digest: row.get(3)?,
                subject_generation: row.get(4)?,
                evidence_quality: row.get(5)?,
                policy_revision: row.get(6)?,
                status: row.get(7)?,
                reason: row.get(8)?,
                expires_unix_ms: row.get(9)?,
                created_unix_ms: row.get(10)?,
            })
        })
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)
}

fn load_effect_intents(
    conn: &Connection,
    run_id: &RunId,
) -> Result<Vec<EffectIntentSummary>, String> {
    let mut statement = conn
        .prepare("select id,step_id,attempt_id,kind,state,exact_head,reconciliation_key,dispatch_generation,result_json,created_unix_ms,updated_unix_ms from effect_intent where run_id=?1 order by created_unix_ms,id")
        .map_err(sql_error)?;
    statement
        .query_map([run_id.as_str()], |row| {
            Ok(EffectIntentSummary {
                id: EffectIntentId(row.get(0)?),
                step_id: StepId(row.get(1)?),
                attempt_id: AttemptId(row.get(2)?),
                kind: row.get(3)?,
                state: row.get(4)?,
                exact_head: row.get(5)?,
                reconciliation_key: row.get(6)?,
                dispatch_generation: row.get(7)?,
                result_json: row.get(8)?,
                created_unix_ms: row.get(9)?,
                updated_unix_ms: row.get(10)?,
            })
        })
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)
}

fn load_approval_projections(
    conn: &Connection,
    run_id: &RunId,
) -> Result<Vec<ApprovalProjection>, String> {
    let mut statement = conn.prepare("select r.id,r.step_id,r.attempt_id,r.mode,r.request_digest,r.input_digest,r.evidence_digest,r.expires_unix_ms,r.state,d.id,d.decision,d.actor,d.reason,d.decided_unix_ms from approval_request r left join approval_decision d on d.request_id=r.id where r.run_id=?1 order by r.created_unix_ms,r.id").map_err(sql_error)?;
    statement
        .query_map([run_id.as_str()], |row| {
            let request_id = ApprovalRequestId(row.get(0)?);
            let decision_id: Option<String> = row.get(9)?;
            Ok(ApprovalProjection {
                request: ApprovalRequest {
                    id: request_id.clone(),
                    run_id: run_id.clone(),
                    step_id: StepId(row.get(1)?),
                    attempt_id: AttemptId(row.get(2)?),
                    mode: parse_json(&row.get::<_, String>(3)?)?,
                    request_digest: row.get(4)?,
                    input_digest: row.get(5)?,
                    evidence_digest: row.get(6)?,
                    expires_unix_ms: row.get(7)?,
                    state: row.get(8)?,
                },
                decision: decision_id.map(|id| ApprovalDecision {
                    id: ApprovalDecisionId(id),
                    request_id,
                    approved: row
                        .get::<_, String>(10)
                        .is_ok_and(|value| value == "approve"),
                    actor: row.get(11).unwrap_or_default(),
                    reason: row.get(12).unwrap_or_default(),
                    decided_unix_ms: row.get(13).unwrap_or_default(),
                }),
            })
        })
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)
}

fn load_run_authority(conn: &Connection, run_id: &RunId) -> Result<AuthorityGrant, String> {
    conn.query_row("select g.id,g.capabilities_json,g.secret_scope_json,g.target_scope_json,g.expires_unix_ms from authority_grant g join workflow_run r on r.authority_grant_id=g.id where r.id=?1",[run_id.as_str()],|row|Ok(AuthorityGrant{id:AuthorityGrantId(row.get(0)?),capabilities:serde_json::from_str(&row.get::<_,String>(1)?).map_err(json_sql_error)?,secret_handles:serde_json::from_str(&row.get::<_,String>(2)?).map_err(json_sql_error)?,target_scope:serde_json::from_str(&row.get::<_,String>(3)?).map_err(json_sql_error)?,expires_unix_ms:row.get(4)?})).map_err(sql_error)
}

fn load_bounded_output(
    conn: &Connection,
    run_id: &RunId,
    byte_limit: usize,
) -> Result<Vec<AttemptOutputSummary>, String> {
    let mut statement = conn.prepare("select o.attempt_id,o.sequence,o.stream,o.bytes,o.truncated from attempt_output o join step_attempt a on a.id=o.attempt_id where a.run_id=?1 order by a.created_unix_ms,o.sequence").map_err(sql_error)?;
    let rows = statement
        .query_map([run_id.as_str()], |row| {
            Ok(AttemptOutputSummary {
                attempt_id: AttemptId(row.get(0)?),
                sequence: row.get(1)?,
                stream: row.get(2)?,
                bytes: row.get(3)?,
                truncated: row.get::<_, i64>(4)? != 0,
            })
        })
        .map_err(sql_error)?;
    let mut output = Vec::new();
    let mut remaining = byte_limit;
    for row in rows {
        let mut row = row.map_err(sql_error)?;
        if remaining == 0 {
            break;
        }
        if row.bytes.len() > remaining {
            row.bytes.truncate(remaining);
            row.truncated = true;
        }
        remaining -= row.bytes.len();
        output.push(row);
    }
    Ok(output)
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

pub(crate) fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("write String");
    }
    output
}

pub(crate) fn json(value: &impl Serialize) -> Result<String, String> {
    serde_json::to_string(value).map_err(|error| error.to_string())
}

fn enum_json(value: &impl Serialize) -> Result<String, String> {
    let value = serde_json::to_value(value).map_err(|error| error.to_string())?;
    value
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "enum did not serialize as text".to_string())
}

fn parse_json<T: for<'de> Deserialize<'de>>(value: &str) -> rusqlite::Result<T> {
    serde_json::from_str(&format!("\"{value}\"")).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })
}

fn json_sql_error(error: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}

fn run_summary_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunSummary> {
    Ok(RunSummary {
        id: RunId(row.get(0)?),
        definition: row.get(1)?,
        snapshot_digest: row.get(2)?,
        state: parse_run_state(&row.get::<_, String>(3)?)?,
        control: row.get(4)?,
        revision: row.get(5)?,
        created_unix_ms: row.get(6)?,
        updated_unix_ms: row.get(7)?,
    })
}

fn parse_run_state(value: &str) -> rusqlite::Result<RunState> {
    parse_json(value)
}
fn parse_step_state(value: &str) -> rusqlite::Result<StepState> {
    parse_json(value)
}

fn append_event(
    conn: &Connection,
    run_id: &RunId,
    step_id: Option<&StepId>,
    kind: &str,
    data: &serde_json::Value,
    now: i64,
) -> Result<(), String> {
    conn.execute("insert into run_event (run_id, step_id, kind, data_json, created_unix_ms) values (?1, ?2, ?3, ?4, ?5)", params![run_id.as_str(), step_id.map(StepId::as_str), kind, data.to_string(), now]).map_err(sql_error)?;
    Ok(())
}

pub(crate) fn recompute_run_state(
    conn: &Connection,
    run_id: &RunId,
    now: i64,
) -> Result<(), String> {
    let states = conn
        .prepare("select state from workflow_step where run_id = ?1")
        .and_then(|mut statement| {
            statement
                .query_map([run_id.as_str()], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()
        })
        .map_err(sql_error)?;
    let control: String = conn
        .query_row(
            "select control from workflow_run where id = ?1",
            [run_id.as_str()],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    let state = aggregate_run_state(&states, &control);
    conn.execute("update workflow_run set state = ?2, revision = revision + 1, updated_unix_ms = ?3 where id = ?1", params![run_id.as_str(), state.label(), now]).map_err(sql_error)?;
    Ok(())
}

fn aggregate_run_state(states: &[String], control: &str) -> RunState {
    if states.iter().any(|state| state == "recovery_required") {
        RunState::RecoveryRequired
    } else if control == "cancel_requested" {
        if states.iter().all(|state| {
            matches!(
                state.as_str(),
                "completed" | "failed" | "cancelled" | "skipped"
            )
        }) {
            RunState::Cancelled
        } else {
            RunState::Cancelling
        }
    } else if states.iter().any(|state| state == "cancelled") {
        RunState::Cancelled
    } else if states.iter().any(|state| state == "active") {
        RunState::Active
    } else if states.iter().any(|state| state == "input_required") {
        RunState::InputRequired
    } else if control == "pause_requested" || control == "paused" {
        RunState::Paused
    } else if states.iter().any(|state| state == "waiting") {
        RunState::Waiting
    } else if states
        .iter()
        .any(|state| matches!(state.as_str(), "pending" | "runnable" | "blocked"))
    {
        RunState::Queued
    } else if states.iter().any(|state| state == "failed") {
        RunState::Failed
    } else {
        RunState::Completed
    }
}

fn determining_steps(state: RunState, steps: &[StepSummary]) -> Vec<StepId> {
    steps
        .iter()
        .filter(|step| match state {
            RunState::RecoveryRequired => step.state == StepState::RecoveryRequired,
            RunState::Active => step.state == StepState::Active,
            RunState::InputRequired => step.state == StepState::InputRequired,
            RunState::Waiting => step.state == StepState::Waiting,
            RunState::Queued => matches!(
                step.state,
                StepState::Pending | StepState::Runnable | StepState::Blocked
            ),
            RunState::Failed => step.state == StepState::Failed,
            RunState::Cancelled => step.state == StepState::Cancelled,
            RunState::Completed => step.state == StepState::Completed,
            RunState::Paused => !matches!(step.state, StepState::Completed | StepState::Skipped),
            RunState::Cancelling => !matches!(
                step.state,
                StepState::Completed
                    | StepState::Failed
                    | StepState::Cancelled
                    | StepState::Skipped
            ),
        })
        .map(|step| step.id.clone())
        .collect()
}

fn sql_error(error: rusqlite::Error) -> String {
    error.to_string()
}

fn secure_parent(path: &Path) -> Result<(), String> {
    let created = !path.exists();
    fs::create_dir_all(path).map_err(|error| format!("create {}: {error}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if created {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                .map_err(|error| format!("secure {}: {error}", path.display()))?;
        }
    }
    Ok(())
}

fn secure_file(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("secure {}: {error}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::DefinitionCatalog;

    fn ledger() -> (RunLedger, PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "prism-workflow-ledger-{}-{}-{:?}.db",
            std::process::id(),
            now_ms(),
            std::thread::current().id()
        ));
        (RunLedger::open(path.clone()).unwrap(), path)
    }

    fn request(key: Option<&str>) -> StartRun {
        let snapshot = DefinitionCatalog::discover(None)
            .resolve("builtin:approval")
            .unwrap();
        StartRun {
            snapshot,
            repository_id: None,
            inputs: vec![ArtifactInput {
                name: "task".to_string(),
                artifact_type: "builtin:task@1".to_string(),
                payload: serde_json::json!({"title":"test"}),
                trust: TrustClass::Trusted,
                sensitivity: Sensitivity::Internal,
            }],
            idempotency_key: key.map(str::to_string),
            actor: "local:test".to_string(),
            actor_capabilities: BTreeSet::new(),
        }
    }

    #[test]
    fn start_is_transactional_and_idempotent() {
        let (ledger, path) = ledger();
        let first = ledger.start(request(Some("same"))).unwrap();
        let second = ledger.start(request(Some("same"))).unwrap();
        assert!(first.created);
        assert!(!second.created);
        assert_eq!(first.run_id, second.run_id);
        let mut changed = request(Some("same"));
        changed.inputs[0].payload = serde_json::json!({"title":"different"});
        assert!(
            ledger
                .start(changed)
                .unwrap_err()
                .contains("different inputs")
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn start_persists_reachable_child_snapshots() {
        let (ledger, path) = ledger();
        let snapshot = DefinitionCatalog::discover(None)
            .resolve("builtin:plan")
            .unwrap();
        let capabilities = snapshot.content.capabilities.clone();
        let child_digest = snapshot.content.pinned_workflows[0].digest.clone();
        ledger
            .start(StartRun {
                snapshot,
                repository_id: None,
                inputs: vec![ArtifactInput {
                    name: "task".into(),
                    artifact_type: "builtin:task@1".into(),
                    payload: serde_json::json!({"title":"plan"}),
                    trust: TrustClass::Trusted,
                    sensitivity: Sensitivity::Internal,
                }],
                idempotency_key: None,
                actor: "local:test".into(),
                actor_capabilities: capabilities,
            })
            .unwrap();
        let persisted: bool = ledger
            .connection()
            .unwrap()
            .query_row(
                "select exists(select 1 from definition_snapshot where digest=?1)",
                [child_digest],
                |row| row.get(0),
            )
            .unwrap();
        assert!(persisted);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn manual_start_rejects_duplicate_and_untrusted_inputs() {
        let (ledger, path) = ledger();
        let mut duplicate = request(None);
        duplicate.inputs.push(duplicate.inputs[0].clone());
        assert!(ledger.start(duplicate).unwrap_err().contains("duplicate"));
        let mut untrusted = request(None);
        untrusted.inputs[0].trust = TrustClass::Untrusted;
        assert!(ledger.start(untrusted).unwrap_err().contains("untrusted"));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn snapshot_survives_source_lifetime_and_approval_survives_reopen() {
        let (ledger, path) = ledger();
        let run = ledger.start(request(None)).unwrap();
        let projection = ledger.inspect(&run.run_id).unwrap();
        let step = projection.steps[0].id.clone();
        ledger
            .connection()
            .unwrap()
            .execute(
                "update workflow_step set state='runnable' where id=?1",
                [step.as_str()],
            )
            .unwrap();
        let approval_input = ledger.step_input_digest(&run.run_id, &step).unwrap();
        let approval = ledger
            .create_approval(
                &run.run_id,
                &step,
                ApprovalMode::ArtifactAcceptance,
                &approval_input,
                "evidence",
                None,
            )
            .unwrap();
        drop(ledger);
        let reopened = RunLedger::open(path.clone()).unwrap();
        reopened
            .decide_approval(
                &approval.id,
                &approval_input,
                "evidence",
                true,
                "local:test",
                None,
            )
            .unwrap();
        assert_eq!(
            reopened.inspect(&run.run_id).unwrap().run.state,
            RunState::Completed
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn changed_approval_evidence_invalidates_request() {
        let (ledger, path) = ledger();
        let run = ledger.start(request(None)).unwrap();
        let step = ledger.inspect(&run.run_id).unwrap().steps[0].id.clone();
        ledger
            .connection()
            .unwrap()
            .execute(
                "update workflow_step set state='runnable' where id=?1",
                [step.as_str()],
            )
            .unwrap();
        let approval_input = ledger.step_input_digest(&run.run_id, &step).unwrap();
        let approval = ledger
            .create_approval(
                &run.run_id,
                &step,
                ApprovalMode::HumanAttestation,
                &approval_input,
                "old",
                None,
            )
            .unwrap();
        assert!(
            ledger
                .decide_approval(
                    &approval.id,
                    &approval_input,
                    "new",
                    true,
                    "local:test",
                    None
                )
                .unwrap_err()
                .contains("changed")
        );
        assert_eq!(
            ledger.inspect(&run.run_id).unwrap().steps[0].state,
            StepState::Runnable
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn future_schema_is_rejected_without_fallback() {
        let path = std::env::temp_dir().join(format!(
            "prism-workflow-future-{}-{}-{:?}.db",
            std::process::id(),
            now_ms(),
            std::thread::current().id()
        ));
        let conn = storage::open_writable_connection(&path).unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        drop(conn);
        assert!(
            RunLedger::open(path.clone())
                .unwrap_err()
                .contains("future schema")
        );
        fs::remove_file(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn workflow_state_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let (ledger, path) = ledger();
        assert_eq!(
            fs::metadata(ledger.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&ledger.blob_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn health_detects_snapshot_digest_corruption() {
        let (ledger, path) = ledger();
        ledger.start(request(None)).unwrap();
        assert!(ledger.health().unwrap().integrity_ok);
        ledger
            .connection()
            .unwrap()
            .execute(
                "update definition_snapshot set canonical_bytes='corrupt'",
                [],
            )
            .unwrap();
        let health = ledger.health().unwrap();
        assert!(!health.integrity_ok);
        assert!(
            health
                .problems
                .iter()
                .any(|problem| problem.contains("Snapshot digest"))
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn retention_prunes_only_unreferenced_diagnostics_and_orphaned_blobs() {
        let (ledger, path) = ledger();
        let run = ledger.start(request(None)).unwrap();
        let conn = ledger.connection().unwrap();
        conn.execute(
            "update workflow_run set state='completed' where id=?1",
            [run.run_id.as_str()],
        )
        .unwrap();
        conn.execute(
            "insert into run_event(run_id,kind,data_json,created_unix_ms) values(?1,'attempt_output','{}',?2)",
            params![run.run_id.as_str(), now_ms() - 10],
        )
        .unwrap();
        let artifacts_before: i64 = conn
            .query_row("select count(*) from artifact", [], |row| row.get(0))
            .unwrap();
        drop(conn);
        fs::write(ledger.blob_dir.join("orphan"), b"orphan").unwrap();

        let report = ledger
            .apply_retention(RetentionPolicy {
                noisy_event_age_ms: 0,
                attempt_output_age_ms: 0,
                notification_age_ms: 0,
                provider_observation_age_ms: 0,
            })
            .unwrap();
        assert!(report.events_deleted >= 1);
        assert_eq!(report.blobs_deleted, 1);
        assert!(!ledger.blob_dir.join("orphan").exists());
        assert_eq!(
            ledger
                .connection()
                .unwrap()
                .query_row("select count(*) from artifact", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            artifacts_before
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn health_reports_orphaned_blob_files() {
        let (ledger, path) = ledger();
        fs::write(ledger.blob_dir.join("orphan"), b"orphan").unwrap();
        let health = ledger.health().unwrap();
        assert!(!health.integrity_ok);
        assert_eq!(health.orphaned_blobs, 1);
        fs::remove_file(ledger.blob_dir.join("orphan")).unwrap();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn concurrent_initializers_share_one_global_schema() {
        let path = std::env::temp_dir().join(format!(
            "prism-workflow-concurrent-{}-{}-{:?}.db",
            std::process::id(),
            now_ms(),
            std::thread::current().id()
        ));
        let threads = (0..4)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || RunLedger::open(path))
            })
            .collect::<Vec<_>>();
        for thread in threads {
            thread.join().unwrap().unwrap();
        }
        fs::remove_file(path).unwrap();
    }
}
