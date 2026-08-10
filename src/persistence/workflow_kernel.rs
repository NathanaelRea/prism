use std::path::{Path, PathBuf};

use sqlx::Connection as _;
use sqlx::sqlite::SqlitePoolOptions;

use crate::workflow::kernel::{
    StoreFuture, WorkflowKernelError, WorkflowRunState, WorkflowRunStore,
};
use crate::workflow::source::CompiledWorkflow;

const SCHEMA_EPOCH: i64 = 4;
const SCHEMA: &str = r#"
create table if not exists workflow_database_identity (
  singleton integer primary key check(singleton=1),
  kind text not null check(kind='workflow'),
  schema_epoch integer not null check(schema_epoch=4)
);
insert into workflow_database_identity(singleton, kind, schema_epoch)
values(1, 'workflow', 4)
on conflict(singleton) do update set kind=excluded.kind, schema_epoch=excluded.schema_epoch;

create table if not exists workflow_snapshot (
  digest text primary key,
  workflow_name text not null,
  source_path text not null,
  source_revision text not null,
  source text not null,
  body_json text not null,
  created_unix_ms integer not null
);

create table if not exists workflow_run (
  id text primary key,
  workflow_digest text not null references workflow_snapshot(digest),
  workflow_name text not null,
  repository text not null,
  worktree text not null,
  change_request text,
  change_request_head text,
  status text not null check(status in ('queued','running','waiting','needs_input','paused','succeeded','failed','cancelled','recovery_required')),
  cycle integer not null check(cycle > 0),
  max_agent_runs integer not null check(max_agent_runs > 0),
  agent_runs_consumed integer not null check(agent_runs_consumed >= 0),
  cancellation_requested integer not null check(cancellation_requested in (0,1)),
  created_unix_ms integer not null,
  updated_unix_ms integer not null,
  revision integer not null check(revision >= 0),
  state_json text not null
);
create index if not exists workflow_run_status_idx on workflow_run(status, updated_unix_ms);

create table if not exists workflow_step (
  run_id text not null references workflow_run(id) on delete cascade,
  step_index integer not null,
  step_key text not null,
  trigger_name text,
  phase text not null,
  summary text,
  wake_at_unix_ms integer,
  satisfied_cycle integer,
  unconditional_completed integer not null check(unconditional_completed in (0,1)),
  primary key(run_id, step_index),
  unique(run_id, step_key)
);
create index if not exists workflow_step_wake_idx on workflow_step(wake_at_unix_ms) where wake_at_unix_ms is not null;

create table if not exists workflow_dependency (
  run_id text not null,
  step_index integer not null,
  dependency_key text not null,
  primary key(run_id, step_index, dependency_key),
  foreign key(run_id, step_index) references workflow_step(run_id, step_index) on delete cascade
);

create table if not exists step_lifecycle_attempt (
  id text primary key,
  run_id text not null,
  step_index integer not null,
  attempt_number integer not null check(attempt_number > 0),
  status text not null,
  phase text not null,
  prepared_state_json text,
  agent_status text,
  agent_process_id integer,
  agent_session_id text,
  agent_final_text text,
  agent_turn_in_flight integer check(agent_turn_in_flight > 0),
  error text,
  started_unix_ms integer not null,
  finished_unix_ms integer,
  fencing_token integer not null check(fencing_token > 0),
  phase_owner text,
  lease_expires_unix_ms integer,
  foreign key(run_id, step_index) references workflow_step(run_id, step_index) on delete cascade,
  unique(run_id, step_index, attempt_number)
);

create table if not exists agent_turn (
  attempt_id text not null references step_lifecycle_attempt(id) on delete cascade,
  turn_number integer not null check(turn_number > 0),
  process_id integer,
  session_id text not null,
  final_text text not null,
  primary key(attempt_id, turn_number)
);

create table if not exists workflow_run_event (
  run_id text not null references workflow_run(id) on delete cascade,
  sequence integer not null,
  time_unix_ms integer not null,
  step_key text,
  attempt_id text,
  kind text not null,
  summary text not null,
  primary key(run_id, sequence)
);

create table if not exists trigger_executable_snapshot (
  digest text primary key,
  trigger_name text not null,
  executable_path text not null,
  retained_path text not null,
  created_unix_ms integer not null
);

