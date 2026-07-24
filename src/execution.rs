use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

const DEFAULT_LEASE_MS: i64 = 15_000;
static ID_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WorkflowKind {
    Auto,
    Plan,
}

impl WorkflowKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Plan => "plan",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "auto" => Ok(Self::Auto),
            "plan" => Ok(Self::Plan),
            other => Err(format!("unknown workflow kind: {other}")),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DispatchState {
    Queued,
    Claimed,
    RecoveryPending,
    Paused,
    Terminal,
}

impl DispatchState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Claimed => "claimed",
            Self::RecoveryPending => "recovery_pending",
            Self::Paused => "paused",
            Self::Terminal => "terminal",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "queued" => Ok(Self::Queued),
            "claimed" => Ok(Self::Claimed),
            "recovery_pending" => Ok(Self::RecoveryPending),
            "paused" => Ok(Self::Paused),
            "terminal" => Ok(Self::Terminal),
            other => Err(format!("unknown dispatch state: {other}")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WorkflowIdentity {
    pub kind: WorkflowKind,
    pub run_id: String,
}

impl WorkflowIdentity {
    pub fn new(kind: WorkflowKind, run_id: impl Into<String>) -> Self {
        Self {
            kind,
            run_id: run_id.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionClaim {
    pub workflow: WorkflowIdentity,
    pub worker_id: String,
    pub daemon_instance_id: String,
    pub fencing_token: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryCandidate {
    pub workflow: WorkflowIdentity,
    pub repo_root: PathBuf,
    pub worktree: PathBuf,
    pub branch: String,
    pub active_step: String,
    pub last_heartbeat_unix_ms: Option<i64>,
    pub interruption_generation: i64,
}

pub fn new_instance_id(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}-{}",
        std::process::id(),
        now_ms(),
        ID_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

pub fn migrate_schema(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "
        create table if not exists workflow_execution (
          workflow_kind text not null,
          run_id text not null,
          dispatch_state text not null,
          worker_id text,
          daemon_instance_id text,
          lease_expires_unix_ms integer,
          heartbeat_unix_ms integer,
          fencing_token integer not null default 0,
          executor_pid integer,
          executor_process_identity text,
          requeue_requested integer not null default 0,
          interruption_generation integer not null default 0,
          recovery_decided_unix_ms integer,
          created_unix_ms integer not null,
          updated_unix_ms integer not null,
          primary key (workflow_kind, run_id),
          check (workflow_kind in ('auto', 'plan')),
          check (dispatch_state in ('queued', 'claimed', 'recovery_pending', 'paused', 'terminal'))
        );
        create index if not exists workflow_execution_dispatch_idx
          on workflow_execution(dispatch_state, updated_unix_ms);
        create index if not exists workflow_execution_lease_idx
          on workflow_execution(dispatch_state, lease_expires_unix_ms);
        create index if not exists workflow_execution_daemon_idx
          on workflow_execution(daemon_instance_id, dispatch_state);

        insert or ignore into workflow_execution (
          workflow_kind, run_id, dispatch_state, fencing_token,
          interruption_generation, created_unix_ms, updated_unix_ms
        )
        select 'plan', id,
          case status
            when 'queued' then 'recovery_pending'
            when 'running' then 'recovery_pending'
            when 'paused' then 'paused'
            when 'draft' then 'paused'
            else 'terminal'
          end,
          0,
          case when status in ('queued', 'running') then 1 else 0 end,
          created_unix_ms, updated_unix_ms
        from plan_run;

        insert or ignore into workflow_execution (
          workflow_kind, run_id, dispatch_state, fencing_token,
          interruption_generation, created_unix_ms, updated_unix_ms
        )
        select 'auto', id,
          case status
            when 'queued' then 'recovery_pending'
            when 'running' then 'recovery_pending'
            when 'paused' then 'paused'
            else 'terminal'
          end,
          0,
          case when status in ('queued', 'running') then 1 else 0 end,
          created_unix_ms, updated_unix_ms
        from auto_run;
        ",
    )
    .map_err(|error| format!("create workflow execution schema: {error}"))?;
    if !table_has_column(conn, "workflow_execution", "requeue_requested")? {
        conn.execute(
            "alter table workflow_execution
             add column requeue_requested integer not null default 0",
            [],
        )
        .map_err(|error| format!("add workflow requeue intent: {error}"))?;
    }
    Ok(())
}

pub fn enqueue(conn: &Connection, workflow: &WorkflowIdentity) -> Result<(), String> {
    let now = now_ms();
    let changed = conn
        .execute(
            "insert into workflow_execution (
               workflow_kind, run_id, dispatch_state, fencing_token,
               interruption_generation, created_unix_ms, updated_unix_ms
             ) values (?1, ?2, 'queued', 0, 0, ?3, ?3)
             on conflict(workflow_kind, run_id) do update set
               dispatch_state = 'queued',
               worker_id = null,
               daemon_instance_id = null,
               lease_expires_unix_ms = null,
               heartbeat_unix_ms = null,
               executor_pid = null,
               executor_process_identity = null,
               requeue_requested = 0,
               recovery_decided_unix_ms = null,
               fencing_token = workflow_execution.fencing_token + 1,
               updated_unix_ms = excluded.updated_unix_ms
             where workflow_execution.dispatch_state != 'claimed'",
            params![workflow.kind.label(), workflow.run_id, now],
        )
        .map_err(|error| format!("enqueue workflow: {error}"))?;
    if changed == 0 {
        let requested = conn
            .execute(
                "update workflow_execution set requeue_requested = 1, updated_unix_ms = ?1
                 where workflow_kind = ?2 and run_id = ?3 and dispatch_state = 'claimed'",
                params![now, workflow.kind.label(), workflow.run_id],
            )
            .map_err(|error| format!("request workflow requeue: {error}"))?;
        if requested == 0 {
            return Err("workflow could not be queued".to_string());
        }
    }
    Ok(())
}

pub fn dispatch_state(
    conn: &Connection,
    workflow: &WorkflowIdentity,
) -> Result<Option<DispatchState>, String> {
    conn.query_row(
        "select dispatch_state from workflow_execution
         where workflow_kind = ?1 and run_id = ?2",
        params![workflow.kind.label(), workflow.run_id],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map_err(|error| format!("load workflow dispatch state: {error}"))?
    .map(|state| DispatchState::parse(&state))
    .transpose()
}

pub fn queued(conn: &Connection, limit: usize) -> Result<Vec<WorkflowIdentity>, String> {
    let mut statement = conn
        .prepare(
            "select workflow_kind, run_id from workflow_execution
             where dispatch_state = 'queued'
             order by created_unix_ms, workflow_kind, run_id
             limit ?1",
        )
        .map_err(|error| format!("prepare queued workflow query: {error}"))?;
    let rows = statement
        .query_map([limit as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| format!("query queued workflows: {error}"))?;
    let mut workflows = Vec::new();
    for row in rows {
        let (kind, run_id) = row.map_err(|error| format!("read queued workflow: {error}"))?;
        workflows.push(WorkflowIdentity::new(WorkflowKind::parse(&kind)?, run_id));
    }
    Ok(workflows)
}

pub fn claim(
    conn: &mut Connection,
    workflow: &WorkflowIdentity,
    daemon_instance_id: &str,
    worker_id: &str,
) -> Result<Option<ExecutionClaim>, String> {
    claim_with_lease(
        conn,
        workflow,
        daemon_instance_id,
        worker_id,
        DEFAULT_LEASE_MS,
    )
}

fn claim_with_lease(
    conn: &mut Connection,
    workflow: &WorkflowIdentity,
    daemon_instance_id: &str,
    worker_id: &str,
    lease_ms: i64,
) -> Result<Option<ExecutionClaim>, String> {
    let now = now_ms();
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| format!("begin workflow claim: {error}"))?;
    let changed = tx
        .execute(
            "update workflow_execution set
               dispatch_state = 'claimed', worker_id = ?1, daemon_instance_id = ?2,
               heartbeat_unix_ms = ?3, lease_expires_unix_ms = ?4,
               fencing_token = fencing_token + 1, executor_pid = ?5,
               executor_process_identity = ?6,
               updated_unix_ms = ?3
             where workflow_kind = ?7 and run_id = ?8 and dispatch_state = 'queued'",
            params![
                worker_id,
                daemon_instance_id,
                now,
                now.saturating_add(lease_ms),
                i64::from(std::process::id()),
                worker_id,
                workflow.kind.label(),
                workflow.run_id,
            ],
        )
        .map_err(|error| format!("claim workflow: {error}"))?;
    if changed == 0 {
        tx.commit()
            .map_err(|error| format!("commit empty workflow claim: {error}"))?;
        return Ok(None);
    }
    let token = tx
        .query_row(
            "select fencing_token from workflow_execution
             where workflow_kind = ?1 and run_id = ?2",
            params![workflow.kind.label(), workflow.run_id],
            |row| row.get(0),
        )
        .map_err(|error| format!("load workflow fencing token: {error}"))?;
    tx.commit()
        .map_err(|error| format!("commit workflow claim: {error}"))?;
    Ok(Some(ExecutionClaim {
        workflow: workflow.clone(),
        worker_id: worker_id.to_string(),
        daemon_instance_id: daemon_instance_id.to_string(),
        fencing_token: token,
    }))
}

pub fn heartbeat(conn: &Connection, claim: &ExecutionClaim) -> Result<(), String> {
    let now = now_ms();
    let changed = conn
        .execute(
            "update workflow_execution set heartbeat_unix_ms = ?1,
               lease_expires_unix_ms = ?2, updated_unix_ms = ?1
             where workflow_kind = ?3 and run_id = ?4 and dispatch_state = 'claimed'
               and worker_id = ?5 and daemon_instance_id = ?6 and fencing_token = ?7
               and lease_expires_unix_ms > ?1",
            params![
                now,
                now.saturating_add(DEFAULT_LEASE_MS),
                claim.workflow.kind.label(),
                claim.workflow.run_id,
                claim.worker_id,
                claim.daemon_instance_id,
                claim.fencing_token,
            ],
        )
        .map_err(|error| format!("heartbeat workflow claim: {error}"))?;
    if changed == 1 {
        return Ok(());
    }
    conn.execute(
        "update workflow_execution set dispatch_state = 'recovery_pending',
           worker_id = null, daemon_instance_id = null, lease_expires_unix_ms = null,
           requeue_requested = 0,
           interruption_generation = interruption_generation + 1,
           fencing_token = fencing_token + 1, updated_unix_ms = ?1
         where workflow_kind = ?2 and run_id = ?3 and dispatch_state = 'claimed'
           and worker_id = ?4 and daemon_instance_id = ?5 and fencing_token = ?6
           and lease_expires_unix_ms <= ?1",
        params![
            now,
            claim.workflow.kind.label(),
            claim.workflow.run_id,
            claim.worker_id,
            claim.daemon_instance_id,
            claim.fencing_token,
        ],
    )
    .map_err(|error| format!("expire workflow claim: {error}"))?;
    Err(stale_claim_error())
}

pub fn validate_claim(conn: &Connection, claim: &ExecutionClaim) -> Result<(), String> {
    let current = conn
        .query_row(
            "select 1 from workflow_execution
             where workflow_kind = ?1 and run_id = ?2 and dispatch_state = 'claimed'
               and worker_id = ?3 and daemon_instance_id = ?4 and fencing_token = ?5
               and lease_expires_unix_ms > ?6",
            params![
                claim.workflow.kind.label(),
                claim.workflow.run_id,
                claim.worker_id,
                claim.daemon_instance_id,
                claim.fencing_token,
                now_ms(),
            ],
            |_| Ok(()),
        )
        .optional()
        .map_err(|error| format!("validate workflow claim: {error}"))?;
    current.ok_or_else(stale_claim_error)
}

pub fn install_claim_guards(conn: &Connection, claim: &ExecutionClaim) -> Result<(), String> {
    validate_claim(conn, claim)?;
    conn.execute_batch(
        "create temp table if not exists _prism_execution_claim_guard (
           workflow_kind text not null,
           run_id text not null,
           worker_id text not null,
           daemon_instance_id text not null,
           fencing_token integer not null
         );
         delete from _prism_execution_claim_guard;",
    )
    .map_err(|error| format!("initialize execution claim guards: {error}"))?;
    conn.execute(
        "insert into _prism_execution_claim_guard
         (workflow_kind, run_id, worker_id, daemon_instance_id, fencing_token)
         values (?1, ?2, ?3, ?4, ?5)",
        params![
            claim.workflow.kind.label(),
            claim.workflow.run_id,
            claim.worker_id,
            claim.daemon_instance_id,
            claim.fencing_token,
        ],
    )
    .map_err(|error| format!("store execution claim guard: {error}"))?;

    let current_claim = "exists (
           select 1
           from workflow_execution e, _prism_execution_claim_guard g
           where e.workflow_kind = g.workflow_kind
             and e.run_id = g.run_id
             and e.dispatch_state = 'claimed'
             and e.worker_id = g.worker_id
             and e.daemon_instance_id = g.daemon_instance_id
             and e.fencing_token = g.fencing_token
             and e.lease_expires_unix_ms > cast(unixepoch('now', 'subsec') * 1000 as integer)
         )";
    let guarded_tables = [
        ("plan_run", "id", true, false),
        ("plan_step_run", "run_id", true, false),
        ("plan_output_line", "run_id", true, false),
        ("auto_run", "id", false, false),
        ("auto_step_run", "run_id", false, false),
        ("auto_output_line", "step_run_id", false, true),
        ("auto_event", "run_id", false, false),
    ];
    for (table, key, plan_table, output_table) in guarded_tables {
        for operation in ["insert", "update", "delete"] {
            let rows = match operation {
                "insert" => vec!["new"],
                "delete" => vec!["old"],
                _ => vec!["old", "new"],
            };
            let ownership = rows
                .into_iter()
                .map(|row| {
                    if plan_table {
                        format!(
                            "exists (
                               select 1 from _prism_execution_claim_guard g
                               where (g.workflow_kind = 'plan' and g.run_id = {row}.{key})
                                  or (g.workflow_kind = 'auto' and exists (
                                    select 1 from auto_step_run s
                                    where s.run_id = g.run_id and s.plan_run_id = {row}.{key}
                                  ))
                             )"
                        )
                    } else if output_table {
                        format!(
                            "exists (
                               select 1 from _prism_execution_claim_guard g
                               join auto_step_run s on s.run_id = g.run_id
                               where g.workflow_kind = 'auto' and s.id = {row}.{key}
                             )"
                        )
                    } else {
                        format!(
                            "exists (
                               select 1 from _prism_execution_claim_guard g
                               where g.workflow_kind = 'auto' and g.run_id = {row}.{key}
                             )"
                        )
                    }
                })
                .collect::<Vec<_>>()
                .join(" and ");
            let trigger = format!(
                "drop trigger if exists temp._prism_guard_{table}_{operation};
                 create temp trigger _prism_guard_{table}_{operation}
                 before {operation} on main.{table}
                 when not (({ownership}) and ({current_claim}))
                 begin
                   select raise(abort, 'execution claim is stale');
                 end;"
            );
            conn.execute_batch(&trigger)
                .map_err(|error| format!("install execution claim guard for {table}: {error}"))?;
        }
    }
    Ok(())
}

