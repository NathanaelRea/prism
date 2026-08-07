use std::collections::BTreeMap;

use prism_extension_protocol::StepClass;
use sha2::{Digest, Sha256};
use sqlx::FromRow;

use super::error::DatabaseError;
use super::pools::WorkflowDatabase;

#[derive(Clone)]
pub(crate) struct RunLedger {
    database: WorkflowDatabase,
}

pub(crate) struct StartRun<'a> {
    pub run_id: &'a str,
    pub definition_snapshot_id: &'a str,
    pub repository: Option<&'a str>,
    pub idempotency_key: &'a str,
    pub input_json: &'a str,
    pub now_unix_ms: i64,
    pub paused: bool,
}

struct ChildLaunch<'a> {
    parent_step: &'a str,
    parent_run: &'a str,
    snapshot: &'a str,
    repository: Option<&'a str>,
    iteration: i64,
    input_json: &'a str,
    now: i64,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub(crate) struct MaterializedStep {
    pub id: String,
    pub key: String,
    pub implementation: String,
    pub target_id: String,
    pub input_json: String,
    pub dependencies: Vec<String>,
    pub resources: Vec<String>,
}

pub(crate) struct RegisterDefinition<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub revision: &'a str,
    pub source: &'a str,
    pub trusted: bool,
    pub body_json: &'a str,
    pub digest: &'a str,
    pub now_unix_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunCommand {
    Pause,
    Resume,
    Cancel,
    Retry,
}

pub(crate) struct RunProjection {
    pub id: String,
    pub definition_name: String,
    pub status: String,
    pub repository: Option<String>,
    pub created_unix_ms: i64,
    pub updated_unix_ms: i64,
    pub completed_unix_ms: Option<i64>,
    pub parent_run_id: Option<String>,
    pub lineage_root_id: Option<String>,
    pub archived_unix_ms: Option<i64>,
    pub detached: bool,
    pub attempt_budget: Option<i64>,
    pub attempts_consumed: i64,
    pub steps: Vec<StepProjection>,
    pub attempts: Vec<AttemptProjection>,
    pub artifacts: Vec<ArtifactProjection>,
    pub approvals: Vec<ApprovalProjection>,
    pub effects: Vec<EffectProjection>,
    pub gates: Vec<GateProjection>,
    pub events: Vec<AuditProjection>,
    pub children: Vec<ChildProjection>,
    pub authority: Vec<AuthorityProjection>,
}

pub(crate) struct HealthProjection {
    pub orphaned_definition_snapshots: i64,
    pub quarantined_workspaces: i64,
    pub indeterminate_effects: i64,
    pub recovery_required_runs: i64,
    pub invalid_child_links: i64,
    pub artifact_integrity_failures: Vec<ArtifactIntegrityProjection>,
}

pub(crate) struct ArtifactIntegrityProjection {
    pub artifact_id: String,
    pub reason: String,
}