create table if not exists remote_lane_cooldown (
  canonical_host text not null,
  credential_profile text not null,
  next_request_unix_ms integer not null,
  retry_count integer not null,
  updated_unix_ms integer not null,
  primary key(canonical_host, credential_profile)
);

create table if not exists remote_observation_subscription (
  observation_key text not null,
  subscriber_id text not null,
  created_unix_ms integer not null,
  primary key(observation_key, subscriber_id)
);
"#;

#[derive(Clone)]
pub struct DurableWorkflowRunStore {
    path: PathBuf,
    pool: sqlx::SqlitePool,
}

impl DurableWorkflowRunStore {
    pub async fn open(path: &Path) -> Result<Self, WorkflowKernelError> {
        prepare_epoch(path).await?;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                super::pools::options(path, true, false)
                    .map_err(|error| WorkflowKernelError::Persistence(error.to_string()))?,
            )
            .await
            .map_err(persistence)?;
        sqlx::raw_sql(SCHEMA)
            .execute(&pool)
            .await
            .map_err(persistence)?;
        Ok(Self {
            path: path.to_path_buf(),
            pool,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }

    pub async fn list_runs(
        &self,
        repository: Option<&Path>,
        limit: usize,
    ) -> Result<Vec<WorkflowRunState>, WorkflowKernelError> {
        let limit = i64::try_from(limit.min(10_000))
            .map_err(|_| WorkflowKernelError::Persistence("run list limit overflow".into()))?;
        let bodies = if let Some(repository) = repository {
            sqlx::query_scalar::<_, String>(
                "select state_json from workflow_run where repository=? order by updated_unix_ms desc, id limit ?",
            )
            .bind(repository.to_string_lossy().into_owned())
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .map_err(persistence)?
        } else {
            sqlx::query_scalar::<_, String>(
                "select state_json from workflow_run order by updated_unix_ms desc, id limit ?",
            )
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .map_err(persistence)?
        };
        bodies
            .into_iter()
            .map(|body| {
                serde_json::from_str(&body)
                    .map_err(|error| WorkflowKernelError::Persistence(error.to_string()))
            })
            .collect()
    }

    pub async fn active_run_ids(&self) -> Result<Vec<String>, WorkflowKernelError> {
        sqlx::query_scalar::<_, String>(
            "select id from workflow_run where status in ('queued','running','waiting') order by updated_unix_ms, id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(persistence)
    }
}

async fn prepare_epoch(path: &Path) -> Result<(), WorkflowKernelError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            WorkflowKernelError::Persistence(format!(
                "create Workflow database directory {}: {error}",
                parent.display()
            ))
        })?;
    }
    if !path.exists()
        || std::fs::metadata(path)
            .map(|metadata| metadata.len() == 0)
            .unwrap_or(true)
    {
        return Ok(());
    }
    let options = super::pools::options(path, false, false)
        .map_err(|error| WorkflowKernelError::Persistence(error.to_string()))?;
    let mut connection = sqlx::SqliteConnection::connect_with(&options)
        .await
        .map_err(persistence)?;
    let identity_exists: i64 = sqlx::query_scalar(
        "select exists(select 1 from sqlite_master where type='table' and name='workflow_database_identity')",
    )
    .fetch_one(&mut connection)
    .await
    .map_err(persistence)?;
    let epoch = if identity_exists == 1 {
        sqlx::query_scalar::<_, i64>(
            "select schema_epoch from workflow_database_identity where singleton=1",
        )
        .fetch_optional(&mut connection)
        .await
        .map_err(persistence)?
    } else {
        None
    };
    if epoch == Some(SCHEMA_EPOCH) {
        connection.close().await.map_err(persistence)?;
        return Ok(());
    }
    if crate::worker::socket_path().exists() {
        connection.close().await.map_err(persistence)?;
        return Err(WorkflowKernelError::Persistence(format!(
            "cannot replace a pre-cutover Workflow database {} while a Prism worker socket exists",
            path.display()
        )));
    }
    let backup = path.with_extension("db.pre-prompt-workflow-backup");
    if !backup.exists() {
        sqlx::query("vacuum into ?")
            .bind(backup.to_string_lossy().into_owned())
            .execute(&mut connection)
            .await
            .map_err(persistence)?;
        super::pools::set_owner_only(&backup)
            .map_err(|error| WorkflowKernelError::Persistence(error.to_string()))?;
    }
    connection.close().await.map_err(persistence)?;
    for candidate in [
        path.to_path_buf(),
        path.with_extension("db-wal"),
        path.with_extension("db-shm"),
    ] {
        match std::fs::remove_file(&candidate) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(WorkflowKernelError::Persistence(format!(
                    "remove incompatible Workflow database {}: {error}",
                    candidate.display()
                )));
            }
        }
    }
    Ok(())
}