pub fn validate_installed_claim(conn: &Connection) -> Result<(), String> {
    let installed = conn
        .query_row(
            "select 1 from temp.sqlite_temp_master
             where type = 'table' and name = '_prism_execution_claim_guard'",
            [],
            |_| Ok(()),
        )
        .optional()
        .map_err(|error| format!("inspect installed execution claim: {error}"))?;
    if installed.is_none() {
        return Ok(());
    }
    let current = conn
        .query_row(
            "select 1
             from workflow_execution e, temp._prism_execution_claim_guard g
             where e.workflow_kind = g.workflow_kind
               and e.run_id = g.run_id
               and e.dispatch_state = 'claimed'
               and e.worker_id = g.worker_id
               and e.daemon_instance_id = g.daemon_instance_id
               and e.fencing_token = g.fencing_token
               and e.lease_expires_unix_ms > ?1",
            [now_ms()],
            |_| Ok(()),
        )
        .optional()
        .map_err(|error| format!("validate installed execution claim: {error}"))?;
    current.ok_or_else(stale_claim_error)
}

pub fn release(
    conn: &Connection,
    claim: &ExecutionClaim,
    state: DispatchState,
) -> Result<(), String> {
    if matches!(
        state,
        DispatchState::Claimed | DispatchState::RecoveryPending
    ) {
        return Err("invalid executor release state".to_string());
    }
    let changed = conn
        .execute(
            "update workflow_execution set
                dispatch_state = case when requeue_requested = 1 then 'queued' else ?1 end,
                worker_id = null, daemon_instance_id = null,
                lease_expires_unix_ms = null, executor_pid = null,
                executor_process_identity = null, requeue_requested = 0, updated_unix_ms = ?2
             where workflow_kind = ?3 and run_id = ?4 and dispatch_state = 'claimed'
                and worker_id = ?5 and daemon_instance_id = ?6 and fencing_token = ?7
                and lease_expires_unix_ms > ?2",
            params![
                state.label(),
                now_ms(),
                claim.workflow.kind.label(),
                claim.workflow.run_id,
                claim.worker_id,
                claim.daemon_instance_id,
                claim.fencing_token,
            ],
        )
        .map_err(|error| format!("release workflow claim: {error}"))?;
    require_current_claim(changed)
}