#[derive(FromRow)]
struct ArtifactHealthRow {
    id: String,
    digest: String,
    size_bytes: i64,
    inline_body: Option<Vec<u8>>,
    file_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub(crate) struct ChildProjection {
    pub step_id: String,
    pub iteration: i64,
    pub run_id: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub(crate) struct AuthorityProjection {
    pub scope: String,
    pub granted_by: String,
    pub granted_unix_ms: i64,
    pub expires_unix_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub(crate) struct StepProjection {
    pub id: String,
    pub key: String,
    pub implementation: String,
    pub target_id: String,
    pub status: String,
    pub input_json: String,
    pub class: String,
    pub effect_boundary: String,
    pub skippable: bool,
    pub dependencies: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
struct StepRow {
    id: String,
    key: String,
    implementation: String,
    target_id: String,
    status: String,
    input_json: String,
    class: String,
    effect_boundary: String,
    skippable: bool,
}

#[derive(Debug, FromRow)]
struct RunPageRow {
    id: String,
    definition_name: String,
    status: String,
    repository: Option<String>,
    created_unix_ms: i64,
    updated_unix_ms: i64,
    completed_unix_ms: Option<i64>,
    parent_run_id: Option<String>,
    lineage_root_id: Option<String>,
    archived_unix_ms: Option<i64>,
    detached: bool,
    attempt_budget: Option<i64>,
    attempts_consumed: i64,
}

#[derive(Debug, FromRow)]
struct InspectRunRow {
    id: String,
    definition_name: String,
    status: String,
    repository: Option<String>,
    created_unix_ms: i64,
    updated_unix_ms: i64,
    completed_unix_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttemptProjection {
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
    pub input_revisions_json: String,
    pub bindings: Vec<AttemptBindingProjection>,
    pub output: Vec<OutputProjection>,
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub(crate) struct AttemptBindingProjection {
    pub attempt_id: String,
    pub name: String,
    pub schema_id: String,
    pub value_json: String,
    pub artifact_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
struct AttemptRow {
    id: String,
    step_id: String,
    status: String,
    worker_id: String,
    target_id: String,
    fencing_token: i64,
    process_id: Option<i64>,
    process_start_time_ticks: Option<i64>,
    started_unix_ms: i64,
    finished_unix_ms: Option<i64>,
    input_revisions_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub(crate) struct OutputProjection {
    pub sequence: i64,
    pub stream: String,
    pub body: Vec<u8>,
    pub time_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
struct AttemptOutputRow {
    attempt_id: String,
    sequence: i64,
    stream: String,
    body: Vec<u8>,
    time_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub(crate) struct ArtifactProjection {
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

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub(crate) struct ApprovalProjection {
    pub id: String,
    pub step_id: Option<String>,
    pub status: String,
    pub requested_unix_ms: i64,
    pub decided_unix_ms: Option<i64>,
    pub decided_by: Option<String>,
    pub decision_note: Option<String>,
    pub subject_json: Option<String>,
    pub evidence_json: Option<String>,
    pub policy_json: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub(crate) struct EffectProjection {
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

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub(crate) struct GateProjection {
    pub step_id: String,
    pub gate_kind: String,
    pub due_unix_ms: i64,
    pub checkpoint_json: String,
    pub poll_count: i64,
    pub subject_json: Option<String>,
    pub evidence_json: Option<String>,
    pub policy_json: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub(crate) struct AuditProjection {
    pub sequence: i64,
    pub step_id: Option<String>,
    pub attempt_id: Option<String>,
    pub kind: String,
    pub time_unix_ms: i64,
    pub data_json: String,
}

#[allow(
    dead_code,
    reason = "used by the generalized scheduler during workflow cutover"
)]
#[derive(Clone)]
pub(crate) struct Coordinator {
    database: WorkflowDatabase,
}

#[allow(
    dead_code,
    reason = "used by the generalized scheduler during workflow cutover"
)]
pub(crate) struct ClaimRequest<'a> {
    pub attempt_id: &'a str,
    pub step_id: &'a str,
    pub worker_id: &'a str,
    pub now_unix_ms: i64,
    pub lease_expires_unix_ms: i64,
}

#[allow(
    dead_code,
    reason = "used by the generalized scheduler during workflow cutover"
)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AttemptLease {
    pub attempt_id: String,
    pub step_id: String,
    pub worker_id: String,
    pub target_id: String,
    pub fencing_token: i64,
    pub lease_expires_unix_ms: i64,
}

#[allow(
    dead_code,
    reason = "used by the generalized scheduler during workflow cutover"
)]
pub(crate) struct AttemptResult<'a> {
    pub status: &'a str,
    pub result_json: &'a str,
    pub finished_unix_ms: i64,
}

impl RunLedger {
    pub(crate) fn new(database: WorkflowDatabase) -> Self {
        Self { database }
    }

    pub(crate) async fn register_definition(
        &self,
        command: RegisterDefinition<'_>,
    ) -> Result<(), DatabaseError> {
        let values = (
            command.id.to_string(),
            command.name.to_string(),
            command.revision.to_string(),
            command.source.to_string(),
            command.body_json.to_string(),
            command.digest.to_string(),
        );
        let trusted = command.trusted;
        let now_unix_ms = command.now_unix_ms;
        self.database.write_immediate(|connection| Box::pin(async move {
            let changed = sqlx::query("insert into definition_snapshot (id, definition_name, revision, source, trusted, body_json, digest, created_unix_ms) values (?, ?, ?, ?, ?, ?, ?, ?) on conflict(id) do nothing")
                .bind(&values.0).bind(&values.1).bind(&values.2).bind(&values.3).bind(trusted)
                .bind(&values.4).bind(&values.5).bind(now_unix_ms).execute(&mut *connection).await.map_err(DatabaseError::Query)?
                .rows_affected();
            if changed == 0 {
                let matches: i64 = sqlx::query_scalar("select exists(select 1 from definition_snapshot where id = ? and definition_name = ? and revision = ? and source = ? and trusted = ? and body_json = ? and digest = ?)")
                    .bind(&values.0).bind(&values.1).bind(&values.2).bind(&values.3).bind(trusted)
                    .bind(&values.4).bind(&values.5).fetch_one(connection).await.map_err(DatabaseError::Query)?;
                if matches != 1 {
                    return Err(DatabaseError::Conflict { operation: "register immutable definition snapshot" });
                }
            }
            Ok(())
        })).await
    }

    pub(crate) async fn definition_body(
        &self,
        definition_snapshot_id: &str,
    ) -> Result<String, DatabaseError> {
        sqlx::query_scalar("select body_json from definition_snapshot where id = ?")
            .bind(definition_snapshot_id)
            .fetch_one(self.database.readers())
            .await
            .map_err(DatabaseError::Query)
    }

    /// Advance idempotent Workflow Call steps without consuming a worker lease.
    pub(crate) async fn advance_children(&self, now_unix_ms: i64) -> Result<usize, DatabaseError> {
        #[derive(FromRow)]
        struct ChildStep {
            id: String,
            run_id: String,
            child_snapshot_id: String,
            input_json: String,
            repository: Option<String>,
            repeat_json: Option<String>,
        }
        let steps = sqlx::query_as::<_, ChildStep>("select step.id, step.run_id, step.child_snapshot_id, step.input_json, run.repository, step.repeat_json from workflow_step step join workflow_run run on run.id = step.run_id where step.class = 'workflow_call' and step.status = 'waiting' and step.runtime_status = 'waiting_child' order by step.id limit 64")
            .fetch_all(self.database.readers()).await.map_err(DatabaseError::Query)?;
        let mut changed = 0;
        for step in steps {
            let latest: Option<(i64, String, String)> = sqlx::query_as("select link.iteration, link.child_run_id, run.status from child_run_link link join workflow_run run on run.id = link.child_run_id where link.parent_step_id = ? order by link.iteration desc limit 1")
                .bind(&step.id).fetch_optional(self.database.readers()).await.map_err(DatabaseError::Query)?;
            let Some((iteration, child_id, child_status)) = latest else {
                self.launch_child_iteration(ChildLaunch {
                    parent_step: &step.id,
                    parent_run: &step.run_id,
                    snapshot: &step.child_snapshot_id,
                    repository: step.repository.as_deref(),
                    iteration: 1,
                    input_json: &step.input_json,
                    now: now_unix_ms,
                })
                .await?;
                changed += 1;
                continue;
            };
            match child_status.as_str() {
                "succeeded" => {
                    let child_outputs = self.workflow_outputs(&child_id).await?;
                    let repeat = step
                        .repeat_json
                        .as_deref()
                        .map(serde_json::from_str::<crate::workflow::definition::CompiledRepeat>)
                        .transpose()
                        .map_err(|error| DatabaseError::InvalidValue {
                            field: "workflow repeat",
                            value: error.to_string(),
                        })?;
                    let complete = repeat
                        .as_ref()
                        .is_none_or(|repeat| repeat_satisfied(repeat, &child_outputs));
                    if !complete
                        && repeat
                            .as_ref()
                            .is_some_and(|repeat| iteration < i64::from(repeat.max_iterations))
                    {
                        let repeat = repeat.expect("repeat exists when incomplete");
                        let mut next: serde_json::Map<String, serde_json::Value> =
                            serde_json::from_str::<serde_json::Value>(&step.input_json)
                                .ok()
                                .and_then(|value| value.as_object().cloned())
                                .unwrap_or_default();
                        for (input, output) in &repeat.successor {
                            if let Some((_, value, _)) = child_outputs.get(output) {
                                next.insert(input.clone(), value.clone());
                            }
                        }
                        let input_json = serde_json::Value::Object(next).to_string();
                        self.launch_child_iteration(ChildLaunch {
                            parent_step: &step.id,
                            parent_run: &step.run_id,
                            snapshot: &step.child_snapshot_id,
                            repository: step.repository.as_deref(),
                            iteration: iteration + 1,
                            input_json: &input_json,
                            now: now_unix_ms,
                        })
                        .await?;
                        changed += 1;
                        continue;
                    }
                    let id = step.id.clone();
                    let run = step.run_id.clone();
                    let exhausted = !complete;
                    let exhausted_policy = repeat.map(|repeat| repeat.on_exhausted);
                    let repeat_evidence = serde_json::Value::Object(
                        child_outputs
                            .iter()
                            .map(|(name, (_, value, _))| (name.clone(), value.clone()))
                            .collect(),
                    )
                    .to_string();
                    self.database.write_immediate(|connection| Box::pin(async move {
                        for (name, (schema, value, source_artifact_id)) in child_outputs {
                            let body = serde_json::to_vec(&value).map_err(|error| DatabaseError::InvalidValue { field: "workflow-call output", value: error.to_string() })?;
                            let artifact_id = format!("{id}:output:{name}");
                            let digest = crate::resource::ContentRevision::digest(&body).to_string();
                            sqlx::query("insert into artifact (id, run_id, revision, digest, size_bytes, sensitivity, inline_body, created_unix_ms) values (?, ?, 1, ?, ?, 'normal', ?, ?) on conflict(id) do nothing")
                                .bind(&artifact_id).bind(&run).bind(digest).bind(i64::try_from(body.len()).unwrap_or(i64::MAX)).bind(body).bind(now_unix_ms)
                                .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                            if let Some(source_artifact_id) = source_artifact_id {
                                sqlx::query("insert into artifact_lineage (artifact_id, parent_artifact_id) values (?, ?) on conflict do nothing")
                                    .bind(&artifact_id).bind(source_artifact_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                            }
                            sqlx::query("insert into step_output_binding (step_id, name, schema_id, value_json, source_run_id, artifact_id) values (?, ?, ?, ?, ?, ?) on conflict(step_id, name) do update set schema_id=excluded.schema_id, value_json=excluded.value_json, source_run_id=excluded.source_run_id, artifact_id=excluded.artifact_id")
                                .bind(&id).bind(name).bind(schema).bind(value.to_string()).bind(&child_id).bind(artifact_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                        }
                        let (status, runtime) = match (exhausted, exhausted_policy) {
                            (false, _) => ("succeeded", "succeeded"),
                            (true, Some(crate::workflow::definition::ExhaustedPolicy::Fail)) => ("failed", "failed"),
                            (true, Some(crate::workflow::definition::ExhaustedPolicy::Approval)) => ("waiting", "waiting_approval"),
                            _ => ("waiting", "input_required"),
                        };
                        sqlx::query("update workflow_step set status = ?, runtime_status = ? where id = ? and status = 'waiting'")
                            .bind(status).bind(runtime).bind(&id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                        if runtime == "waiting_approval" {
                            let approval_id = format!("{id}:repeat-exhausted");
                            sqlx::query("insert into approval_request (id,run_id,step_id,status,requested_unix_ms) values (?,?,?,'pending',?) on conflict(id) do nothing")
                                .bind(&approval_id).bind(&run).bind(&id).bind(now_unix_ms).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                            sqlx::query("insert into approval_evidence (approval_id,subject_json,evidence_json,policy_json) values (?,?,?,?) on conflict(approval_id) do nothing")
                                .bind(&approval_id).bind(serde_json::json!({"step_id": id, "reason": "repeat_exhausted"}).to_string())
                                .bind(&repeat_evidence).bind(serde_json::json!({"decision": "continue_after_exhaustion"}).to_string())
                                .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                        }
                        project_run_state(&mut *connection, &run, now_unix_ms).await?;
                        if runtime == "input_required" {
                            sqlx::query("update workflow_run set status='waiting', runtime_status='input_required', completed_unix_ms=null, updated_unix_ms=? where id=?")
                                .bind(now_unix_ms).bind(&run).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                            sqlx::query("insert into audit_event (run_id, step_id, sequence, kind, time_unix_ms, data_json) select ?, ?, coalesce(max(sequence),0)+1, 'input_required', ?, ? from audit_event where run_id=?")
                                .bind(&run).bind(&id).bind(now_unix_ms).bind(serde_json::json!({
                                    "reason":"iteration_budget_exhausted",
                                    "options":["additional_budget","open_agent_session","permitted_override","stop"]
                                }).to_string()).bind(&run).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                        }
                        Ok(())
                    })).await?;
                    changed += 1;
                }
                "failed" | "cancelled" | "recovery_required" => {
                    let status = child_status;
                    let id = step.id.clone();
                    let run = step.run_id.clone();
                    self.database.write_immediate(|connection| Box::pin(async move {
                        sqlx::query("update workflow_step set status = 'failed', runtime_status = ? where id = ? and status = 'waiting'")
                            .bind(&status).bind(&id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                        project_run_state(&mut *connection, &run, now_unix_ms).await
                    })).await?;
                    changed += 1;
                }
                _ => {}
            }
        }
        Ok(changed)
    }

    async fn launch_child_iteration(&self, launch: ChildLaunch<'_>) -> Result<(), DatabaseError> {
        let ChildLaunch {
            parent_step,
            parent_run,
            snapshot,
            repository,
            iteration,
            input_json,
            now,
        } = launch;
        let child_id = format!("{parent_step}:child:{iteration}");
        let key = format!("child:{parent_step}:{iteration}");
        self.start(StartRun {
            run_id: &child_id,
            definition_snapshot_id: snapshot,
            repository,
            idempotency_key: &key,
            input_json,
            now_unix_ms: now,
            paused: false,
        })
        .await?;
        let parent_run = parent_run.to_string();
        let parent_step = parent_step.to_string();
        self.database.write_immediate(|connection| Box::pin(async move {
            let root: String = sqlx::query_scalar("select coalesce(lineage_root_id, id) from workflow_run where id = ?").bind(&parent_run).fetch_one(&mut *connection).await.map_err(DatabaseError::Query)?;
            sqlx::query("update workflow_run set parent_run_id=?, parent_step_id=?, lineage_root_id=? where id=?").bind(&parent_run).bind(&parent_step).bind(root).bind(&child_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
            sqlx::query("insert into child_run_link (parent_run_id,parent_step_id,iteration,child_run_id) values (?,?,?,?) on conflict(parent_step_id,iteration) do nothing").bind(&parent_run).bind(&parent_step).bind(iteration).bind(&child_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
            sqlx::query("insert into authority_grant (id, run_id, scope, granted_by, granted_unix_ms, expires_unix_ms) select ? || ':authority:' || scope, ?, scope, 'delegated:' || granted_by, ?, expires_unix_ms from authority_grant where run_id=? and (expires_unix_ms is null or expires_unix_ms>?) on conflict(id) do nothing")
                .bind(&child_id).bind(&child_id).bind(now).bind(&parent_run).bind(now).execute(connection).await.map_err(DatabaseError::Query)?;
            Ok(())
        })).await
    }

    async fn workflow_outputs(
        &self,
        run_id: &str,
    ) -> Result<BTreeMap<String, (String, serde_json::Value, Option<String>)>, DatabaseError> {
        let body: String = sqlx::query_scalar("select snapshot.body_json from workflow_run run join definition_snapshot snapshot on snapshot.id=run.definition_snapshot_id where run.id=?").bind(run_id).fetch_one(self.database.readers()).await.map_err(DatabaseError::Query)?;
        let snapshot: crate::workflow::definition::DefinitionSnapshot = serde_json::from_str(&body)
            .map_err(|error| DatabaseError::InvalidValue {
                field: "child snapshot",
                value: error.to_string(),
            })?;
        let mut outputs = BTreeMap::new();
        for (name, port) in snapshot.definition.outputs {
            let Some(reference) = port.from else { continue };
            if let Some((value, artifact_id)) =
                load_run_reference_record(self.database.readers(), run_id, &reference).await?
            {
                outputs.insert(name, (port.schema, value, artifact_id));
            }
        }
        Ok(outputs)
    }

    pub(crate) async fn start(&self, command: StartRun<'_>) -> Result<String, DatabaseError> {
        let body = self.definition_body(command.definition_snapshot_id).await?;
        let snapshot: crate::workflow::definition::DefinitionSnapshot = serde_json::from_str(&body)
            .map_err(|error| DatabaseError::InvalidValue {
                field: "definition snapshot",
                value: error.to_string(),
            })?;
        self.start_snapshot(command, snapshot).await
    }

    pub(crate) async fn activate_paused(
        &self,
        run_id: &str,
        now_unix_ms: i64,
    ) -> Result<(), DatabaseError> {
        let run_id = run_id.to_string();
        self.database.write_immediate(|connection| Box::pin(async move {
            let status: Option<String> = sqlx::query_scalar("select status from workflow_run where id=?")
                .bind(&run_id).fetch_optional(&mut *connection).await.map_err(DatabaseError::Query)?;
            match status.as_deref() {
                Some("paused") => {
                    sqlx::query("update workflow_run set status='runnable',runtime_status='runnable',updated_unix_ms=? where id=? and status='paused'")
                        .bind(now_unix_ms).bind(&run_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                    sqlx::query("insert into audit_event (run_id,sequence,kind,time_unix_ms,data_json) select ?,coalesce(max(sequence),0)+1,'run_activated_after_admission',?,'{}' from audit_event where run_id=?")
                        .bind(&run_id).bind(now_unix_ms).bind(&run_id).execute(connection).await.map_err(DatabaseError::Query)?;
                    Ok(())
                }
                Some("waiting" | "runnable" | "running" | "succeeded") => Ok(()),
                Some(_) => Err(DatabaseError::Conflict { operation: "activate admitted Workflow Run" }),
                None => Err(DatabaseError::Conflict { operation: "activate missing admitted Workflow Run" }),
            }
        })).await
    }

    async fn start_snapshot(
        &self,
        command: StartRun<'_>,
        snapshot: crate::workflow::definition::DefinitionSnapshot,
    ) -> Result<String, DatabaseError> {
        let run_id = command.run_id.to_string();
        let definition_snapshot_id = command.definition_snapshot_id.to_string();
        let repository = command.repository.map(str::to_string);
        let idempotency_key = command.idempotency_key.to_string();
        let input_json = command.input_json.to_string();
        let now = command.now_unix_ms;
        let initial_status = if command.paused { "paused" } else { "runnable" };
        let inputs: serde_json::Value =
            serde_json::from_str(&input_json).map_err(|error| DatabaseError::InvalidValue {
                field: "workflow inputs",
                value: error.to_string(),
            })?;
        let input_object =
            inputs
                .as_object()
                .cloned()
                .ok_or_else(|| DatabaseError::InvalidValue {
                    field: "workflow inputs",
                    value: "expected a JSON object".into(),
                })?;
        for (name, port) in &snapshot.definition.inputs {
            if port.required && !input_object.contains_key(name) {
                return Err(DatabaseError::InvalidValue {
                    field: "workflow inputs",
                    value: format!("missing required input {name}"),
                });
            }
        }
        let steps = snapshot.definition.steps.clone();
        let children = snapshot.children.clone();
        let definition_name = snapshot.definition.name.clone();
        let attempt_budget = snapshot.definition.budgets.max_attempts.map(i64::from);
        self.database.write_immediate(|connection| Box::pin(async move {
            // Child snapshots are retained before links are materialized. They are immutable and
            // content-addressed, so repeated registration is harmless.
            for child in children.values() {
                let body = serde_json::to_string(child).map_err(|error| DatabaseError::InvalidValue {
                    field: "child definition snapshot", value: error.to_string(),
                })?;
                sqlx::query("insert into definition_snapshot (id, definition_name, revision, source, trusted, body_json, digest, created_unix_ms) values (?, ?, ?, 'snapshot-child', ?, ?, ?, ?) on conflict(id) do nothing")
                    .bind(&child.digest).bind(&child.definition.name)
                    .bind(child.sources.get(&child.definition.id).map(|source| source.revision.as_str()).unwrap_or("snapshot"))
                    .bind(child.trusted).bind(body).bind(&child.digest).bind(now)
                    .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
            }
            let changed = sqlx::query("insert into workflow_run (id, definition_snapshot_id, repository, status, created_unix_ms, updated_unix_ms, input_json, lineage_root_id, attempt_budget, runtime_status) select ?, ?, ?, ?, ?, ?, ?, ?, ?, ? where not exists (select 1 from idempotency_record where scope = 'manual_invocation' and key = ?)")
                .bind(&run_id).bind(&definition_snapshot_id).bind(&repository).bind(initial_status).bind(now).bind(now)
                .bind(&input_json).bind(&run_id).bind(attempt_budget).bind(initial_status).bind(&idempotency_key)
                .execute(&mut *connection).await.map_err(DatabaseError::Query)?.rows_affected();
            if changed == 0 {
                return sqlx::query_scalar("select result_id from idempotency_record where scope = 'manual_invocation' and key = ?")
                    .bind(&idempotency_key).fetch_one(connection).await.map_err(DatabaseError::Query);
            }
            sqlx::query("insert into idempotency_record (scope, key, result_kind, result_id, created_unix_ms) values ('manual_invocation', ?, 'run', ?, ?)")
                .bind(&idempotency_key).bind(&run_id).bind(now).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
            sqlx::query("insert into audit_event (run_id, sequence, kind, time_unix_ms, data_json) values (?, 1, 'run_started', ?, ?)")
                .bind(&run_id).bind(now).bind(serde_json::json!({"definition": definition_name}).to_string())
                .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
            for (name, port) in &snapshot.definition.inputs {
                let Some(value) = input_object.get(name) else { continue };
                let body = serde_json::to_vec(value).map_err(|error| DatabaseError::InvalidValue { field: "workflow input", value: error.to_string() })?;
                let artifact_id = format!("{run_id}:input:{name}");
                let digest = crate::resource::ContentRevision::digest(&body).to_string();
                sqlx::query("insert into artifact (id, run_id, revision, digest, size_bytes, sensitivity, inline_body, created_unix_ms) values (?, ?, 1, ?, ?, 'normal', ?, ?)")
                    .bind(&artifact_id).bind(&run_id).bind(digest).bind(i64::try_from(body.len()).unwrap_or(i64::MAX)).bind(body).bind(now)
                    .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                sqlx::query("insert into workflow_input_binding (run_id, name, schema_id, artifact_id, revision) values (?, ?, ?, ?, 1)")
                    .bind(&run_id).bind(name).bind(&port.schema).bind(artifact_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
            }
            for step in &steps {
                let step_id = format!("{run_id}:step:{}", step.id);
                let implementation = step.implementation.clone().unwrap_or_else(|| "__workflow_call__".into());
                let class = step_class_name(step.class);
                let target = step.target.as_deref().unwrap_or("local");
                let child_snapshot_id = step.workflow.as_ref().and_then(|id| children.get(id)).map(|child| child.digest.as_str());
                let input = resolve_launch_bindings(&step.inputs, &input_object, &snapshot.definition.parameters)?;
                // Every step passes through readiness resolution, including roots. This is where
                // exact input artifact revisions are frozen for the eventual attempt.
                let status = "waiting";
                sqlx::query("insert into workflow_step (id, run_id, step_key, implementation, target_id, status, available_unix_ms, input_json, class, bindings_json, outputs_json, settings_json, condition_json, on_unknown, skippable, retry_max_attempts, child_snapshot_id, runtime_status, repeat_json, effect_boundary) values (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")
                    .bind(&step_id).bind(&run_id).bind(&step.id).bind(implementation).bind(target).bind(status).bind(now)
                    .bind(input.to_string()).bind(class)
                    .bind(serde_json::to_string(&step.inputs).unwrap_or_else(|_| "{}".into()))
                    .bind(serde_json::to_string(&step.outputs).unwrap_or_else(|_| "{}".into()))
                    .bind(serde_json::to_string(&step.settings).unwrap_or_else(|_| "{}".into()))
                    .bind(step.condition.as_ref().map(|value| serde_json::to_string(value).unwrap_or_default()))
                    .bind(unknown_policy_name(step.on_unknown)).bind(step.skippable).bind(i64::from(step.retry.max_attempts))
                    .bind(child_snapshot_id).bind(status)
                    .bind(step.repeat.as_ref().map(|repeat| serde_json::to_string(repeat).unwrap_or_default()))
                    .bind(effect_boundary_name(step.effect_boundary))
                    .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                for resource in &step.resources {
                    sqlx::query("insert into step_resource_requirement (step_id, resource_key) values (?, ?)")
                        .bind(&step_id).bind(resource).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                }
            }
            for step in &steps {
                let step_id = format!("{run_id}:step:{}", step.id);
                for dependency in &step.dependencies {
                    sqlx::query("insert into step_dependency (step_id, depends_on_step_id) values (?, ?)")
                        .bind(&step_id).bind(format!("{run_id}:step:{dependency}"))
                        .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                }
            }
            Ok(run_id)
        })).await
    }

    pub(crate) async fn start_materialized(
        &self,
        command: StartRun<'_>,
        steps: Vec<MaterializedStep>,
    ) -> Result<String, DatabaseError> {
        let run_id = command.run_id.to_string();
        let definition_snapshot_id = command.definition_snapshot_id.to_string();
        let repository = command.repository.map(str::to_string);
        let idempotency_key = command.idempotency_key.to_string();
        let now_unix_ms = command.now_unix_ms;
        let input_json = command.input_json.to_string();
        self.database.write_immediate(|connection| Box::pin(async move {
            let changed = sqlx::query_file!(
                "sql/workflow_ledger/start_run.sql",
                run_id,
                definition_snapshot_id,
                repository,
                now_unix_ms,
                now_unix_ms,
                idempotency_key
            )
                .execute(&mut *connection).await.map_err(DatabaseError::Query)?.rows_affected();
            if changed == 1 {
                sqlx::query("update workflow_run set input_json = ?, lineage_root_id = ? where id = ?")
                    .bind(input_json).bind(&run_id).bind(&run_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                sqlx::query("insert into idempotency_record (scope, key, result_kind, result_id, created_unix_ms) values ('manual_invocation', ?, 'run', ?, ?)")
                    .bind(&idempotency_key).bind(&run_id).bind(now_unix_ms)
                    .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                sqlx::query("insert into audit_event (run_id, sequence, kind, time_unix_ms, data_json) values (?, 1, 'run_started', ?, '{}')")
                    .bind(&run_id).bind(now_unix_ms).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                for step in &steps {
                    sqlx::query("insert into workflow_step (id, run_id, step_key, implementation, target_id, status, available_unix_ms, input_json, runtime_status) values (?, ?, ?, ?, ?, 'runnable', ?, ?, 'runnable')")
                        .bind(&step.id).bind(&run_id).bind(&step.key).bind(&step.implementation).bind(&step.target_id)
                        .bind(now_unix_ms).bind(&step.input_json).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                    for resource in &step.resources {
                        sqlx::query("insert into step_resource_requirement (step_id, resource_key) values (?, ?)")
                            .bind(&step.id).bind(resource).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                    }
                }
                for step in &steps {
                    for dependency in &step.dependencies {
                        sqlx::query("insert into step_dependency (step_id, depends_on_step_id) values (?, ?)")
                            .bind(&step.id).bind(dependency).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                    }
                }
                if !steps.is_empty() {
                    sqlx::query("update workflow_run set status = 'runnable' where id = ?")
                        .bind(&run_id).execute(connection).await.map_err(DatabaseError::Query)?;
                }
                Ok(run_id)
            } else {
                sqlx::query_scalar("select result_id from idempotency_record where scope = 'manual_invocation' and key = ?")
                    .bind(&idempotency_key).fetch_one(connection).await.map_err(DatabaseError::Query)
            }
        })).await
    }

    pub(crate) async fn command(
        &self,
        run_id: &str,
        command: RunCommand,
        now_unix_ms: i64,
    ) -> Result<(), DatabaseError> {
        let run_id = run_id.to_string();
        self.database
            .write_immediate(|connection| {
                Box::pin(async move {
                    let changed = match command {
                        RunCommand::Pause => {
                            sqlx::query("update workflow_run set status = 'paused', runtime_status = 'paused', updated_unix_ms = ? where id = ? and status in ('waiting','runnable','running')")
                                .bind(now_unix_ms).bind(&run_id).execute(&mut *connection).await
                        }
                        RunCommand::Resume => {
                            sqlx::query("update workflow_run set status = case when exists (select 1 from workflow_step step join step_attempt attempt on attempt.step_id = step.id where step.run_id = workflow_run.id and attempt.status = 'claimed') then 'running' else 'runnable' end, runtime_status = case when exists (select 1 from workflow_step step join step_attempt attempt on attempt.step_id = step.id where step.run_id = workflow_run.id and attempt.status = 'claimed') then 'running' else 'runnable' end, updated_unix_ms = ? where id = ? and status = 'paused'")
                                .bind(now_unix_ms).bind(&run_id).execute(&mut *connection).await
                        }
                        RunCommand::Cancel => {
                            let result = sqlx::query("update workflow_run set status = 'cancelled', runtime_status = 'cancelled', updated_unix_ms = ?, completed_unix_ms = ? where id = ? and status in ('waiting','runnable','running','paused','recovery_required')")
                                .bind(now_unix_ms).bind(now_unix_ms).bind(&run_id).execute(&mut *connection).await;
                            if result.as_ref().is_ok_and(|result| result.rows_affected() == 1) {
                                sqlx::query("update workflow_step set status = 'cancelled', runtime_status = 'cancelled' where run_id = ? and status in ('waiting','runnable')")
                                    .bind(&run_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                            }
                            result
                        }
                        RunCommand::Retry => {
                            let result = sqlx::query("update workflow_run set status = 'runnable', runtime_status = 'runnable', updated_unix_ms = ?, completed_unix_ms = null where id = ? and status in ('failed','recovery_required')")
                                .bind(now_unix_ms).bind(&run_id).execute(&mut *connection).await;
                            if result.as_ref().is_ok_and(|result| result.rows_affected() == 1) {
                                sqlx::query("update workflow_step set status = 'runnable', runtime_status = 'runnable', available_unix_ms = ? where run_id = ? and (status in ('failed','cancelled') or (status = 'claimed' and exists (select 1 from step_attempt attempt where attempt.step_id = workflow_step.id and attempt.status = 'recovery_required')))")
                                    .bind(now_unix_ms).bind(&run_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                            }
                            result
                        }
                    }
                    .map_err(DatabaseError::Query)?
                    .rows_affected();
                    if changed != 1 {
                        return Err(DatabaseError::Conflict { operation: "command workflow run" });
                    }
                    match command {
                        RunCommand::Pause => {
                            sqlx::query("with recursive descendants(id) as (select id from workflow_run where parent_run_id = ? and detached = 0 union all select run.id from workflow_run run join descendants parent on run.parent_run_id = parent.id where run.detached = 0) update workflow_run set status = 'paused', runtime_status = 'paused', updated_unix_ms = ? where id in (select id from descendants) and status in ('waiting','runnable','running')")
                                .bind(&run_id).bind(now_unix_ms).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                        }
                        RunCommand::Resume => {
                            sqlx::query("with recursive descendants(id) as (select id from workflow_run where parent_run_id = ? and detached = 0 union all select run.id from workflow_run run join descendants parent on run.parent_run_id = parent.id where run.detached = 0) update workflow_run set status = 'runnable', runtime_status = 'runnable', updated_unix_ms = ? where id in (select id from descendants) and status = 'paused'")
                                .bind(&run_id).bind(now_unix_ms).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                        }
                        RunCommand::Cancel => {
                            sqlx::query("with recursive descendants(id) as (select id from workflow_run where parent_run_id = ? and detached = 0 union all select run.id from workflow_run run join descendants parent on run.parent_run_id = parent.id where run.detached = 0) update workflow_run set status = 'cancelled', runtime_status = 'cancelled', updated_unix_ms = ?, completed_unix_ms = ? where id in (select id from descendants) and status not in ('succeeded','failed','cancelled')")
                                .bind(&run_id).bind(now_unix_ms).bind(now_unix_ms).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                        }
                        RunCommand::Retry => {}
                    }
                    let kind = match command {
                        RunCommand::Pause => "run_paused",
                        RunCommand::Resume => "run_resumed",
                        RunCommand::Cancel => "run_cancelled",
                        RunCommand::Retry => "run_retried",
                    };
                    sqlx::query("insert into audit_event (run_id, sequence, kind, time_unix_ms, data_json) select ?, coalesce(max(sequence), 0) + 1, ?, ?, '{}' from audit_event where run_id = ?")
                        .bind(&run_id).bind(kind).bind(now_unix_ms).bind(&run_id)
                        .execute(connection).await.map_err(DatabaseError::Query)?;
                    Ok(())
                })
            })
            .await
    }

    pub(crate) async fn control_target(
        &self,
        run_id: &str,
        scope: crate::workflow::operations::WorkflowControlScope,
    ) -> Result<String, DatabaseError> {
        let column = match scope {
            crate::workflow::operations::WorkflowControlScope::Run => return Ok(run_id.to_string()),
            crate::workflow::operations::WorkflowControlScope::Parent => "parent_run_id",
            crate::workflow::operations::WorkflowControlScope::Lineage => "lineage_root_id",
        };
        let query = format!("select {column} from workflow_run where id = ?");
        sqlx::query_scalar(&query)
            .bind(run_id)
            .fetch_optional(self.database.readers())
            .await
            .map_err(DatabaseError::Query)?
            .flatten()
            .ok_or(DatabaseError::Conflict {
                operation: "resolve workflow control scope",
            })
    }

    pub(crate) async fn set_detached(
        &self,
        child_run_id: &str,
        detached: bool,
        now: i64,
    ) -> Result<(), DatabaseError> {
        let child_run_id = child_run_id.to_string();
        self.database.write_immediate(|connection| Box::pin(async move {
            let changed = sqlx::query("update workflow_run set detached=?, updated_unix_ms=? where id=? and parent_run_id is not null")
                .bind(detached).bind(now).bind(child_run_id).execute(connection).await.map_err(DatabaseError::Query)?.rows_affected();
            if changed == 1 { Ok(()) } else { Err(DatabaseError::Conflict { operation: "set child detachment" }) }
        })).await
    }

    pub(crate) async fn restart_from_step(
        &self,
        run_id: &str,
        step_key: &str,
        now: i64,
    ) -> Result<(), DatabaseError> {
        let run_id = run_id.to_string();
        let step_key = step_key.to_string();
        self.database.write_immediate(|connection| Box::pin(async move {
            let root: Option<String> = sqlx::query_scalar("select id from workflow_step where run_id = ? and step_key = ?")
                .bind(&run_id).bind(&step_key).fetch_optional(&mut *connection).await.map_err(DatabaseError::Query)?;
            let Some(root) = root else { return Err(DatabaseError::Conflict { operation: "restart from workflow step" }) };
            sqlx::query("with recursive affected(id) as (select ? union all select dependency.step_id from step_dependency dependency join affected on dependency.depends_on_step_id = affected.id) update workflow_step set status = 'waiting', runtime_status = 'waiting', invalidated_unix_ms = ?, available_unix_ms = ? where id in (select id from affected) and status <> 'claimed'")
                .bind(&root).bind(now).bind(now).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
            sqlx::query("with recursive affected(id) as (select ? union all select dependency.step_id from step_dependency dependency join affected on dependency.depends_on_step_id = affected.id) delete from step_output_binding where step_id in (select id from affected)")
                .bind(&root).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
            sqlx::query("update workflow_run set status = 'runnable', runtime_status = 'runnable', completed_unix_ms = null, updated_unix_ms = ? where id = ?")
                .bind(now).bind(&run_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
            Ok(())
        })).await
    }

    pub(crate) async fn skip_step(
        &self,
        run_id: &str,
        step_key: &str,
        now: i64,
    ) -> Result<(), DatabaseError> {
        let run_id = run_id.to_string();
        let step_key = step_key.to_string();
        self.database.write_immediate(|connection| Box::pin(async move {
            let step_id: Option<String> = sqlx::query_scalar("update workflow_step set status = 'succeeded', runtime_status = 'skipped', invalidated_unix_ms=? where run_id = ? and step_key = ? and skippable = 1 and status in ('waiting','runnable','failed') returning id")
                .bind(now).bind(&run_id).bind(&step_key).fetch_optional(&mut *connection).await.map_err(DatabaseError::Query)?;
            let Some(step_id) = step_id else { return Err(DatabaseError::Conflict { operation: "skip workflow step" }) };
            sqlx::query("delete from step_output_binding where step_id=?")
                .bind(&step_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
            sqlx::query("with recursive affected(id) as (select step_id from step_dependency where depends_on_step_id = ? union all select dependency.step_id from step_dependency dependency join affected on dependency.depends_on_step_id = affected.id) update workflow_step set status = 'waiting', runtime_status = 'waiting', invalidated_unix_ms = ? where id in (select id from affected) and status <> 'claimed'")
                .bind(&step_id).bind(now).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
            project_run_state(connection, &run_id, now).await
        })).await
    }

    pub(crate) async fn archive(&self, run_id: &str, now: i64) -> Result<(), DatabaseError> {
        let run_id = run_id.to_string();
        self.database.write_immediate(|connection| Box::pin(async move {
            let changed = sqlx::query("update workflow_run set archived_unix_ms = ?, runtime_status = 'archived', updated_unix_ms = ? where id = ? and status in ('succeeded','failed','cancelled') and archived_unix_ms is null")
                .bind(now).bind(now).bind(&run_id).execute(connection).await.map_err(DatabaseError::Query)?.rows_affected();
            if changed == 1 { Ok(()) } else { Err(DatabaseError::Conflict { operation: "archive workflow run" }) }
        })).await
    }

    pub(crate) async fn quarantine_workspace(
        &self,
        workspace_id: &str,
        reason: &str,
        now: i64,
    ) -> Result<(), DatabaseError> {
        if reason.trim().is_empty() {
            return Err(DatabaseError::InvalidValue {
                field: "workspace quarantine reason",
                value: reason.into(),
            });
        }
        let workspace_id = workspace_id.to_string();
        let reason = reason.to_string();
        self.database.write_immediate(|connection| Box::pin(async move {
            let changed = sqlx::query("update execution_workspace set state='quarantined', quarantine_reason=?, updated_unix_ms=? where id=? and state<>'released'")
                .bind(&reason).bind(now).bind(&workspace_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?.rows_affected();
            if changed != 1 {
                return Err(DatabaseError::Conflict { operation: "quarantine execution workspace" });
            }
            sqlx::query("update workflow_step set status='waiting', runtime_status='recovery_required' where id in (select requirement.step_id from step_resource_requirement requirement where requirement.resource_key=? or requirement.resource_key='workspace:' || ?) and status in ('waiting','runnable')")
                .bind(&workspace_id).bind(&workspace_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
            let run_id: String = sqlx::query_scalar("select run_id from execution_workspace where id=?")
                .bind(&workspace_id).fetch_one(&mut *connection).await.map_err(DatabaseError::Query)?;
            sqlx::query("update workflow_run set status='recovery_required', runtime_status='recovery_required', updated_unix_ms=? where id=? and status not in ('succeeded','failed','cancelled')")
                .bind(now).bind(&run_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
            Ok(())
        })).await
    }

    pub(crate) async fn add_attempt_budget(
        &self,
        run_id: &str,
        additional: u32,
        now: i64,
    ) -> Result<(), DatabaseError> {
        if additional == 0 {
            return Err(DatabaseError::InvalidValue {
                field: "additional attempt budget",
                value: "0".into(),
            });
        }
        let run_id = run_id.to_string();
        self.database.write_immediate(|connection| Box::pin(async move {
            let root: Option<String> = sqlx::query_scalar("select coalesce(lineage_root_id, id) from workflow_run where id = ? and runtime_status = 'input_required'")
                .bind(&run_id).fetch_optional(&mut *connection).await.map_err(DatabaseError::Query)?;
            let Some(root) = root else { return Err(DatabaseError::Conflict { operation: "resolve input-required workflow" }) };
            sqlx::query("update workflow_run set attempt_budget = coalesce(attempt_budget, attempts_consumed) + ?, runtime_status = 'runnable', status = 'runnable', updated_unix_ms = ? where id = ?")
                .bind(i64::from(additional)).bind(now).bind(&root).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
            sqlx::query("update workflow_run set runtime_status='runnable', status='runnable', updated_unix_ms=? where id=?")
                .bind(now).bind(&run_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
            let input_steps: Vec<(String, Option<String>)> = sqlx::query_as("select id,repeat_json from workflow_step where run_id=? and runtime_status='input_required'")
                .bind(&run_id).fetch_all(&mut *connection).await.map_err(DatabaseError::Query)?;
            for (step_id, repeat_json) in input_steps {
                let repeat_json = repeat_json.map(|body| {
                    let mut repeat: crate::workflow::definition::CompiledRepeat = serde_json::from_str(&body)
                        .map_err(|error| DatabaseError::InvalidValue { field: "workflow repeat", value: error.to_string() })?;
                    repeat.max_iterations = repeat.max_iterations.saturating_add(additional);
                    serde_json::to_string(&repeat).map_err(|error| DatabaseError::InvalidValue { field: "workflow repeat", value: error.to_string() })
                }).transpose()?;
                sqlx::query("update workflow_step set status='waiting', runtime_status='waiting', repeat_json=coalesce(?,repeat_json), available_unix_ms=? where id=?")
                    .bind(repeat_json).bind(now).bind(step_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
            }
            Ok(())
        })).await
    }

    pub(crate) async fn list(
        &self,
        repository: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RunProjection>, DatabaseError> {
        self.list_page(repository, 0, limit).await
    }

    pub(crate) async fn list_page(
        &self,
        repository: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<RunProjection>, DatabaseError> {
        let limit = i64::try_from(limit).map_err(|_| DatabaseError::InvalidValue {
            field: "workflow list limit",
            value: limit.to_string(),
        })?;
        if limit <= 0 || limit > 256 {
            return Err(DatabaseError::InvalidValue {
                field: "workflow list limit",
                value: limit.to_string(),
            });
        }
        let offset = i64::try_from(offset).map_err(|_| DatabaseError::InvalidValue {
            field: "workflow list offset",
            value: offset.to_string(),
        })?;
        let rows = sqlx::query_as::<_, RunPageRow>(include_str!(
            "../../sql/workflow_ledger/list_run_page.sql"
        ))
        .bind(repository)
        .bind(repository)
        .bind(limit)
        .bind(offset)
        .fetch_all(self.database.readers())
        .await
        .map_err(DatabaseError::Query)?;
        Ok(rows
            .into_iter()
            .map(|row| RunProjection {
                id: row.id,
                definition_name: row.definition_name,
                status: row.status,
                repository: row.repository,
                created_unix_ms: row.created_unix_ms,
                updated_unix_ms: row.updated_unix_ms,
                completed_unix_ms: row.completed_unix_ms,
                parent_run_id: row.parent_run_id,
                lineage_root_id: row.lineage_root_id,
                archived_unix_ms: row.archived_unix_ms,
                detached: row.detached,
                attempt_budget: row.attempt_budget,
                attempts_consumed: row.attempts_consumed,
                steps: Vec::new(),
                attempts: Vec::new(),
                artifacts: Vec::new(),
                approvals: Vec::new(),
                effects: Vec::new(),
                gates: Vec::new(),
                events: Vec::new(),
                children: Vec::new(),
                authority: Vec::new(),
            })
            .collect())
    }

    pub(crate) async fn inspect(
        &self,
        run_id: &str,
    ) -> Result<Option<RunProjection>, DatabaseError> {
        let row = sqlx::query_as::<_, InspectRunRow>(include_str!(
            "../../sql/workflow_ledger/inspect_run.sql"
        ))
        .bind(run_id)
        .fetch_optional(self.database.readers())
        .await
        .map_err(DatabaseError::Query)?;
        let Some(row) = row else { return Ok(None) };
        let step_rows = sqlx::query_as::<_, StepRow>("select id, step_key as key, implementation, target_id, runtime_status as status, input_json, class, effect_boundary, skippable from workflow_step where run_id = ? order by id")
        .bind(run_id)
        .fetch_all(self.database.readers())
        .await
        .map_err(DatabaseError::Query)?;
        let dependency_rows: Vec<(String, String)> = sqlx::query_as("select dependency.step_id, prerequisite.step_key from step_dependency dependency join workflow_step prerequisite on prerequisite.id = dependency.depends_on_step_id join workflow_step step on step.id = dependency.step_id where step.run_id = ? order by dependency.step_id, prerequisite.step_key")
            .bind(run_id).fetch_all(self.database.readers()).await.map_err(DatabaseError::Query)?;
        let mut dependencies = BTreeMap::<String, Vec<String>>::new();
        for (step_id, key) in dependency_rows {
            dependencies.entry(step_id).or_default().push(key);
        }
        let steps = step_rows
            .into_iter()
            .map(|step| StepProjection {
                dependencies: dependencies.remove(&step.id).unwrap_or_default(),
                id: step.id,
                key: step.key,
                implementation: step.implementation,
                target_id: step.target_id,
                status: step.status,
                input_json: step.input_json,
                class: step.class,
                effect_boundary: step.effect_boundary,
                skippable: step.skippable,
            })
            .collect();
        let attempt_rows = sqlx::query_as::<_, AttemptRow>(include_str!(
            "../../sql/workflow_ledger/inspect_attempts.sql"
        ))
        .bind(run_id)
        .fetch_all(self.database.readers())
        .await
        .map_err(DatabaseError::Query)?;
        let output_rows = sqlx::query_file_as!(
            AttemptOutputRow,
            "sql/workflow_ledger/inspect_output.sql",
            run_id
        )
        .fetch_all(self.database.readers())
        .await
        .map_err(DatabaseError::Query)?;
        let mut output_by_attempt = BTreeMap::<String, Vec<OutputProjection>>::new();
        for output in output_rows {
            output_by_attempt
                .entry(output.attempt_id)
                .or_default()
                .push(OutputProjection {
                    sequence: output.sequence,
                    stream: output.stream,
                    body: output.body,
                    time_unix_ms: output.time_unix_ms,
                });
        }
        let binding_rows = sqlx::query_as::<_, AttemptBindingProjection>("select binding.attempt_id,binding.name,binding.schema_id,binding.value_json,binding.artifact_id from attempt_output_binding binding join step_attempt attempt on attempt.id=binding.attempt_id join workflow_step step on step.id=attempt.step_id where step.run_id=? order by binding.attempt_id,binding.name")
            .bind(run_id).fetch_all(self.database.readers()).await.map_err(DatabaseError::Query)?;
        let mut bindings_by_attempt = BTreeMap::<String, Vec<AttemptBindingProjection>>::new();
        for binding in binding_rows {
            bindings_by_attempt
                .entry(binding.attempt_id.clone())
                .or_default()
                .push(binding);
        }
        let attempts = attempt_rows
            .into_iter()
            .map(|attempt| AttemptProjection {
                output: output_by_attempt.remove(&attempt.id).unwrap_or_default(),
                bindings: bindings_by_attempt.remove(&attempt.id).unwrap_or_default(),
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
            })
            .collect();
        let artifacts = sqlx::query_file_as!(
            ArtifactProjection,
            "sql/workflow_ledger/inspect_artifacts.sql",
            run_id
        )
        .fetch_all(self.database.readers())
        .await
        .map_err(DatabaseError::Query)?;
        let approvals = sqlx::query_as::<_, ApprovalProjection>("select request.id, request.step_id, request.status, request.requested_unix_ms, request.decided_unix_ms, request.decided_by, request.decision_note, evidence.subject_json, evidence.evidence_json, evidence.policy_json from approval_request request left join approval_evidence evidence on evidence.approval_id = request.id where request.run_id = ? order by request.requested_unix_ms, request.id")
        .bind(run_id)
        .fetch_all(self.database.readers())
        .await
        .map_err(DatabaseError::Query)?;
        let effects = sqlx::query_file_as!(
            EffectProjection,
            "sql/workflow_ledger/inspect_effects.sql",
            run_id
        )
        .fetch_all(self.database.readers())
        .await
        .map_err(DatabaseError::Query)?;
        let gates = sqlx::query_as::<_, GateProjection>("select wait.step_id, wait.gate_kind, wait.due_unix_ms, wait.checkpoint_json, wait.poll_count, observation.subject_json, observation.evidence_json, observation.policy_json from gate_wait wait join workflow_step step on step.id=wait.step_id left join gate_observation observation on observation.attempt_id=(select latest.attempt_id from gate_observation latest where latest.step_id=wait.step_id order by latest.observed_unix_ms desc limit 1) where step.run_id=? order by wait.due_unix_ms, wait.step_id")
        .bind(run_id)
        .fetch_all(self.database.readers())
        .await
        .map_err(DatabaseError::Query)?;
        let events = sqlx::query_file_as!(
            AuditProjection,
            "sql/workflow_ledger/inspect_events.sql",
            run_id
        )
        .fetch_all(self.database.readers())
        .await
        .map_err(DatabaseError::Query)?;
        let lineage: (Option<String>, Option<String>, Option<i64>, bool, Option<i64>, i64) = sqlx::query_as("select parent_run_id, lineage_root_id, archived_unix_ms, detached, attempt_budget, attempts_consumed from workflow_run where id = ?")
            .bind(run_id).fetch_one(self.database.readers()).await.map_err(DatabaseError::Query)?;
        let children = sqlx::query_as::<_, ChildProjection>("select link.parent_step_id as step_id, link.iteration, link.child_run_id as run_id, run.runtime_status as status from child_run_link link join workflow_run run on run.id=link.child_run_id where link.parent_run_id=? order by link.parent_step_id, link.iteration")
            .bind(run_id).fetch_all(self.database.readers()).await.map_err(DatabaseError::Query)?;
        let authority = sqlx::query_as::<_, AuthorityProjection>("select scope, granted_by, granted_unix_ms, expires_unix_ms from authority_grant where run_id=? order by scope")
            .bind(run_id).fetch_all(self.database.readers()).await.map_err(DatabaseError::Query)?;
        Ok(Some(RunProjection {
            id: row.id,
            definition_name: row.definition_name,
            status: row.status,
            repository: row.repository,
            created_unix_ms: row.created_unix_ms,
            updated_unix_ms: row.updated_unix_ms,
            completed_unix_ms: row.completed_unix_ms,
            parent_run_id: lineage.0,
            lineage_root_id: lineage.1,
            archived_unix_ms: lineage.2,
            detached: lineage.3,
            attempt_budget: lineage.4,
            attempts_consumed: lineage.5,
            steps,
            attempts,
            artifacts,
            approvals,
            effects,
            gates,
            events,
            children,
            authority,
        }))
    }

    pub(crate) async fn health(&self) -> Result<HealthProjection, DatabaseError> {
        let pool = self.database.readers();
        let orphaned_definition_snapshots = sqlx::query_scalar(
            "select count(*) from definition_snapshot snapshot where not exists (select 1 from workflow_run run where run.definition_snapshot_id = snapshot.id) and not exists (select 1 from trigger_definition trigger where trigger.definition_snapshot_id = snapshot.id) and not exists (select 1 from workflow_step step where step.child_snapshot_id = snapshot.id)",
        )
        .fetch_one(pool)
        .await
        .map_err(DatabaseError::Query)?;
        let quarantined_workspaces = sqlx::query_scalar(
            "select count(*) from execution_workspace where state = 'quarantined'",
        )
        .fetch_one(pool)
        .await
        .map_err(DatabaseError::Query)?;
        let indeterminate_effects = sqlx::query_scalar(
            "select count(*) from effect_intent where status in ('dispatching', 'indeterminate')",
        )
        .fetch_one(pool)
        .await
        .map_err(DatabaseError::Query)?;
        let recovery_required_runs = sqlx::query_scalar(
            "select count(*) from workflow_run where status = 'recovery_required' or runtime_status = 'recovery_required'",
        )
        .fetch_one(pool)
        .await
        .map_err(DatabaseError::Query)?;
        let invalid_child_links = sqlx::query_scalar(
            "select count(*) from workflow_run child where child.parent_run_id is not null and not exists (select 1 from child_run_link link where link.child_run_id = child.id and link.parent_run_id = child.parent_run_id and link.parent_step_id = child.parent_step_id)",
        )
        .fetch_one(pool)
        .await
        .map_err(DatabaseError::Query)?;
        let artifacts: Vec<ArtifactHealthRow> = sqlx::query_as(
            "select id, digest, size_bytes, inline_body, file_path from artifact order by id",
        )
        .fetch_all(pool)
        .await
        .map_err(DatabaseError::Query)?;
        let mut artifact_integrity_failures = Vec::new();
        for artifact in artifacts {
            let bytes = match (artifact.inline_body, artifact.file_path) {
                (Some(bytes), None) => Ok(bytes),
                (None, Some(path)) => std::fs::read(&path).map_err(|error| {
                    format!("read {}: {error}", std::path::Path::new(&path).display())
                }),
                _ => Err("artifact must have exactly one storage location".into()),
            };
            let failure = match bytes {
                Err(reason) => Some(reason),
                Ok(bytes)
                    if i64::try_from(bytes.len()).unwrap_or(i64::MAX) != artifact.size_bytes =>
                {
                    Some(format!(
                        "size mismatch: expected {}, got {}",
                        artifact.size_bytes,
                        bytes.len()
                    ))
                }
                Ok(bytes) => {
                    let actual = format!("sha256:{:x}", Sha256::digest(bytes));
                    (actual != artifact.digest).then(|| {
                        format!(
                            "digest mismatch: expected {}, got {actual}",
                            artifact.digest
                        )
                    })
                }
            };
            if let Some(reason) = failure {
                artifact_integrity_failures.push(ArtifactIntegrityProjection {
                    artifact_id: artifact.id,
                    reason,
                });
            }
        }
        Ok(HealthProjection {
            orphaned_definition_snapshots,
            quarantined_workspaces,
            indeterminate_effects,
            recovery_required_runs,
            invalid_child_links,
            artifact_integrity_failures,
        })
    }
}

fn step_class_name(class: StepClass) -> &'static str {
    match class {
        StepClass::Action => "action",
        StepClass::Gate => "gate",
        StepClass::Approval => "approval",
        StepClass::Wait => "wait",
        StepClass::Notification => "notification",
        StepClass::WorkflowCall => "workflow_call",
    }
}

fn effect_boundary_name(boundary: prism_extension_protocol::EffectBoundary) -> &'static str {
    use prism_extension_protocol::EffectBoundary;
    match boundary {
        EffectBoundary::None => "none",
        EffectBoundary::Brokered => "brokered",
        EffectBoundary::Unbrokered => "unbrokered",
    }
}

fn unknown_policy_name(
    policy: crate::workflow::definition::UnknownConditionPolicy,
) -> &'static str {
    use crate::workflow::definition::UnknownConditionPolicy;
    match policy {
        UnknownConditionPolicy::Wait => "wait",
        UnknownConditionPolicy::Skip => "skip",
        UnknownConditionPolicy::Fail => "fail",
    }
}

fn resolve_launch_bindings(
    bindings: &BTreeMap<String, crate::workflow::definition::Binding>,
    inputs: &serde_json::Map<String, serde_json::Value>,
    parameters: &BTreeMap<String, serde_json::Value>,
) -> Result<serde_json::Value, DatabaseError> {
    use crate::workflow::definition::Binding;
    let mut output = serde_json::Map::new();
    for (name, binding) in bindings {
        let value = match binding {
            Binding::Literal { value } | Binding::Parameter { value, .. } => value.clone(),
            Binding::Reference { reference, .. } if reference.starts_with("inputs.") => {
                let key = reference.trim_start_matches("inputs.");
                inputs
                    .get(key)
                    .cloned()
                    .ok_or_else(|| DatabaseError::InvalidValue {
                        field: "workflow inputs",
                        value: format!("missing binding {reference}"),
                    })?
            }
            Binding::Reference { reference, .. } if reference.starts_with("parameters.") => {
                let key = reference.trim_start_matches("parameters.");
                parameters
                    .get(key)
                    .cloned()
                    .ok_or_else(|| DatabaseError::InvalidValue {
                        field: "workflow parameters",
                        value: format!("missing binding {reference}"),
                    })?
            }
            Binding::Reference { .. } => continue,
        };
        output.insert(name.clone(), value);
    }
    Ok(serde_json::Value::Object(output))
}

fn repeat_satisfied(
    repeat: &crate::workflow::definition::CompiledRepeat,
    outputs: &BTreeMap<String, (String, serde_json::Value, Option<String>)>,
) -> bool {
    let mut values = BTreeMap::new();
    let mut references = Vec::new();
    repeat.until.references(&mut references);
    for reference in references {
        if let Some(name) = reference.split('.').next_back()
            && let Some((_, value, _)) = outputs.get(name)
        {
            values.insert(
                reference,
                crate::workflow::definition::ConditionValue::Known(value.clone()),
            );
        }
    }
    matches!(
        repeat.until.evaluate(&values),
        crate::workflow::definition::ConditionValue::Known(serde_json::Value::Bool(true))
    )
}

async fn load_run_reference_record(
    pool: &sqlx::SqlitePool,
    run_id: &str,
    reference: &str,
) -> Result<Option<(serde_json::Value, Option<String>)>, DatabaseError> {
    let parts: Vec<_> = reference.split('.').collect();
    if parts.len() != 4 || parts[0] != "steps" || parts[2] != "outputs" {
        return Ok(None);
    }
    let row: Option<(String, Option<String>)> = sqlx::query_as("select value_json, artifact_id from (select binding.value_json, binding.artifact_id, attempt.attempt_number as ordering from workflow_step step join step_attempt attempt on attempt.step_id=step.id join attempt_output_binding binding on binding.attempt_id=attempt.id where step.run_id=? and step.step_key=? and binding.name=? and attempt.status='succeeded' and (step.invalidated_unix_ms is null or attempt.finished_unix_ms >= step.invalidated_unix_ms) union all select binding.value_json, binding.artifact_id, 2147483647 from workflow_step step join step_output_binding binding on binding.step_id=step.id where step.run_id=? and step.step_key=? and binding.name=?) order by ordering desc limit 1")
        .bind(run_id).bind(parts[1]).bind(parts[3]).bind(run_id).bind(parts[1]).bind(parts[3])
        .fetch_optional(pool).await.map_err(DatabaseError::Query)?;
    row.map(|(value, artifact_id)| {
        serde_json::from_str(&value)
            .map(|value| (value, artifact_id))
            .map_err(|error| DatabaseError::InvalidValue {
                field: "workflow output",
                value: error.to_string(),
            })
    })
    .transpose()
}

pub(super) async fn project_run_state(
    connection: &mut sqlx::SqliteConnection,
    run_id: &str,
    now_unix_ms: i64,
) -> Result<(), DatabaseError> {
    let current: String = sqlx::query_scalar("select status from workflow_run where id = ?")
        .bind(run_id)
        .fetch_one(&mut *connection)
        .await
        .map_err(DatabaseError::Query)?;
    if current == "cancelled" {
        return Ok(());
    }
    let (failed, cancelled, unfinished): (i64, i64, i64) = sqlx::query_as("select sum(case when status = 'failed' then 1 else 0 end), sum(case when status = 'cancelled' then 1 else 0 end), sum(case when status <> 'succeeded' then 1 else 0 end) from workflow_step where run_id = ?")
        .bind(run_id).fetch_one(&mut *connection).await.map_err(DatabaseError::Query)?;
    if failed > 0 {
        sqlx::query("update workflow_run set status = 'failed', runtime_status = 'failed', updated_unix_ms = ?, completed_unix_ms = ? where id = ?")
            .bind(now_unix_ms).bind(now_unix_ms).bind(run_id).execute(connection).await.map_err(DatabaseError::Query)?;
    } else if cancelled > 0 {
        sqlx::query("update workflow_run set status = 'cancelled', runtime_status = 'cancelled', updated_unix_ms = ?, completed_unix_ms = ? where id = ?")
            .bind(now_unix_ms).bind(now_unix_ms).bind(run_id).execute(connection).await.map_err(DatabaseError::Query)?;
    } else if unfinished == 0 {
        sqlx::query("update workflow_run set status = 'succeeded', runtime_status = 'succeeded', updated_unix_ms = ?, completed_unix_ms = ? where id = ?")
            .bind(now_unix_ms).bind(now_unix_ms).bind(run_id).execute(connection).await.map_err(DatabaseError::Query)?;
    } else {
        sqlx::query("update workflow_run set status = case when exists(select 1 from workflow_step where run_id = ? and status in ('runnable','claimed')) then 'runnable' else 'waiting' end, runtime_status = case when exists(select 1 from workflow_step where run_id = ? and status in ('runnable','claimed')) then 'runnable' else 'waiting' end, updated_unix_ms = ? where id = ? and status <> 'paused'")
            .bind(run_id).bind(run_id).bind(now_unix_ms).bind(run_id).execute(connection).await.map_err(DatabaseError::Query)?;
    }
    Ok(())
}

async fn validate_and_extract_outputs(
    connection: &mut sqlx::SqliteConnection,
    step_id: &str,
    declared: &BTreeMap<String, String>,
    result_json: &str,
) -> Result<BTreeMap<String, (String, serde_json::Value)>, DatabaseError> {
    let value: serde_json::Value =
        serde_json::from_str(result_json).map_err(|error| DatabaseError::InvalidValue {
            field: "attempt outputs",
            value: error.to_string(),
        })?;
    let object = value
        .as_object()
        .ok_or_else(|| DatabaseError::InvalidValue {
            field: "attempt outputs",
            value: "expected a JSON object".into(),
        })?;
    if object.keys().any(|name| !declared.contains_key(name)) {
        return Err(DatabaseError::InvalidValue {
            field: "attempt outputs",
            value: "undeclared output".into(),
        });
    }
    let body: String = sqlx::query_scalar("select snapshot.body_json from workflow_step step join workflow_run run on run.id = step.run_id join definition_snapshot snapshot on snapshot.id = run.definition_snapshot_id where step.id = ?")
        .bind(step_id).fetch_one(&mut *connection).await.map_err(DatabaseError::Query)?;
    let snapshot: crate::workflow::definition::DefinitionSnapshot = serde_json::from_str(&body)
        .map_err(|error| DatabaseError::InvalidValue {
            field: "definition snapshot",
            value: error.to_string(),
        })?;
    let mut output = BTreeMap::new();
    for (name, schema_id) in declared {
        let value = object
            .get(name)
            .cloned()
            .ok_or_else(|| DatabaseError::InvalidValue {
                field: "attempt outputs",
                value: format!("missing declared output {name}"),
            })?;
        if let Some(schema) = snapshot.schemas.get(schema_id) {
            validate_json_schema(schema, &value).map_err(|message| {
                DatabaseError::InvalidValue {
                    field: "attempt outputs",
                    value: format!("{name}: {message}"),
                }
            })?;
        }
        output.insert(name.clone(), (schema_id.clone(), value));
    }
    Ok(output)
}

fn validate_json_schema(
    schema: &serde_json::Value,
    value: &serde_json::Value,
) -> Result<(), String> {
    if let Some(all) = schema.get("allOf").and_then(serde_json::Value::as_array) {
        for child in all {
            validate_json_schema(child, value)?;
        }
    }
    if let Some(any) = schema.get("anyOf").and_then(serde_json::Value::as_array)
        && !any
            .iter()
            .any(|child| validate_json_schema(child, value).is_ok())
    {
        return Err("does not match anyOf".into());
    }
    if let Some(one) = schema.get("oneOf").and_then(serde_json::Value::as_array)
        && one
            .iter()
            .filter(|child| validate_json_schema(child, value).is_ok())
            .count()
            != 1
    {
        return Err("does not match exactly one oneOf schema".into());
    }
    if let Some(negated) = schema.get("not")
        && validate_json_schema(negated, value).is_ok()
    {
        return Err("matches forbidden schema".into());
    }
    if let Some(constant) = schema.get("const")
        && constant != value
    {
        return Err("does not match const".into());
    }
    if let Some(values) = schema.get("enum").and_then(serde_json::Value::as_array)
        && !values.contains(value)
    {
        return Err("is not an enum member".into());
    }
    if let Some(expected) = schema.get("type").and_then(serde_json::Value::as_str) {
        let valid = match expected {
            "null" => value.is_null(),
            "boolean" => value.is_boolean(),
            "number" => value.is_number(),
            "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
            "string" => value.is_string(),
            "array" => value.is_array(),
            "object" => value.is_object(),
            _ => true,
        };
        if !valid {
            return Err(format!("expected JSON {expected}"));
        }
    }
    if let Some(object) = value.as_object() {
        let properties = schema
            .get("properties")
            .and_then(serde_json::Value::as_object);
        if let Some(required) = schema.get("required").and_then(serde_json::Value::as_array) {
            for name in required.iter().filter_map(serde_json::Value::as_str) {
                if !object.contains_key(name) {
                    return Err(format!("missing required property {name}"));
                }
            }
        }
        for (name, child) in object {
            if let Some(child_schema) = properties.and_then(|properties| properties.get(name)) {
                validate_json_schema(child_schema, child)
                    .map_err(|error| format!("{name}: {error}"))?;
            } else if let Some(additional) = schema.get("additionalProperties") {
                if additional == &serde_json::Value::Bool(false) {
                    return Err(format!("additional property {name} is forbidden"));
                }
                if additional.is_object() {
                    validate_json_schema(additional, child)
                        .map_err(|error| format!("{name}: {error}"))?;
                }
            }
        }
        let count = u64::try_from(object.len()).unwrap_or(u64::MAX);
        if schema
            .get("minProperties")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|minimum| count < minimum)
        {
            return Err("has too few properties".into());
        }
        if schema
            .get("maxProperties")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|maximum| count > maximum)
        {
            return Err("has too many properties".into());
        }
    }
    if let Some(array) = value.as_array() {
        if let Some(item_schema) = schema.get("items") {
            for (index, item) in array.iter().enumerate() {
                validate_json_schema(item_schema, item)
                    .map_err(|error| format!("item {index}: {error}"))?;
            }
        }
        let count = u64::try_from(array.len()).unwrap_or(u64::MAX);
        if schema
            .get("minItems")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|minimum| count < minimum)
        {
            return Err("has too few items".into());
        }
        if schema
            .get("maxItems")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|maximum| count > maximum)
        {
            return Err("has too many items".into());
        }
        if schema.get("uniqueItems") == Some(&serde_json::Value::Bool(true))
            && array
                .iter()
                .enumerate()
                .any(|(index, item)| array[..index].contains(item))
        {
            return Err("items are not unique".into());
        }
    }
    if let Some(text) = value.as_str() {
        let length = u64::try_from(text.chars().count()).unwrap_or(u64::MAX);
        if schema
            .get("minLength")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|minimum| length < minimum)
        {
            return Err("is shorter than minLength".into());
        }
        if schema
            .get("maxLength")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|maximum| length > maximum)
        {
            return Err("is longer than maxLength".into());
        }
        if let Some(pattern) = schema.get("pattern").and_then(serde_json::Value::as_str) {
            let pattern = regex::Regex::new(pattern)
                .map_err(|error| format!("invalid schema pattern: {error}"))?;
            if !pattern.is_match(text) {
                return Err("does not match pattern".into());
            }
        }
    }
    if let Some(number) = value.as_f64() {
        if schema
            .get("minimum")
            .and_then(serde_json::Value::as_f64)
            .is_some_and(|minimum| number < minimum)
        {
            return Err("is below minimum".into());
        }
        if schema
            .get("maximum")
            .and_then(serde_json::Value::as_f64)
            .is_some_and(|maximum| number > maximum)
        {
            return Err("is above maximum".into());
        }
        if schema
            .get("exclusiveMinimum")
            .and_then(serde_json::Value::as_f64)
            .is_some_and(|minimum| number <= minimum)
        {
            return Err("is not above exclusiveMinimum".into());
        }
        if schema
            .get("exclusiveMaximum")
            .and_then(serde_json::Value::as_f64)
            .is_some_and(|maximum| number >= maximum)
        {
            return Err("is not below exclusiveMaximum".into());
        }
    }
    Ok(())
}

#[allow(
    dead_code,
    reason = "used by the generalized scheduler during workflow cutover"
)]
impl Coordinator {
    pub(crate) fn new(database: WorkflowDatabase) -> Self {
        Self { database }
    }

    pub(crate) async fn claim(
        &self,
        request: ClaimRequest<'_>,
    ) -> Result<Option<AttemptLease>, DatabaseError> {
        let attempt_id = request.attempt_id.to_string();
        let step_id = request.step_id.to_string();
        let worker_id = request.worker_id.to_string();
        let now_unix_ms = request.now_unix_ms;
        let lease_expires_unix_ms = request.lease_expires_unix_ms;
        if lease_expires_unix_ms <= now_unix_ms {
            return Err(DatabaseError::InvalidValue {
                field: "lease_expires_unix_ms",
                value: lease_expires_unix_ms.to_string(),
            });
        }
        self.database.write_immediate(|connection| Box::pin(async move {
            let row = sqlx::query_file!(
                "sql/workflow_ledger/claim_attempt.sql",
                attempt_id,
                worker_id,
                lease_expires_unix_ms,
                now_unix_ms,
                step_id,
                now_unix_ms
            )
                .fetch_optional(&mut *connection).await.map_err(DatabaseError::Query)?;
            let Some(row) = row else { return Ok(None) };
            sqlx::query("update step_attempt set input_revisions_json = (select resolved_input_revisions_json from workflow_step where id = ?) where id = ?")
                .bind(&step_id).bind(&attempt_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
            sqlx::query("update workflow_step set status = 'claimed' where id = ? and status = 'runnable'")
                .bind(&step_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
            let lease = AttemptLease {
                attempt_id: row.id,
                step_id: row.step_id,
                worker_id: row.worker_id,
                target_id: row.target_id,
                fencing_token: row.fencing_token,
                lease_expires_unix_ms: row.lease_expires_unix_ms,
            };
            append_event(connection, &lease, "attempt_claimed", "{}", now_unix_ms).await?;
            Ok(Some(lease))
        })).await
    }

    pub(crate) async fn append_event(
        &self,
        lease: &AttemptLease,
        kind: &str,
        data_json: &str,
        now: i64,
    ) -> Result<(), DatabaseError> {
        let lease = lease.clone();
        let kind = kind.to_string();
        let data_json = data_json.to_string();
        self.database
            .write_immediate(|connection| {
                Box::pin(
                    async move { append_event(connection, &lease, &kind, &data_json, now).await },
                )
            })
            .await
    }

    pub(crate) async fn renew(
        &self,
        lease: &AttemptLease,
        now: i64,
        expires: i64,
    ) -> Result<(), DatabaseError> {
        let lease = lease.clone();
        self.database
            .write_immediate(|connection| {
                Box::pin(async move {
                    let changed = sqlx::query_file!(
                        "sql/workflow_ledger/renew_lease.sql",
                        expires,
                        lease.attempt_id,
                        lease.worker_id,
                        lease.target_id,
                        lease.fencing_token,
                        now
                    )
                    .execute(connection)
                    .await
                    .map_err(DatabaseError::Query)?
                    .rows_affected();
                    exactly_one_fenced(changed)
                })
            })
            .await
    }

    pub(crate) async fn finish(
        &self,
        lease: &AttemptLease,
        result: AttemptResult<'_>,
    ) -> Result<(), DatabaseError> {
        if !matches!(result.status, "succeeded" | "failed" | "cancelled") {
            return Err(DatabaseError::InvalidValue {
                field: "attempt status",
                value: result.status.into(),
            });
        }
        let lease = lease.clone();
        let status = result.status.to_string();
        let result_json = result.result_json.to_string();
        let finished_unix_ms = result.finished_unix_ms;
        self.database
            .write_immediate(|connection| {
                Box::pin(async move {
                    let (class, outputs_json, retry_max, run_id, settings_json): (String, String, i64, String, String) = sqlx::query_as("select class, outputs_json, retry_max_attempts, run_id, settings_json from workflow_step where id = ?")
                        .bind(&lease.step_id).fetch_one(&mut *connection).await.map_err(DatabaseError::Query)?;
                    let mut effective_status = status.clone();
                    let mut effective_result = result_json.clone();
                    let outputs: BTreeMap<String, String> = serde_json::from_str(&outputs_json)
                        .map_err(|error| DatabaseError::InvalidValue { field: "declared outputs", value: error.to_string() })?;
                    if effective_status == "succeeded" && !outputs.is_empty() {
                        match validate_and_extract_outputs(&mut *connection, &lease.step_id, &outputs, &effective_result).await {
                            Ok(values) => {
                                for (name, (schema, value)) in values {
                                    let body = serde_json::to_vec(&value).map_err(|error| DatabaseError::InvalidValue { field: "attempt output", value: error.to_string() })?;
                                    let artifact_id = format!("{}:output:{name}", lease.attempt_id);
                                    let digest = crate::resource::ContentRevision::digest(&body).to_string();
                                    sqlx::query("insert into artifact (id, run_id, producing_attempt_id, revision, digest, size_bytes, sensitivity, inline_body, created_unix_ms) values (?, ?, ?, 1, ?, ?, 'normal', ?, ?)")
                                        .bind(&artifact_id).bind(&run_id).bind(&lease.attempt_id).bind(digest).bind(i64::try_from(body.len()).unwrap_or(i64::MAX)).bind(body).bind(finished_unix_ms)
                                        .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                                    sqlx::query("insert into attempt_output_binding (attempt_id, name, schema_id, value_json, artifact_id) values (?, ?, ?, ?, ?)")
                                        .bind(&lease.attempt_id).bind(name).bind(schema).bind(value.to_string()).bind(artifact_id)
                                        .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                                }
                            }
                            Err(error) => {
                                effective_status = "failed".into();
                                effective_result = serde_json::json!({"error": error.to_string(), "kind": "invalid_outputs"}).to_string();
                            }
                        }
                    }
                    append_event(
                        &mut *connection,
                        &lease,
                        "attempt_finished",
                        &effective_result,
                        finished_unix_ms,
                    )
                    .await?;
                    let changed = sqlx::query_file!(
                        "sql/workflow_ledger/finish_attempt.sql",
                        effective_status,
                        effective_result,
                        finished_unix_ms,
                        lease.attempt_id,
                        lease.worker_id,
                        lease.target_id,
                        lease.fencing_token,
                        finished_unix_ms
                    )
                    .execute(&mut *connection)
                    .await
                    .map_err(DatabaseError::Query)?
                    .rows_affected();
                    exactly_one_fenced(changed)?;
                    let attempts: i64 = sqlx::query_scalar("select count(*) from step_attempt where step_id = ?")
                        .bind(&lease.step_id).fetch_one(&mut *connection).await.map_err(DatabaseError::Query)?;
                    // `max_attempts` is the total number of attempts, not the retry count.
                    let retry = effective_status == "failed" && attempts < retry_max;
                    let gate_reobserve = class == "gate" && effective_status == "succeeded"
                        && serde_json::from_str::<serde_json::Value>(&effective_result).ok().and_then(|value| value.get("ready").and_then(serde_json::Value::as_bool)) == Some(false);
                    let step_status = if gate_reobserve { "waiting" } else if class == "notification" { "succeeded" } else if retry { "runnable" } else { effective_status.as_str() };
                    let runtime_status = if gate_reobserve { "waiting_gate" } else if class == "notification" && effective_status != "succeeded" { "succeeded_with_diagnostic" } else { step_status };
                    let step_changed = sqlx::query(
                        "update workflow_step set status = ?, runtime_status = ?, available_unix_ms = ? where id = ? and status = 'claimed'",
                    )
                    .bind(step_status)
                    .bind(runtime_status)
                    .bind(finished_unix_ms)
                    .bind(&lease.step_id)
                    .execute(&mut *connection)
                    .await
                    .map_err(DatabaseError::Query)?
                    .rows_affected();
                    if step_changed != 1 {
                        return Err(DatabaseError::Conflict {
                            operation: "finish step",
                        });
                    }
                    if class == "gate" {
                        sqlx::query("insert into gate_observation (attempt_id, step_id, subject_json, evidence_json, policy_json, observed_unix_ms) values (?, ?, ?, ?, ?, ?)")
                            .bind(&lease.attempt_id).bind(&lease.step_id).bind(&settings_json).bind(&effective_result).bind(&settings_json).bind(finished_unix_ms)
                            .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                        if gate_reobserve {
                            let settings: serde_json::Value = serde_json::from_str(&settings_json).unwrap_or_default();
                            let delay = settings.get("reobserve_after_ms").and_then(serde_json::Value::as_i64).unwrap_or(1_000).max(1);
                            sqlx::query("insert into gate_wait (step_id, gate_kind, due_unix_ms, checkpoint_json, poll_count) values (?, 'reobserve', ?, ?, 1) on conflict(step_id) do update set due_unix_ms=excluded.due_unix_ms, checkpoint_json=excluded.checkpoint_json, poll_count=gate_wait.poll_count+1")
                                .bind(&lease.step_id).bind(finished_unix_ms.saturating_add(delay)).bind(&effective_result)
                                .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                        }
                    }
                    sqlx::query("delete from resource_claim where attempt_id = ?")
                        .bind(&lease.attempt_id)
                        .execute(&mut *connection)
                        .await
                        .map_err(DatabaseError::Query)?;
                    sqlx::query("delete from capacity_claim where attempt_id = ?")
                        .bind(&lease.attempt_id)
                        .execute(&mut *connection)
                        .await
                        .map_err(DatabaseError::Query)?;
                    project_run_state(connection, &run_id, finished_unix_ms).await
                })
            })
            .await
    }

    pub(crate) async fn run_is_cancelled(&self, run_id: &str) -> Result<bool, DatabaseError> {
        sqlx::query_scalar("select status = 'cancelled' from workflow_run where id = ?")
            .bind(run_id)
            .fetch_one(self.database.readers())
            .await
            .map_err(DatabaseError::Query)
    }
}

#[allow(
    dead_code,
    reason = "used by the generalized scheduler during workflow cutover"
)]
async fn append_event(
    connection: &mut sqlx::SqliteConnection,
    lease: &AttemptLease,
    kind: &str,
    data: &str,
    now: i64,
) -> Result<(), DatabaseError> {
    let changed = sqlx::query_file!(
        "sql/workflow_ledger/append_fenced_event.sql",
        kind,
        now,
        data,
        lease.attempt_id,
        lease.worker_id,
        lease.target_id,
        lease.fencing_token,
        now
    )
    .execute(connection)
    .await
    .map_err(DatabaseError::Query)?
    .rows_affected();
    exactly_one_fenced(changed)
}

#[allow(
    dead_code,
    reason = "used by the generalized scheduler during workflow cutover"
)]
fn exactly_one_fenced(changed: u64) -> Result<(), DatabaseError> {
    if changed == 1 {
        Ok(())
    } else {
        Err(DatabaseError::StaleClaim)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use sqlx::Connection;

    use super::*;

    static SEQUENCE: AtomicU64 = AtomicU64::new(1);

    fn path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "prism-workflow-ledger-{}-{}.db",
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
            sqlx::query("insert into definition_snapshot (id, definition_name, revision, source, trusted, body_json, digest, created_unix_ms) values ('definition-1', 'approval-tracer', '1', 'bundled', 1, '{}', 'digest', 1)")
                .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
            sqlx::query("insert into workflow_run (id, definition_snapshot_id, status, created_unix_ms, updated_unix_ms) values ('run-1', 'definition-1', 'runnable', 1, 1)")
                .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
            sqlx::query("insert into workflow_step (id, run_id, step_key, implementation, target_id, status, available_unix_ms, input_json) values ('step-1', 'run-1', 'approval', 'approval', 'local', 'runnable', 1, '{}')")
                .execute(connection).await.map_err(DatabaseError::Query)?;
            Ok(())
        })).await.unwrap();
    }

    #[test]
    fn manual_invocation_is_idempotent_and_survives_reopen() {
        let path = path();
        runtime().block_on(async {
            let database = WorkflowDatabase::open(&path).await.unwrap();
            database.write_immediate(|connection| Box::pin(async move {
                sqlx::query("insert into definition_snapshot (id, definition_name, revision, source, trusted, body_json, digest, created_unix_ms) values ('definition-1', 'approval-tracer', '1', 'bundled', 1, '{}', 'digest', 1)")
                    .execute(connection).await.map_err(DatabaseError::Query)?;
                Ok(())
            })).await.unwrap();
            let ledger = RunLedger::new(database.clone());
            for proposed_id in ["run-1", "run-2"] {
                assert_eq!(ledger.start_materialized(StartRun {
                    run_id: proposed_id,
                    definition_snapshot_id: "definition-1",
                    repository: None,
                    idempotency_key: "invocation-1",
                    input_json: "{}",
                    now_unix_ms: 2,
                    paused: false,
                }, Vec::new()).await.unwrap(), "run-1");
            }
            drop(ledger);
            database.close().await;
            drop(database);
            let reopened = WorkflowDatabase::open(&path).await.unwrap();
            assert_eq!(RunLedger::new(reopened).inspect("run-1").await.unwrap().unwrap().definition_name, "approval-tracer");
        });
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn claims_are_exclusive_and_all_attempt_writes_are_fenced() {
        let path = path();
        runtime().block_on(async {
            let database = WorkflowDatabase::open(&path).await.unwrap();
            fixture(&database).await;
            let coordinator = Coordinator::new(database);
            let lease = coordinator
                .claim(ClaimRequest {
                    attempt_id: "attempt-1",
                    step_id: "step-1",
                    worker_id: "worker-1",
                    now_unix_ms: 2,
                    lease_expires_unix_ms: 10,
                })
                .await
                .unwrap()
                .unwrap();
            assert!(
                coordinator
                    .claim(ClaimRequest {
                        attempt_id: "attempt-2",
                        step_id: "step-1",
                        worker_id: "worker-2",
                        now_unix_ms: 2,
                        lease_expires_unix_ms: 10,
                    })
                    .await
                    .unwrap()
                    .is_none()
            );
            coordinator
                .append_event(&lease, "output", "{}", 3)
                .await
                .unwrap();
            assert!(matches!(
                coordinator
                    .append_event(&lease, "late_output", "{}", 11)
                    .await,
                Err(DatabaseError::StaleClaim)
            ));
            coordinator
                .finish(
                    &lease,
                    AttemptResult {
                        status: "succeeded",
                        result_json: "{}",
                        finished_unix_ms: 4,
                    },
                )
                .await
                .unwrap();
            assert!(matches!(
                coordinator
                    .append_event(&lease, "after_finish", "{}", 5)
                    .await,
                Err(DatabaseError::StaleClaim)
            ));
        });
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn final_attempt_terminalizes_a_paused_run() {
        let path = path();
        runtime().block_on(async {
            let database = WorkflowDatabase::open(&path).await.unwrap();
            fixture(&database).await;
            let ledger = RunLedger::new(database.clone());
            let coordinator = Coordinator::new(database);
            let lease = coordinator
                .claim(ClaimRequest {
                    attempt_id: "attempt-1",
                    step_id: "step-1",
                    worker_id: "worker-1",
                    now_unix_ms: 2,
                    lease_expires_unix_ms: 10,
                })
                .await
                .unwrap()
                .unwrap();
            ledger.command("run-1", RunCommand::Pause, 3).await.unwrap();

            coordinator
                .finish(
                    &lease,
                    AttemptResult {
                        status: "succeeded",
                        result_json: "{}",
                        finished_unix_ms: 4,
                    },
                )
                .await
                .unwrap();

            assert_eq!(
                ledger.inspect("run-1").await.unwrap().unwrap().status,
                "succeeded"
            );
        });
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn workflow_database_refuses_a_repository_database_without_mutating_it() {
        let path = path();
        runtime().block_on(async {
            let mut connection = sqlx::SqliteConnection::connect_with(
                &super::super::pools::options(&path, true, false).unwrap(),
            )
            .await
            .unwrap();
            sqlx::query("create table _sqlx_migrations (version integer primary key)")
                .execute(&mut connection)
                .await
                .unwrap();
            connection.close().await.unwrap();
            let before = std::fs::read(&path).unwrap();
            assert!(matches!(
                WorkflowDatabase::open(&path).await,
                Err(DatabaseError::WrongDatabase { .. })
            ));
            assert_eq!(std::fs::read(&path).unwrap(), before);
        });
        let _ = std::fs::remove_file(&path);
    }
}