impl WorkflowRunStore for DurableWorkflowRunStore {
    fn retain_workflow<'a>(&'a self, workflow: &'a CompiledWorkflow) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            let body = serde_json::to_string(workflow)
                .map_err(|error| WorkflowKernelError::Persistence(error.to_string()))?;
            let changed = sqlx::query("insert into workflow_snapshot(digest, workflow_name, source_path, source_revision, source, body_json, created_unix_ms) values(?,?,?,?,?,?,?) on conflict(digest) do nothing")
                .bind(&workflow.digest)
                .bind(&workflow.name)
                .bind(workflow.source_path.to_string_lossy().into_owned())
                .bind(&workflow.source_revision)
                .bind(&workflow.source)
                .bind(&body)
                .bind(now_ms())
                .execute(&self.pool)
                .await
                .map_err(persistence)?
                .rows_affected();
            if changed == 0 {
                let existing: String =
                    sqlx::query_scalar("select body_json from workflow_snapshot where digest=?")
                        .bind(&workflow.digest)
                        .fetch_one(&self.pool)
                        .await
                        .map_err(persistence)?;
                if existing != body {
                    return Err(WorkflowKernelError::Conflict(format!(
                        "immutable Workflow snapshot {} has different content",
                        workflow.digest
                    )));
                }
            }
            for trigger in workflow
                .steps
                .iter()
                .filter_map(|step| step.trigger.as_ref())
                .filter(|trigger| trigger.executable.is_some())
            {
                let path = trigger.executable.as_ref().expect("filtered executable");
                sqlx::query("insert into trigger_executable_snapshot(digest, trigger_name, executable_path, retained_path, created_unix_ms) values(?,?,?,?,?) on conflict(digest) do update set trigger_name=excluded.trigger_name, executable_path=excluded.executable_path, retained_path=excluded.retained_path")
                    .bind(&trigger.digest)
                    .bind(&trigger.name)
                    .bind(path.to_string_lossy().into_owned())
                    .bind(path.to_string_lossy().into_owned())
                    .bind(now_ms())
                    .execute(&self.pool)
                    .await
                    .map_err(persistence)?;
            }
            Ok(())
        })
    }

    fn load_workflow<'a>(&'a self, digest: &'a str) -> StoreFuture<'a, CompiledWorkflow> {
        Box::pin(async move {
            let body: Option<String> =
                sqlx::query_scalar("select body_json from workflow_snapshot where digest=?")
                    .bind(digest)
                    .fetch_optional(&self.pool)
                    .await
                    .map_err(persistence)?;
            serde_json::from_str(&body.ok_or_else(|| {
                WorkflowKernelError::Persistence(format!(
                    "missing immutable Workflow snapshot {digest}"
                ))
            })?)
            .map_err(|error| WorkflowKernelError::Persistence(error.to_string()))
        })
    }

    fn create_run<'a>(&'a self, run: &'a WorkflowRunState) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            let workflow = self.load_workflow(&run.workflow_digest).await?;
            let body = serde_json::to_string(run)
                .map_err(|error| WorkflowKernelError::Persistence(error.to_string()))?;
            let mut transaction = self.pool.begin().await.map_err(persistence)?;
            sqlx::query("insert into workflow_run(id, workflow_digest, workflow_name, repository, worktree, change_request, change_request_head, status, cycle, max_agent_runs, agent_runs_consumed, cancellation_requested, created_unix_ms, updated_unix_ms, revision, state_json) values(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)")
                .bind(&run.id)
                .bind(&run.workflow_digest)
                .bind(&run.workflow_name)
                .bind(run.subject.repository.to_string_lossy().into_owned())
                .bind(run.subject.worktree.to_string_lossy().into_owned())
                .bind(&run.subject.change_request)
                .bind(&run.subject.change_request_head)
                .bind(json_name(&run.status)?)
                .bind(i64_value(run.cycle)?)
                .bind(i64::from(run.max_agent_runs))
                .bind(i64::from(run.agent_runs_consumed))
                .bind(run.cancellation_requested)
                .bind(run.created_unix_ms)
                .bind(run.updated_unix_ms)
                .bind(i64_value(run.revision)?)
                .bind(body)
                .execute(&mut *transaction)
                .await
                .map_err(persistence)?;
            insert_projection(&mut transaction, &workflow, run).await?;
            transaction.commit().await.map_err(persistence)?;
            Ok(())
        })
    }

    fn load_run<'a>(&'a self, run_id: &'a str) -> StoreFuture<'a, Option<WorkflowRunState>> {
        Box::pin(async move {
            let body: Option<String> =
                sqlx::query_scalar("select state_json from workflow_run where id=?")
                    .bind(run_id)
                    .fetch_optional(&self.pool)
                    .await
                    .map_err(persistence)?;
            body.map(|body| {
                serde_json::from_str(&body)
                    .map_err(|error| WorkflowKernelError::Persistence(error.to_string()))
            })
            .transpose()
        })
    }

    fn save_run<'a>(&'a self, run: &'a mut WorkflowRunState) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            let workflow = self.load_workflow(&run.workflow_digest).await?;
            let expected = run.revision;
            let next = expected.checked_add(1).ok_or_else(|| {
                WorkflowKernelError::Persistence("Workflow Run revision overflow".into())
            })?;
            let mut persisted = run.clone();
            persisted.revision = next;
            let body = serde_json::to_string(&persisted)
                .map_err(|error| WorkflowKernelError::Persistence(error.to_string()))?;
            let mut transaction = self.pool.begin().await.map_err(persistence)?;
            let changed = sqlx::query("update workflow_run set status=?, cycle=?, max_agent_runs=?, agent_runs_consumed=?, cancellation_requested=?, updated_unix_ms=?, revision=?, state_json=? where id=? and revision=?")
                .bind(json_name(&persisted.status)?)
                .bind(i64_value(persisted.cycle)?)
                .bind(i64::from(persisted.max_agent_runs))
                .bind(i64::from(persisted.agent_runs_consumed))
                .bind(persisted.cancellation_requested)
                .bind(persisted.updated_unix_ms)
                .bind(i64_value(next)?)
                .bind(body)
                .bind(&persisted.id)
                .bind(i64_value(expected)?)
                .execute(&mut *transaction)
                .await
                .map_err(persistence)?
                .rows_affected();
            if changed != 1 {
                return Err(WorkflowKernelError::Conflict(format!(
                    "Workflow Run {} changed concurrently",
                    run.id
                )));
            }
            for table in [
                "workflow_run_event",
                "step_lifecycle_attempt",
                "workflow_dependency",
                "workflow_step",
            ] {
                sqlx::query(&format!("delete from {table} where run_id=?"))
                    .bind(&run.id)
                    .execute(&mut *transaction)
                    .await
                    .map_err(persistence)?;
            }
            insert_projection(&mut transaction, &workflow, &persisted).await?;
            transaction.commit().await.map_err(persistence)?;
            run.revision = next;
            Ok(())
        })
    }
}