pub fn mark_abandoned(conn: &Connection, daemon_instance_id: &str) -> Result<usize, String> {
    let now = now_ms();
    conn.execute(
        "update workflow_execution set dispatch_state = 'recovery_pending',
           worker_id = null, daemon_instance_id = null, lease_expires_unix_ms = null,
           executor_pid = null, executor_process_identity = null,
           requeue_requested = 0,
           interruption_generation = interruption_generation + 1,
           fencing_token = fencing_token + 1, updated_unix_ms = ?1
          where dispatch_state = 'claimed'
            and (daemon_instance_id != ?2 or lease_expires_unix_ms <= ?1)",
        params![now, daemon_instance_id],
    )
    .map_err(|error| format!("mark abandoned workflows: {error}"))
}

pub fn recovery_candidates(conn: &Connection) -> Result<Vec<RecoveryCandidate>, String> {
    let mut candidates = Vec::new();
    let mut statement = conn
        .prepare(
            "select e.workflow_kind, e.run_id, e.heartbeat_unix_ms,
                    e.interruption_generation,
                    coalesce(a.repo_root, p.repo_root),
                    coalesce(a.worktree_path, p.scope_path),
                    coalesce(a.branch, p.plan_display),
                    case e.workflow_kind
                      when 'auto' then coalesce((
                        select s.step_key from auto_step_run s
                        where s.run_id = e.run_id
                        order by s.sequence desc limit 1
                      ), 'Auto Flow')
                      else coalesce((
                        select p2.step_name || ' ' || s.step || ' of ' || p2.total_steps
                        from plan_step_run s join plan_run p2 on p2.id = s.run_id
                        where s.run_id = e.run_id
                        order by s.step desc limit 1
                      ), 'Plan')
                    end
             from workflow_execution e
             left join auto_run a on e.workflow_kind = 'auto' and a.id = e.run_id
             left join plan_run p on e.workflow_kind = 'plan' and p.id = e.run_id
             where e.dispatch_state = 'recovery_pending'
             order by coalesce(a.repo_root, p.repo_root), e.workflow_kind, e.run_id",
        )
        .map_err(|error| format!("prepare recovery candidate query: {error}"))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
            ))
        })
        .map_err(|error| format!("query recovery candidates: {error}"))?;
    for row in rows {
        let (kind, run_id, heartbeat, generation, repo, worktree, branch, active_step) =
            row.map_err(|error| format!("read recovery candidate: {error}"))?;
        candidates.push(RecoveryCandidate {
            workflow: WorkflowIdentity::new(WorkflowKind::parse(&kind)?, run_id),
            repo_root: PathBuf::from(repo),
            worktree: PathBuf::from(worktree),
            branch,
            active_step,
            last_heartbeat_unix_ms: heartbeat,
            interruption_generation: generation,
        });
    }
    Ok(candidates)
}