async fn insert_projection(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    workflow: &CompiledWorkflow,
    run: &WorkflowRunState,
) -> Result<(), WorkflowKernelError> {
    for (index, (compiled, step)) in workflow.steps.iter().zip(&run.steps).enumerate() {
        let index = i64::try_from(index)
            .map_err(|_| WorkflowKernelError::Persistence("Step index overflow".into()))?;
        sqlx::query("insert into workflow_step(run_id, step_index, step_key, trigger_name, phase, summary, wake_at_unix_ms, satisfied_cycle, unconditional_completed) values(?,?,?,?,?,?,?,?,?)")
            .bind(&run.id)
            .bind(index)
            .bind(&step.key)
            .bind(compiled.trigger.as_ref().map(|trigger| &trigger.name))
            .bind(json_name(&step.phase)?)
            .bind(&step.summary)
            .bind(step.wake_at_unix_ms)
            .bind(step.satisfied_cycle.map(i64_value).transpose()?)
            .bind(step.unconditional_completed)
            .execute(&mut **transaction)
            .await
            .map_err(persistence)?;
        for dependency in &compiled.dependencies {
            sqlx::query(
                "insert into workflow_dependency(run_id, step_index, dependency_key) values(?,?,?)",
            )
            .bind(&run.id)
            .bind(index)
            .bind(dependency)
            .execute(&mut **transaction)
            .await
            .map_err(persistence)?;
        }
        for attempt in &step.attempts {
            let prepared = attempt
                .prepared_state
                .as_ref()
                .map(serde_json::to_string)
                .transpose()
                .map_err(|error| WorkflowKernelError::Persistence(error.to_string()))?;
            sqlx::query("insert into step_lifecycle_attempt(id, run_id, step_index, attempt_number, status, phase, prepared_state_json, agent_status, agent_process_id, agent_session_id, agent_final_text, agent_turn_in_flight, error, started_unix_ms, finished_unix_ms, fencing_token, phase_owner, lease_expires_unix_ms) values(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)")
                .bind(&attempt.id)
                .bind(&run.id)
                .bind(index)
                .bind(i64::from(attempt.number))
                .bind(json_name(&attempt.status)?)
                .bind(json_name(&attempt.phase)?)
                .bind(prepared)
                .bind(attempt.agent_outcome.as_ref().map(|outcome| json_name(&outcome.status)).transpose()?)
                .bind(attempt.agent_outcome.as_ref().and_then(|outcome| outcome.process_id).map(i64::from))
                .bind(attempt.agent_outcome.as_ref().map(|outcome| &outcome.session_id))
                .bind(attempt.agent_outcome.as_ref().map(|outcome| &outcome.final_text))
                .bind(attempt.agent_turn_in_flight.map(i64::from))
                .bind(&attempt.error)
                .bind(attempt.started_unix_ms)
                .bind(attempt.finished_unix_ms)
                .bind(i64_value(attempt.fencing_token)?)
                .bind(&attempt.phase_owner)
                .bind(attempt.lease_expires_unix_ms)
                .execute(&mut **transaction)
                .await
                .map_err(persistence)?;
            for (turn_index, turn) in attempt.agent_turns.iter().enumerate() {
                let turn_number = i64::try_from(turn_index + 1).map_err(|_| {
                    WorkflowKernelError::Persistence("Agent turn index overflow".into())
                })?;
                sqlx::query("insert into agent_turn(attempt_id, turn_number, process_id, session_id, final_text) values(?,?,?,?,?)")
                    .bind(&attempt.id)
                    .bind(turn_number)
                    .bind(turn.process_id.map(i64::from))
                    .bind(&turn.session_id)
                    .bind(&turn.final_text)
                    .execute(&mut **transaction)
                    .await
                    .map_err(persistence)?;
            }
        }
    }
    for event in &run.events {
        sqlx::query("insert into workflow_run_event(run_id, sequence, time_unix_ms, step_key, attempt_id, kind, summary) values(?,?,?,?,?,?,?)")
            .bind(&run.id)
            .bind(i64_value(event.sequence)?)
            .bind(event.time_unix_ms)
            .bind(&event.step_key)
            .bind(&event.attempt_id)
            .bind(&event.kind)
            .bind(&event.summary)
            .execute(&mut **transaction)
            .await
            .map_err(persistence)?;
    }
    Ok(())
}

fn json_name(value: &impl serde::Serialize) -> Result<String, WorkflowKernelError> {
    let value = serde_json::to_string(value)
        .map_err(|error| WorkflowKernelError::Persistence(error.to_string()))?;
    Ok(value.trim_matches('"').to_string())
}

fn i64_value(value: u64) -> Result<i64, WorkflowKernelError> {
    i64::try_from(value)
        .map_err(|_| WorkflowKernelError::Persistence("integer exceeds SQLite range".into()))
}

fn persistence(error: sqlx::Error) -> WorkflowKernelError {
    WorkflowKernelError::Persistence(error.to_string())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::kernel::WorkflowRunStatus;
    use crate::workflow::source::{TriggerCatalog, compile_workflow};
    use crate::workflow::step_trigger::TriggerSubject;

    #[test]
    fn durable_store_open_waits_for_a_transient_write_lock() {
        let root = std::env::temp_dir().join(format!(
            "prism-workflow-open-lock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = root.join("workflow.db");
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let store = DurableWorkflowRunStore::open(&path).await.unwrap();
                store.close().await;

                let mut blocker = sqlx::SqliteConnection::connect_with(
                    &super::super::pools::options(&path, false, false).unwrap(),
                )
                .await
                .unwrap();
                sqlx::query("begin immediate")
                    .execute(&mut blocker)
                    .await
                    .unwrap();
                sqlx::query(
                    "update workflow_database_identity set schema_epoch = schema_epoch where singleton = 1",
                )
                .execute(&mut blocker)
                .await
                .unwrap();

                let mut reopening = Box::pin(DurableWorkflowRunStore::open(&path));
                assert!(
                    tokio::time::timeout(std::time::Duration::from_millis(50), &mut reopening)
                        .await
                        .is_err(),
                    "workflow open should remain pending while the SQLite lock is held"
                );

                sqlx::query("commit")
                    .execute(&mut blocker)
                    .await
                    .unwrap();
                blocker.close().await.unwrap();

                let reopened = tokio::time::timeout(super::super::pools::WRITER_BUSY_TIMEOUT, reopening)
                    .await
                    .expect("workflow open should finish after the SQLite lock is released")
                    .expect("workflow open should wait for a transient SQLite lock");
                reopened.close().await;
            });
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn durable_store_round_trips_compact_run_projection() {
        let root = std::env::temp_dir().join(format!(
            "prism-prompt-ledger-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let store = DurableWorkflowRunStore::open(&root.join("workflow.db"))
            .await
            .unwrap();
        let workflow = compile_workflow(
            Path::new("test.toml"),
            "[[step]]\nprompt='run'\n",
            &TriggerCatalog::builtins(),
        )
        .unwrap();
        store.retain_workflow(&workflow).await.unwrap();
        let run = WorkflowRunState {
            id: "run".into(),
            workflow_digest: workflow.digest.clone(),
            workflow_name: workflow.name.clone(),
            subject: TriggerSubject {
                repository: "/repo".into(),
                worktree: "/repo/wt".into(),
                change_request: None,
                change_request_head: None,
            },
            status: WorkflowRunStatus::Queued,
            cycle: 1,
            cycle_started_unix_ms: 1,
            max_agent_runs: 10,
            agent_runs_consumed: 0,
            cancellation_requested: false,
            created_unix_ms: 1,
            updated_unix_ms: 1,
            revision: 0,
            steps: vec![crate::workflow::kernel::WorkflowStepState {
                key: "step-1".into(),
                dependencies: Vec::new(),
                explicit_dependencies: false,
                phase: crate::workflow::kernel::StepPhase::Pending,
                summary: None,
                wake_at_unix_ms: None,
                satisfied_cycle: None,
                unconditional_completed: false,
                attempts: Vec::new(),
            }],
            events: Vec::new(),
        };
        store.create_run(&run).await.unwrap();
        assert_eq!(store.load_run("run").await.unwrap(), Some(run));
        store.close().await;
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn durable_projection_records_each_completed_agent_turn() {
        use crate::workflow::agent_phase::RecordingAgentExecutor;
        use crate::workflow::kernel::{SchedulerProgress, StartPromptWorkflow, WorkflowScheduler};
        use crate::workflow::step_trigger::{AgentOutcome, AgentOutcomeStatus, TriggerRegistry};
        use std::sync::Arc;

        let root = std::env::temp_dir().join(format!(
            "prism-prompt-turns-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let store = Arc::new(
            DurableWorkflowRunStore::open(&root.join("workflow.db"))
                .await
                .unwrap(),
        );
        let workflow = compile_workflow(
            Path::new("followups.toml"),
            "[[step]]\nprompt='audit'\nfollowups=['implement gaps']\n",
            &TriggerCatalog::builtins(),
        )
        .unwrap();
        let agents = Arc::new(RecordingAgentExecutor::default());
        for text in ["found a gap", "implemented"] {
            agents.push_outcome(AgentOutcome {
                status: AgentOutcomeStatus::Succeeded,
                process_id: Some(42),
                session_id: "shared".into(),
                final_text: text.into(),
            });
        }
        let scheduler = WorkflowScheduler::new(store.clone(), TriggerRegistry::default(), agents);
        scheduler
            .start(StartPromptWorkflow {
                run_id: "run",
                workflow: &workflow,
                subject: TriggerSubject {
                    repository: "/repo".into(),
                    worktree: "/repo/wt".into(),
                    change_request: None,
                    change_request_head: None,
                },
                now_unix_ms: 1,
            })
            .await
            .unwrap();
        assert_eq!(
            scheduler.tick("run", 2).await.unwrap(),
            SchedulerProgress::Advanced
        );
        let turns = sqlx::query_as::<_, (i64, String, String)>(
            "select turn_number, session_id, final_text from agent_turn order by turn_number",
        )
        .fetch_all(&store.pool)
        .await
        .unwrap();
        assert_eq!(
            turns,
            vec![
                (1, "shared".into(), "found a gap".into()),
                (2, "shared".into(), "implemented".into()),
            ]
        );
        drop(scheduler);
        Arc::try_unwrap(store).ok().unwrap().close().await;
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn wait_survives_store_and_scheduler_restart_with_fake_time() {
        use crate::workflow::agent_phase::RecordingAgentExecutor;
        use crate::workflow::kernel::{SchedulerProgress, StartPromptWorkflow, WorkflowScheduler};
        use crate::workflow::step_trigger::{ScriptedTrigger, TriggerDecision, TriggerRegistry};
        use std::sync::Arc;

        let root = std::env::temp_dir().join(format!(
            "prism-prompt-restart-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("workflow.db");
        let workflow = compile_workflow(
            Path::new("ready.toml"),
            "[[step]]\ntrigger='ready_to_merge'\n",
            &TriggerCatalog::builtins(),
        )
        .unwrap();
        let trigger = ScriptedTrigger::new([
            TriggerDecision::Wait {
                summary: "checks running".into(),
                wake_at_unix_ms: 20,
            },
            TriggerDecision::Satisfied {
                summary: "ready".into(),
            },
        ]);
        let first_store = Arc::new(DurableWorkflowRunStore::open(&path).await.unwrap());
        let first_registry = TriggerRegistry::default();
        first_registry
            .insert("ready_to_merge", trigger.clone())
            .unwrap();
        let first = WorkflowScheduler::new(
            first_store.clone(),
            first_registry,
            Arc::new(RecordingAgentExecutor::default()),
        );
        first
            .start(StartPromptWorkflow {
                run_id: "run",
                workflow: &workflow,
                subject: TriggerSubject {
                    repository: "/repo".into(),
                    worktree: "/repo/wt".into(),
                    change_request: Some("cr:1".into()),
                    change_request_head: Some("abc".into()),
                },
                now_unix_ms: 1,
            })
            .await
            .unwrap();
        assert_eq!(
            first.tick("run", 2).await.unwrap(),
            SchedulerProgress::Waiting
        );
        drop(first);
        Arc::try_unwrap(first_store).ok().unwrap().close().await;

        let second_store = Arc::new(DurableWorkflowRunStore::open(&path).await.unwrap());
        let second_registry = TriggerRegistry::default();
        second_registry.insert("ready_to_merge", trigger).unwrap();
        let second = WorkflowScheduler::new(
            second_store.clone(),
            second_registry,
            Arc::new(RecordingAgentExecutor::default()),
        );
        assert_eq!(
            second.tick("run", 19).await.unwrap(),
            SchedulerProgress::Waiting
        );
        assert_eq!(
            second.tick("run", 20).await.unwrap(),
            SchedulerProgress::Succeeded
        );
        drop(second);
        Arc::try_unwrap(second_store).ok().unwrap().close().await;
        std::fs::remove_dir_all(root).unwrap();
    }
}