pub fn apply_recovery_decision(
    conn: &mut Connection,
    decisions: &[(WorkflowIdentity, i64, bool)],
) -> Result<(), String> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| format!("begin recovery decision: {error}"))?;
    let now = now_ms();
    for (workflow, generation, _) in decisions {
        let current = tx
            .query_row(
                "select 1 from workflow_execution
                 where workflow_kind = ?1 and run_id = ?2
                   and dispatch_state = 'recovery_pending'
                   and interruption_generation = ?3",
                params![workflow.kind.label(), workflow.run_id, generation],
                |_| Ok(()),
            )
            .optional()
            .map_err(|error| format!("validate recovery decision: {error}"))?;
        if current.is_none() {
            return Err(format!(
                "recovery state changed for {} run {}",
                workflow.kind.label(),
                workflow.run_id
            ));
        }
    }
    for (workflow, _, _) in decisions {
        terminate_recorded_processes(&tx, workflow)?;
    }
    for (workflow, generation, selected) in decisions {
        let state = if *selected { "queued" } else { "paused" };
        let changed = tx
            .execute(
                "update workflow_execution set dispatch_state = ?1,
                   recovery_decided_unix_ms = ?2, updated_unix_ms = ?2,
                   requeue_requested = 0,
                   fencing_token = fencing_token + 1,
                   interruption_generation = interruption_generation + 1
                 where workflow_kind = ?3 and run_id = ?4
                   and dispatch_state = 'recovery_pending'
                   and interruption_generation = ?5",
                params![
                    state,
                    now,
                    workflow.kind.label(),
                    workflow.run_id,
                    generation,
                ],
            )
            .map_err(|error| format!("apply recovery decision: {error}"))?;
        if changed != 1 {
            return Err(format!(
                "recovery state changed for {} run {}",
                workflow.kind.label(),
                workflow.run_id
            ));
        }
        match workflow.kind {
            WorkflowKind::Auto => {
                if *selected {
                    tx.execute(
                        "update auto_step_run set status = 'queued', started_unix_ms = null,
                           finished_unix_ms = null, execution_state = null,
                           execution_process_id = null, execution_process_start_time_ticks = null,
                           process_id = null, error = null
                         where run_id = ?1 and status in ('starting', 'running', 'waiting')",
                        [&workflow.run_id],
                    )
                    .map_err(|error| format!("reset interrupted Auto Flow step: {error}"))?;
                }
                tx.execute(
                    "update auto_run set status = ?1, pause_requested = 0,
                       updated_unix_ms = ?2 where id = ?3",
                    params![state, now, workflow.run_id],
                )
                .map_err(|error| format!("update recovered Auto Flow run: {error}"))?;
            }
            WorkflowKind::Plan => {
                if *selected {
                    tx.execute(
                        "update plan_step_run set status = 'queued', started_unix_ms = null,
                           finished_unix_ms = null, execution_state = null,
                           execution_process_id = null, execution_process_start_time_ticks = null,
                           process_id = null, error = null
                         where run_id = ?1 and status in ('starting', 'running')",
                        [&workflow.run_id],
                    )
                    .map_err(|error| format!("reset interrupted Plan step: {error}"))?;
                }
                tx.execute(
                    "update plan_run set status = ?1, pause_requested = 0,
                       updated_unix_ms = ?2 where id = ?3",
                    params![state, now, workflow.run_id],
                )
                .map_err(|error| format!("update recovered Plan run: {error}"))?;
            }
        }
    }
    tx.commit()
        .map_err(|error| format!("commit recovery decision: {error}"))
}

fn terminate_recorded_processes(
    conn: &Connection,
    workflow: &WorkflowIdentity,
) -> Result<(), String> {
    let table = match workflow.kind {
        WorkflowKind::Auto => "auto_step_run",
        WorkflowKind::Plan => "plan_step_run",
    };
    let mut statement = conn
        .prepare(&format!(
            "select distinct execution_process_id, execution_process_start_time_ticks
             from {table}
             where run_id = ?1 and execution_process_id is not null"
        ))
        .map_err(|error| format!("prepare interrupted process query: {error}"))?;
    let rows = statement
        .query_map([&workflow.run_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?))
        })
        .map_err(|error| format!("query interrupted processes: {error}"))?;
    let mut processes = Vec::new();
    for row in rows {
        processes.push(row.map_err(|error| format!("read interrupted process: {error}"))?);
    }
    drop(statement);
    for (process_id, start_time_ticks) in processes {
        let Ok(process_id) = u32::try_from(process_id) else {
            continue;
        };
        let start_time_ticks = start_time_ticks.and_then(|ticks| u64::try_from(ticks).ok());
        if start_time_ticks.is_none() {
            let result = unsafe { libc::kill(process_id as libc::pid_t, 0) };
            if result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) {
                return Err(format!(
                    "interrupted {} run {} is blocked by live process {process_id} without a reusable process identity",
                    workflow.kind.label(),
                    workflow.run_id
                ));
            }
            continue;
        }
        crate::harness::terminate_process(process_id, start_time_ticks)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if crate::harness::process_start_time_ticks(process_id) != start_time_ticks {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        if crate::harness::process_start_time_ticks(process_id) == start_time_ticks {
            return Err(format!(
                "interrupted {} run {} is blocked because process {process_id} did not exit",
                workflow.kind.label(),
                workflow.run_id
            ));
        }
    }
    Ok(())
}

fn require_current_claim(changed: usize) -> Result<(), String> {
    if changed == 1 {
        Ok(())
    } else {
        Err(stale_claim_error())
    }
}

fn stale_claim_error() -> String {
    "execution claim is stale".to_string()
}

pub fn is_stale_claim_error(error: &str) -> bool {
    error == "execution claim is stale"
}

fn table_has_column(conn: &Connection, table: &str, column: &str) -> Result<bool, String> {
    let mut statement = conn
        .prepare(&format!("pragma table_info({table})"))
        .map_err(|error| format!("inspect {table} schema: {error}"))?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| format!("query {table} schema: {error}"))?;
    for row in rows {
        if row.map_err(|error| format!("read {table} schema: {error}"))? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn connections(name: &str) -> (PathBuf, Connection, Connection) {
        let path = std::env::temp_dir().join(format!(
            "prism-execution-{name}-{}-{}.db",
            std::process::id(),
            new_instance_id("test")
        ));
        let first = Connection::open(&path).unwrap();
        first
            .execute_batch(
                "create table plan_run (
                   id text primary key, status text, pause_requested integer default 0,
                   created_unix_ms integer, updated_unix_ms integer
                 );
                 create table auto_run (
                   id text primary key, status text, pause_requested integer default 0,
                   created_unix_ms integer, updated_unix_ms integer
                 );
                 create table plan_step_run (
                   run_id text, step integer, status text, started_unix_ms integer, finished_unix_ms integer,
                   execution_state text, execution_process_id integer,
                   execution_process_start_time_ticks integer, process_id integer, error text
                  );
                  create table plan_output_line (
                    run_id text, step integer, line_number integer, time_unix_ms integer,
                    kind text, text text
                  );
                  create table auto_step_run (
                   id integer primary key, run_id text, plan_run_id text, status text,
                   started_unix_ms integer, finished_unix_ms integer,
                   execution_state text, execution_process_id integer,
                   execution_process_start_time_ticks integer, process_id integer, error text
                  );
                  create table auto_output_line (
                    step_run_id integer, line_number integer, time_unix_ms integer,
                    kind text, text text
                  );
                  create table auto_event (
                    id integer primary key, run_id text, step_run_id integer,
                    time_unix_ms integer, kind text, data_json text
                  );",
            )
            .unwrap();
        migrate_schema(&first).unwrap();
        let second = Connection::open(&path).unwrap();
        (path, first, second)
    }

    #[test]
    fn only_one_connection_can_claim_a_run() {
        let (path, mut first, mut second) = connections("claim");
        let workflow = WorkflowIdentity::new(WorkflowKind::Plan, "plan-1");
        enqueue(&first, &workflow).unwrap();

        let first_claim = claim(&mut first, &workflow, "daemon-a", "worker-a").unwrap();
        let second_claim = claim(&mut second, &workflow, "daemon-b", "worker-b").unwrap();

        assert!(first_claim.is_some());
        assert!(second_claim.is_none());
        drop(first);
        drop(second);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn replaced_claim_is_fenced() {
        let (path, mut first, mut second) = connections("fence");
        let workflow = WorkflowIdentity::new(WorkflowKind::Auto, "auto-1");
        enqueue(&first, &workflow).unwrap();
        let old = claim_with_lease(&mut first, &workflow, "old", "worker", -1)
            .unwrap()
            .unwrap();
        mark_abandoned(&second, "new").unwrap();
        apply_recovery_decision(&mut second, &[(workflow.clone(), 1, true)]).unwrap();
        let new = claim(&mut second, &workflow, "new", "worker-new")
            .unwrap()
            .unwrap();

        assert!(new.fencing_token > old.fencing_token);
        assert_eq!(
            validate_claim(&first, &old).unwrap_err(),
            "execution claim is stale"
        );
        assert!(validate_claim(&second, &new).is_ok());
        drop(first);
        drop(second);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn abandoned_work_requires_an_explicit_recovery_decision() {
        let (path, mut first, mut second) = connections("recovery");
        let workflow = WorkflowIdentity::new(WorkflowKind::Plan, "plan-1");
        enqueue(&first, &workflow).unwrap();
        claim_with_lease(&mut first, &workflow, "old", "worker", -1)
            .unwrap()
            .unwrap();

        assert_eq!(mark_abandoned(&second, "new").unwrap(), 1);
        assert_eq!(
            dispatch_state(&second, &workflow).unwrap(),
            Some(DispatchState::RecoveryPending)
        );
        assert!(
            claim(&mut second, &workflow, "new", "worker-new")
                .unwrap()
                .is_none()
        );
        apply_recovery_decision(&mut second, &[(workflow.clone(), 1, true)]).unwrap();
        assert!(
            claim(&mut second, &workflow, "new", "worker-new")
                .unwrap()
                .is_some()
        );
        drop(first);
        drop(second);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn expired_claim_cannot_renew_itself() {
        let (path, mut first, second) = connections("expired-heartbeat");
        let workflow = WorkflowIdentity::new(WorkflowKind::Auto, "auto-1");
        enqueue(&first, &workflow).unwrap();
        let claim = claim_with_lease(&mut first, &workflow, "daemon", "worker", -1)
            .unwrap()
            .unwrap();

        assert_eq!(
            heartbeat(&second, &claim).unwrap_err(),
            "execution claim is stale"
        );
        assert_eq!(
            dispatch_state(&second, &workflow).unwrap(),
            Some(DispatchState::RecoveryPending)
        );
        drop(first);
        drop(second);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn requeue_requested_while_claimed_wins_over_release() {
        let (path, mut owner, requester) = connections("requeue-release");
        let workflow = WorkflowIdentity::new(WorkflowKind::Plan, "plan-1");
        enqueue(&owner, &workflow).unwrap();
        let claim = claim(&mut owner, &workflow, "daemon", "worker")
            .unwrap()
            .unwrap();

        enqueue(&requester, &workflow).unwrap();
        release(&owner, &claim, DispatchState::Paused).unwrap();

        assert_eq!(
            dispatch_state(&requester, &workflow).unwrap(),
            Some(DispatchState::Queued)
        );
        drop(owner);
        drop(requester);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn expired_claim_from_current_daemon_becomes_recovery_pending() {
        let (path, mut owner, observer) = connections("current-daemon-expired");
        let workflow = WorkflowIdentity::new(WorkflowKind::Auto, "auto-1");
        enqueue(&owner, &workflow).unwrap();
        claim_with_lease(&mut owner, &workflow, "daemon", "worker", -1)
            .unwrap()
            .unwrap();

        assert_eq!(mark_abandoned(&observer, "daemon").unwrap(), 1);
        assert_eq!(
            dispatch_state(&observer, &workflow).unwrap(),
            Some(DispatchState::RecoveryPending)
        );
        drop(owner);
        drop(observer);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn unselected_recovery_preserves_process_identity_for_later_resume() {
        let (path, mut conn, other) = connections("unselected-process");
        conn.execute(
            "insert into plan_run (id, status, created_unix_ms, updated_unix_ms)
             values ('plan-1', 'running', 1, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "insert into plan_step_run (
               run_id, step, status, execution_process_id,
               execution_process_start_time_ticks
             ) values ('plan-1', 1, 'running', 999999, null)",
            [],
        )
        .unwrap();
        migrate_schema(&conn).unwrap();
        let workflow = WorkflowIdentity::new(WorkflowKind::Plan, "plan-1");

        apply_recovery_decision(&mut conn, &[(workflow.clone(), 1, false)]).unwrap();

        assert_eq!(
            dispatch_state(&conn, &workflow).unwrap(),
            Some(DispatchState::Paused)
        );
        let process_id: Option<i64> = conn
            .query_row(
                "select execution_process_id from plan_step_run
                 where run_id = 'plan-1' and step = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(process_id, Some(999999));
        drop(conn);
        drop(other);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn migration_never_queues_legacy_active_runs() {
        let (path, first, second) = connections("migration");
        first
            .execute(
                "insert into plan_run (id, status, created_unix_ms, updated_unix_ms)
                 values ('plan-old', 'running', 1, 2)",
                [],
            )
            .unwrap();
        first
            .execute(
                "insert into auto_run (id, status, created_unix_ms, updated_unix_ms)
                 values ('auto-old', 'queued', 1, 2)",
                [],
            )
            .unwrap();
        migrate_schema(&first).unwrap();
        for workflow in [
            WorkflowIdentity::new(WorkflowKind::Plan, "plan-old"),
            WorkflowIdentity::new(WorkflowKind::Auto, "auto-old"),
        ] {
            assert_eq!(
                dispatch_state(&first, &workflow).unwrap(),
                Some(DispatchState::RecoveryPending)
            );
        }
        drop(first);
        drop(second);
        let _ = fs::remove_file(path);
    }

    fn assert_stale(result: rusqlite::Result<usize>) {
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("execution claim is stale")
        );
    }

    #[test]
    fn plan_claim_guards_allow_current_mutations_and_fence_every_operation() {
        let (path, mut managed, other) = connections("plan-claim-guards");
        let workflow = WorkflowIdentity::new(WorkflowKind::Plan, "plan-guarded");
        enqueue(&managed, &workflow).unwrap();
        let claim = claim(&mut managed, &workflow, "daemon", "worker")
            .unwrap()
            .unwrap();
        install_claim_guards(&managed, &claim).unwrap();

        managed
            .execute(
                "insert into plan_run (id, status, created_unix_ms, updated_unix_ms)
                 values (?1, 'running', 1, 1)",
                [&workflow.run_id],
            )
            .unwrap();
        managed
            .execute(
                "insert into plan_step_run (run_id, step, status) values (?1, 1, 'running')",
                [&workflow.run_id],
            )
            .unwrap();
        managed
            .execute(
                "insert into plan_output_line
                 (run_id, step, line_number, time_unix_ms, kind, text)
                 values (?1, 1, 1, 1, 'stdout', 'one')",
                [&workflow.run_id],
            )
            .unwrap();
        managed
            .execute(
                "update plan_run set updated_unix_ms = 2 where id = ?1",
                [&workflow.run_id],
            )
            .unwrap();
        managed
            .execute(
                "delete from plan_output_line where run_id = ?1",
                [&workflow.run_id],
            )
            .unwrap();

        other
            .execute(
                "update workflow_execution set fencing_token = fencing_token + 1
                 where workflow_kind = 'plan' and run_id = ?1",
                [&workflow.run_id],
            )
            .unwrap();

        assert_stale(managed.execute(
            "insert into plan_output_line
             (run_id, step, line_number, time_unix_ms, kind, text)
             values (?1, 1, 2, 2, 'stdout', 'two')",
            [&workflow.run_id],
        ));
        assert_stale(managed.execute(
            "update plan_step_run set status = 'done' where run_id = ?1",
            [&workflow.run_id],
        ));
        assert_stale(managed.execute("delete from plan_run where id = ?1", [&workflow.run_id]));

        drop(managed);
        drop(other);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn auto_claim_guards_cover_auto_rows_and_linked_plan_rows() {
        let (path, mut managed, other) = connections("auto-claim-guards");
        let workflow = WorkflowIdentity::new(WorkflowKind::Auto, "auto-guarded");
        enqueue(&managed, &workflow).unwrap();
        let claim = claim(&mut managed, &workflow, "daemon", "worker")
            .unwrap()
            .unwrap();
        managed
            .execute(
                "insert into auto_run (id, status, created_unix_ms, updated_unix_ms)
                 values (?1, 'running', 1, 1)",
                [&workflow.run_id],
            )
            .unwrap();
        managed
            .execute(
                "insert into auto_step_run (id, run_id, plan_run_id, status)
                 values (10, ?1, 'linked-plan', 'running')",
                [&workflow.run_id],
            )
            .unwrap();
        install_claim_guards(&managed, &claim).unwrap();
        managed
            .execute(
                "insert into auto_output_line
                 (step_run_id, line_number, time_unix_ms, kind, text)
                 values (10, 1, 1, 'stdout', 'one')",
                [],
            )
            .unwrap();
        managed
            .execute(
                "insert into auto_event (id, run_id, step_run_id, time_unix_ms, kind, data_json)
                 values (20, ?1, 10, 1, 'started', '{}')",
                [&workflow.run_id],
            )
            .unwrap();
        managed
            .execute(
                "insert into plan_run (id, status, created_unix_ms, updated_unix_ms)
                 values ('linked-plan', 'running', 1, 1)",
                [],
            )
            .unwrap();
        managed
            .execute(
                "insert into plan_step_run (run_id, step, status)
                 values ('linked-plan', 1, 'running')",
                [],
            )
            .unwrap();
        managed
            .execute(
                "insert into plan_output_line
                 (run_id, step, line_number, time_unix_ms, kind, text)
                 values ('linked-plan', 1, 1, 1, 'stdout', 'one')",
                [],
            )
            .unwrap();

        for statement in [
            "update auto_run set updated_unix_ms = 2 where id = 'auto-guarded'",
            "update auto_step_run set status = 'done' where id = 10",
            "update auto_output_line set text = 'two' where step_run_id = 10",
            "update auto_event set kind = 'finished' where id = 20",
            "update plan_run set updated_unix_ms = 2 where id = 'linked-plan'",
            "update plan_step_run set status = 'done' where run_id = 'linked-plan'",
            "update plan_output_line set text = 'two' where run_id = 'linked-plan'",
        ] {
            managed.execute(statement, []).unwrap();
        }

        other
            .execute(
                "update workflow_execution set worker_id = 'replacement',
                   daemon_instance_id = 'replacement', fencing_token = fencing_token + 1
                 where workflow_kind = 'auto' and run_id = ?1",
                [&workflow.run_id],
            )
            .unwrap();
        for statement in [
            "update auto_run set updated_unix_ms = 3 where id = 'auto-guarded'",
            "update auto_step_run set status = 'failed' where id = 10",
            "update auto_output_line set text = 'stale' where step_run_id = 10",
            "update auto_event set kind = 'stale' where id = 20",
            "update plan_run set updated_unix_ms = 3 where id = 'linked-plan'",
            "update plan_step_run set status = 'failed' where run_id = 'linked-plan'",
            "update plan_output_line set text = 'stale' where run_id = 'linked-plan'",
        ] {
            assert_stale(managed.execute(statement, []));
        }

        drop(managed);
        drop(other);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn claim_guards_reject_unrelated_workflow_rows() {
        let (path, mut managed, other) = connections("claim-guard-scope");
        let workflow = WorkflowIdentity::new(WorkflowKind::Plan, "owned");
        enqueue(&managed, &workflow).unwrap();
        let claim = claim(&mut managed, &workflow, "daemon", "worker")
            .unwrap()
            .unwrap();
        managed
            .execute(
                "insert into plan_run (id, status, created_unix_ms, updated_unix_ms)
                 values ('unrelated', 'running', 1, 1)",
                [],
            )
            .unwrap();
        install_claim_guards(&managed, &claim).unwrap();

        assert_stale(managed.execute(
            "update plan_run set updated_unix_ms = 2 where id = 'unrelated'",
            [],
        ));

        drop(managed);
        drop(other);
        let _ = fs::remove_file(path);
    }
}
