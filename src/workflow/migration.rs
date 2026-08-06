//! Idempotent import of legacy repository-local Plan and Auto history.
//!
//! Imported rows are deliberately historical: authority and provenance are
//! empty/unknown, and non-terminal work becomes recovery-required. Nothing in
//! this module makes a legacy run executable.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::Serialize;

use crate::definition::{
    CompiledStep, DefinitionSnapshot, EffectClass, ImplementationDescriptor, PrimitiveClass,
    SnapshotContent, StepSettings, TargetRequirement, WorkflowBudgets,
};
use crate::run::{RunId, RunLedger, StartRun, now_ms, sha256};

const MAX_IMPORTED_OUTPUT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct MigrationReport {
    pub schema_version: u32,
    pub sources: Vec<MigrationSourceReport>,
    pub imported_runs: usize,
    pub already_imported_runs: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct MigrationSourceReport {
    pub source_path: PathBuf,
    pub source_schema_version: u32,
    pub source_digest: String,
    pub expected_runs: usize,
    pub imported_runs: usize,
    pub already_complete: bool,
}

#[derive(Clone, Debug, Serialize)]
struct LegacyRun {
    kind: String,
    id: String,
    repository: String,
    status: String,
    created_unix_ms: i64,
    updated_unix_ms: i64,
    steps: Vec<LegacyStep>,
}

#[derive(Clone, Debug, Serialize)]
struct LegacyStep {
    key: String,
    state: String,
    ordinal: u32,
    started_unix_ms: Option<i64>,
    finished_unix_ms: Option<i64>,
    summary: Option<String>,
    error: Option<String>,
    commit: Option<String>,
    head: Option<String>,
    linked_plan: Option<String>,
    output: Vec<LegacyOutput>,
}

#[derive(Clone, Debug, Serialize)]
struct LegacyOutput {
    sequence: u32,
    stream: String,
    bytes: Vec<u8>,
    truncated: bool,
    created_unix_ms: i64,
}

pub(crate) fn import_repositories(
    ledger: &RunLedger,
    repository_databases: impl IntoIterator<Item = PathBuf>,
) -> Result<MigrationReport, String> {
    let mut report = MigrationReport {
        schema_version: 1,
        sources: Vec::new(),
        imported_runs: 0,
        already_imported_runs: 0,
    };
    for path in repository_databases {
        if !path.exists() {
            continue;
        }
        let source = open_legacy_read_only(&path)?;
        refuse_potentially_live_work(&source, &path)?;
        let source_version: u32 = source
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(|error| format!("read legacy schema version {}: {error}", path.display()))?;
        let runs = load_runs(&source)?;
        let source_digest = digest(&runs)?;
        let source_path = path
            .canonicalize()
            .unwrap_or_else(|_| path.clone())
            .display()
            .to_string();
        let existing = journal(&ledger.connection()?, &source_path, source_version)?;
        if let Some((digest, expected, imported, state)) = existing {
            if digest != source_digest || expected != runs.len() as i64 {
                return Err(format!(
                    "legacy source {} changed after migration began; restore the original database or remove its incomplete migration journal",
                    path.display()
                ));
            }
            if state == "complete" {
                report.already_imported_runs += imported.max(0) as usize;
                report.sources.push(MigrationSourceReport {
                    source_path: path,
                    source_schema_version: source_version,
                    source_digest,
                    expected_runs: expected.max(0) as usize,
                    imported_runs: imported.max(0) as usize,
                    already_complete: true,
                });
                continue;
            }
        } else {
            ledger.connection()?.execute(
                "insert into legacy_migration_journal(source_path,source_schema_version,source_digest,state,expected_runs,imported_runs,started_unix_ms) values(?1,?2,?3,'importing',?4,0,?5)",
                params![source_path, source_version, source_digest, runs.len() as i64, now_ms()],
            ).map_err(sql_error)?;
        }

        let mut imported_here = 0usize;
        for run in &runs {
            if import_one(ledger, &source_path, source_version, run)? {
                imported_here += 1;
                ledger.connection()?.execute(
                    "update legacy_migration_journal set imported_runs=imported_runs+1 where source_path=?1 and source_schema_version=?2",
                    params![source_path, source_version],
                ).map_err(sql_error)?;
            }
        }
        link_auto_plan_history(ledger, &source_path, source_version, &runs)?;
        let conn = ledger.connection()?;
        let imported: i64 = conn.query_row(
            "select count(*) from legacy_run_import where source_path=?1 and source_schema_version=?2",
            params![source_path, source_version],
            |row| row.get(0),
        ).map_err(sql_error)?;
        if imported != runs.len() as i64 {
            return Err(format!(
                "legacy import count mismatch for {}: expected {}, imported {imported}",
                path.display(),
                runs.len()
            ));
        }
        conn.execute(
            "update legacy_migration_journal set state='complete',imported_runs=?3,error=null,completed_unix_ms=?4 where source_path=?1 and source_schema_version=?2",
            params![source_path, source_version, imported, now_ms()],
        ).map_err(sql_error)?;
        report.imported_runs += imported_here;
        report.already_imported_runs += imported.max(0) as usize - imported_here;
        report.sources.push(MigrationSourceReport {
            source_path: path,
            source_schema_version: source_version,
            source_digest,
            expected_runs: runs.len(),
            imported_runs: imported.max(0) as usize,
            already_complete: false,
        });
    }
    Ok(report)
}

fn open_legacy_read_only(path: &Path) -> Result<Connection, String> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| format!("open legacy database {} read-only: {error}", path.display()))
}

fn refuse_potentially_live_work(conn: &Connection, path: &Path) -> Result<(), String> {
    if !table_exists(conn, "workflow_execution")? {
        return Ok(());
    }
    let active: i64 = conn.query_row(
        "select count(*) from workflow_execution where dispatch_state='claimed' or executor_pid is not null",
        [],
        |row| row.get(0),
    ).map_err(sql_error)?;
    if active > 0 {
        return Err(format!(
            "cannot migrate {} while {active} legacy execution(s) may still be in flight; stop the worker and recover or cancel them first",
            path.display()
        ));
    }
    Ok(())
}

fn load_runs(conn: &Connection) -> Result<Vec<LegacyRun>, String> {
    let mut runs = Vec::new();
    if table_exists(conn, "plan_run")? {
        let mut statement = conn.prepare(
            "select id,coalesce(repo_root,''),status,created_unix_ms,updated_unix_ms from plan_run order by id",
        ).map_err(sql_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?;
        for (id, repository, status, created, updated) in rows {
            runs.push(LegacyRun {
                steps: load_plan_steps(conn, &id)?,
                kind: "plan".into(),
                id,
                repository,
                status,
                created_unix_ms: created,
                updated_unix_ms: updated,
            });
        }
    }
    if table_exists(conn, "auto_run")? {
        let mut statement = conn.prepare(
            "select id,coalesce(repo_root,''),status,created_unix_ms,updated_unix_ms from auto_run order by id",
        ).map_err(sql_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?;
        for (id, repository, status, created, updated) in rows {
            runs.push(LegacyRun {
                steps: load_auto_steps(conn, &id)?,
                kind: "auto".into(),
                id,
                repository,
                status,
                created_unix_ms: created,
                updated_unix_ms: updated,
            });
        }
    }
    runs.sort_by(|left, right| (&left.kind, &left.id).cmp(&(&right.kind, &right.id)));
    Ok(runs)
}

fn load_plan_steps(conn: &Connection, run_id: &str) -> Result<Vec<LegacyStep>, String> {
    if !table_exists(conn, "plan_step_run")? {
        return Ok(Vec::new());
    }
    let mut statement = conn.prepare(
        "select step,status,started_unix_ms,finished_unix_ms,summary,error from plan_step_run where run_id=?1 order by step",
    ).map_err(sql_error)?;
    let rows = statement
        .query_map([run_id], |row| {
            Ok((
                row.get::<_, u32>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)?;
    rows.into_iter()
        .map(|(step, state, started, finished, summary, error)| {
            Ok(LegacyStep {
                key: format!("phase-{step}"),
                state,
                ordinal: step.max(1),
                started_unix_ms: started,
                finished_unix_ms: finished,
                summary,
                error,
                commit: None,
                head: None,
                linked_plan: None,
                output: load_plan_output(conn, run_id, step)?,
            })
        })
        .collect()
}

fn load_plan_output(
    conn: &Connection,
    run_id: &str,
    step: u32,
) -> Result<Vec<LegacyOutput>, String> {
    if !table_exists(conn, "plan_output_line")? {
        return Ok(Vec::new());
    }
    let mut statement = conn.prepare(
        "select line_number,kind,text,time_unix_ms from plan_output_line where run_id=?1 and step=?2 order by line_number",
    ).map_err(sql_error)?;
    bounded_output(
        statement
            .query_map(params![run_id, step], |row| {
                Ok((
                    row.get::<_, u32>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?,
    )
}

fn load_auto_steps(conn: &Connection, run_id: &str) -> Result<Vec<LegacyStep>, String> {
    if !table_exists(conn, "auto_step_run")? {
        return Ok(Vec::new());
    }
    let mut statement = conn.prepare(
        "select id,sequence,step_key,status,attempt,started_unix_ms,finished_unix_ms,summary,error,commit_sha,head_sha,plan_run_id from auto_step_run where run_id=?1 order by sequence,id",
    ).map_err(sql_error)?;
    let rows = statement
        .query_map([run_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, u32>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, u32>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, Option<String>>(10)?,
                row.get::<_, Option<String>>(11)?,
            ))
        })
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)?;
    rows.into_iter()
        .map(
            |(
                id,
                sequence,
                key,
                state,
                attempt,
                started,
                finished,
                summary,
                error,
                commit,
                head,
                linked_plan,
            )| {
                Ok(LegacyStep {
                    key: format!("{}-{}", safe_id(&key), sequence),
                    state,
                    ordinal: attempt.max(1),
                    started_unix_ms: started,
                    finished_unix_ms: finished,
                    summary,
                    error,
                    commit,
                    head,
                    linked_plan,
                    output: load_auto_output(conn, id)?,
                })
            },
        )
        .collect()
}

fn load_auto_output(conn: &Connection, step_id: i64) -> Result<Vec<LegacyOutput>, String> {
    if !table_exists(conn, "auto_output_line")? {
        return Ok(Vec::new());
    }
    let mut statement = conn.prepare(
        "select line_number,kind,text,time_unix_ms from auto_output_line where step_run_id=?1 order by line_number",
    ).map_err(sql_error)?;
    bounded_output(
        statement
            .query_map([step_id], |row| {
                Ok((
                    row.get::<_, u32>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?,
    )
}

fn bounded_output(rows: Vec<(u32, String, String, i64)>) -> Result<Vec<LegacyOutput>, String> {
    let mut remaining = MAX_IMPORTED_OUTPUT_BYTES;
    let mut output = Vec::new();
    for (sequence, kind, text, created) in rows {
        if remaining == 0 {
            break;
        }
        let bytes = text.into_bytes();
        let take = bytes.len().min(remaining);
        output.push(LegacyOutput {
            sequence,
            stream: kind,
            bytes: bytes[..take].to_vec(),
            truncated: take < bytes.len(),
            created_unix_ms: created,
        });
        remaining -= take;
    }
    Ok(output)
}

fn import_one(
    ledger: &RunLedger,
    source_path: &str,
    source_version: u32,
    run: &LegacyRun,
) -> Result<bool, String> {
    let record_digest = digest(run)?;
    let conn = ledger.connection()?;
    let existing: Option<(String, String)> = conn.query_row(
        "select workflow_run_id,record_digest from legacy_run_import where source_path=?1 and source_schema_version=?2 and legacy_kind=?3 and legacy_run_id=?4",
        params![source_path, source_version, run.kind, run.id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional().map_err(sql_error)?;
    if let Some((_, existing_digest)) = existing {
        if existing_digest != record_digest {
            return Err(format!(
                "legacy {} run '{}' changed after import",
                run.kind, run.id
            ));
        }
        return Ok(false);
    }
    drop(conn);

    let snapshot = legacy_snapshot(run)?;
    let repository_id = if run.repository.is_empty() || !Path::new(&run.repository).exists() {
        None
    } else {
        Some(ledger.repository_id(Path::new(&run.repository))?)
    };
    let result = ledger.start_quarantined(StartRun {
        snapshot,
        repository_id,
        inputs: Vec::new(),
        idempotency_key: Some(format!(
            "legacy:{source_path}:{}:{}:{}",
            source_version, run.kind, run.id
        )),
        actor: "legacy-import:unknown".into(),
        actor_capabilities: BTreeSet::new(),
    })?;
    persist_history(ledger, &result.run_id, source_path, source_version, run)?;
    ledger.connection()?.execute(
        "insert into legacy_run_import(source_path,source_schema_version,legacy_kind,legacy_run_id,workflow_run_id,record_digest,imported_unix_ms) values(?1,?2,?3,?4,?5,?6,?7)",
        params![source_path, source_version, run.kind, run.id, result.run_id.as_str(), record_digest, now_ms()],
    ).map_err(sql_error)?;
    Ok(true)
}

fn legacy_snapshot(run: &LegacyRun) -> Result<DefinitionSnapshot, String> {
    let qualified_name = format!("legacy:{}", run.kind);
    let steps = run
        .steps
        .iter()
        .map(|step| CompiledStep {
            id: step.key.clone(),
            class: PrimitiveClass::Action,
            implementation: format!("legacy:{}-step@1", run.kind),
            implementation_revision: 1,
            dependencies: Vec::new(),
            condition: None,
            capabilities: BTreeSet::new(),
            inputs: BTreeMap::new(),
            outputs: BTreeMap::new(),
            settings: StepSettings::default(),
            child_workflow: None,
        })
        .collect::<Vec<_>>();
    let implementation = ImplementationDescriptor {
        id: format!("legacy:{}-step@1", run.kind),
        revision: 1,
        class: PrimitiveClass::Action,
        capabilities: BTreeSet::new(),
        inputs: BTreeMap::new(),
        outputs: BTreeMap::new(),
        effect: EffectClass::ReadOnly,
        target: TargetRequirement::Any,
    };
    let source_digest = digest(run)?;
    let content = SnapshotContent {
        schema_version: 1,
        qualified_name,
        source_revision: "1".into(),
        source_digest: source_digest.clone(),
        description: "Imported legacy history; authority and exact provenance are unknown.".into(),
        capabilities: BTreeSet::new(),
        inputs: BTreeMap::new(),
        outputs: BTreeMap::new(),
        budgets: WorkflowBudgets {
            max_attempts: run.steps.len().max(1) as u32,
            max_fan_out: 0,
            max_child_depth: 0,
            max_mutations: 0,
        },
        steps,
        implementations: if run.steps.is_empty() {
            Vec::new()
        } else {
            vec![implementation]
        },
        admission_policy: None,
        triggers: Vec::new(),
        pinned_workflows: Vec::new(),
        pinned_snapshots: BTreeMap::new(),
        transitive_capabilities: BTreeSet::new(),
    };
    let canonical_bytes = serde_json::to_vec(&content).map_err(|error| error.to_string())?;
    Ok(DefinitionSnapshot {
        digest: sha256(&canonical_bytes),
        source_trust_digest: source_digest,
        content,
        canonical_bytes,
    })
}

fn persist_history(
    ledger: &RunLedger,
    run_id: &RunId,
    source_path: &str,
    source_version: u32,
    legacy: &LegacyRun,
) -> Result<(), String> {
    let mut conn = ledger.connection()?;
    let tx = conn.transaction().map_err(sql_error)?;
    let terminal = matches!(legacy.status.as_str(), "done" | "failed" | "aborted");
    let (run_state, control) = match legacy.status.as_str() {
        "done" => ("completed", "running"),
        "failed" => ("failed", "running"),
        "aborted" => ("cancelled", "cancel_requested"),
        _ => ("recovery_required", "pause_requested"),
    };
    tx.execute(
        "update workflow_run set state=?2,control=?3,actor='legacy-import:unknown',created_unix_ms=?4,updated_unix_ms=?5,revision=revision+1 where id=?1",
        params![run_id.as_str(), run_state, control, legacy.created_unix_ms, legacy.updated_unix_ms],
    ).map_err(sql_error)?;
    tx.execute(
        "update authority_grant set basis='legacy_unknown',capabilities_json='[]',secret_scope_json='[]',target_scope_json='[]' where run_id=?1",
        [run_id.as_str()],
    ).map_err(sql_error)?;
    for step in &legacy.steps {
        let step_id: String = tx
            .query_row(
                "select id from workflow_step where run_id=?1 and definition_step_id=?2",
                params![run_id.as_str(), step.key],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        let (step_state, attempt_state) = match step.state.as_str() {
            "done" => ("completed", "completed"),
            "skipped" => ("skipped", "completed"),
            "failed" => ("failed", "failed"),
            "aborted" => ("cancelled", "cancelled"),
            _ => ("recovery_required", "recovery_required"),
        };
        let attempt_id = format!(
            "legacy-{}",
            sha256(
                format!(
                    "{source_path}:{source_version}:{}:{}:{}",
                    legacy.kind, legacy.id, step.key
                )
                .as_bytes()
            )
        );
        let created = step.started_unix_ms.unwrap_or(legacy.created_unix_ms);
        let updated = step.finished_unix_ms.unwrap_or(legacy.updated_unix_ms);
        tx.execute(
            "insert into step_attempt(id,run_id,step_id,ordinal,state,input_digest,implementation_id,implementation_revision,terminal_reason,created_unix_ms,updated_unix_ms) values(?1,?2,?3,?4,?5,'unknown',?6,1,?7,?8,?9)",
            params![attempt_id, run_id.as_str(), step_id, step.ordinal, attempt_state,
                format!("legacy:{}-step@1", legacy.kind), step.error.as_deref().or(if terminal { None } else { Some("legacy work cannot be resumed; cancel or restart as new") }), created, updated],
        ).map_err(sql_error)?;
        tx.execute(
            "update workflow_step set state=?2,outcome=?3,attempt_count=1,blocker=?4,created_unix_ms=?5,updated_unix_ms=?6 where id=?1",
            params![step_id, step_state, step.summary.as_deref().unwrap_or("legacy_unknown"),
                if step_state == "recovery_required" { Some("legacy provenance or completion is unknown") } else { None }, created, updated],
        ).map_err(sql_error)?;
        for line in &step.output {
            tx.execute(
                "insert into attempt_output(attempt_id,sequence,stream,bytes,truncated,created_unix_ms) values(?1,?2,?3,?4,?5,?6)",
                params![attempt_id, line.sequence, line.stream, line.bytes, line.truncated, line.created_unix_ms],
            ).map_err(sql_error)?;
        }
        if step.commit.is_some() || step.head.is_some() {
            let payload = serde_json::to_vec(&serde_json::json!({
                "commit": step.commit, "head": step.head, "provenance": "unknown"
            }))
            .map_err(|error| error.to_string())?;
            tx.execute(
                "insert into artifact(id,revision,run_id,producer_attempt_id,port,artifact_type,schema_revision,digest,trust,sensitivity,payload_inline,size,created_unix_ms) values(?1,1,?2,?3,'commit','builtin:commit@1',1,?4,'derived_untrusted','internal',?5,?6,?7)",
                params![format!("legacy-artifact-{}", &attempt_id[7..]), run_id.as_str(), attempt_id, sha256(&payload), payload, payload.len() as i64, updated],
            ).map_err(sql_error)?;
        }
    }
    tx.execute(
        "insert into run_event(run_id,kind,data_json,created_unix_ms) values(?1,'legacy_imported',?2,?3)",
        params![run_id.as_str(), serde_json::json!({"kind":legacy.kind,"legacy_run_id":legacy.id,"authority":"unknown","provenance":"unknown"}).to_string(), now_ms()],
    ).map_err(sql_error)?;
    tx.commit().map_err(sql_error)
}

fn link_auto_plan_history(
    ledger: &RunLedger,
    source_path: &str,
    source_version: u32,
    runs: &[LegacyRun],
) -> Result<(), String> {
    let conn = ledger.connection()?;
    for auto in runs.iter().filter(|run| run.kind == "auto") {
        let auto_id: Option<String> = conn.query_row(
            "select workflow_run_id from legacy_run_import where source_path=?1 and source_schema_version=?2 and legacy_kind='auto' and legacy_run_id=?3",
            params![source_path, source_version, auto.id], |row| row.get(0),
        ).optional().map_err(sql_error)?;
        let Some(auto_id) = auto_id else {
            continue;
        };
        for plan in auto
            .steps
            .iter()
            .filter_map(|step| step.linked_plan.as_deref())
        {
            let plan_id: Option<String> = conn.query_row(
                "select workflow_run_id from legacy_run_import where source_path=?1 and source_schema_version=?2 and legacy_kind='plan' and legacy_run_id=?3",
                params![source_path, source_version, plan], |row| row.get(0),
            ).optional().map_err(sql_error)?;
            let Some(plan_id) = plan_id else {
                continue;
            };
            let parent_step: Option<String> = conn
                .query_row(
                    "select id from workflow_step where run_id=?1 order by rowid limit 1",
                    [auto_id.as_str()],
                    |row| row.get(0),
                )
                .optional()
                .map_err(sql_error)?;
            let Some(parent_step) = parent_step else {
                continue;
            };
            let child_digest: String = conn
                .query_row(
                    "select snapshot_digest from workflow_run where id=?1",
                    [plan_id.as_str()],
                    |row| row.get(0),
                )
                .map_err(sql_error)?;
            conn.execute(
                "insert or ignore into workflow_run_link(parent_run_id,parent_step_id,child_run_id,call_key,child_snapshot_digest,input_digest,purpose,propagation) values(?1,?2,?3,?4,?5,'unknown','legacy_link','parent_only')",
                params![auto_id, parent_step, plan_id, format!("legacy-plan:{plan}"), child_digest],
            ).map_err(sql_error)?;
        }
    }
    Ok(())
}

fn journal(
    conn: &Connection,
    source: &str,
    version: u32,
) -> Result<Option<(String, i64, i64, String)>, String> {
    conn.query_row(
        "select source_digest,expected_runs,imported_runs,state from legacy_migration_journal where source_path=?1 and source_schema_version=?2",
        params![source, version], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    ).optional().map_err(sql_error)
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool, String> {
    conn.query_row(
        "select exists(select 1 from sqlite_master where type='table' and name=?1)",
        [table],
        |row| row.get(0),
    )
    .map_err(sql_error)
}

fn digest(value: &impl Serialize) -> Result<String, String> {
    serde_json::to_vec(value)
        .map(|bytes| sha256(&bytes))
        .map_err(|error| error.to_string())
}

fn safe_id(value: &str) -> String {
    let value = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    if value.is_empty() {
        "step".into()
    } else {
        value
    }
}

fn sql_error(error: rusqlite::Error) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_and_active_legacy_history_imports_idempotently() {
        let root = std::env::temp_dir().join(format!(
            "prism-legacy-import-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source_path = root.join("repository.db");
        let source = Connection::open(&source_path).unwrap();
        source.execute_batch("create table plan_run(id text primary key,repo_root text,status text,created_unix_ms integer,updated_unix_ms integer);create table plan_step_run(run_id text,step integer,status text,started_unix_ms integer,finished_unix_ms integer,summary text,error text);create table plan_output_line(run_id text,step integer,line_number integer,time_unix_ms integer,kind text,text text);insert into plan_run values('done-plan','/repo','done',1,3);insert into plan_step_run values('done-plan',1,'done',1,3,'ok',null);insert into plan_output_line values('done-plan',1,1,2,'assistant','finished');create table auto_run(id text primary key,repo_root text,status text,created_unix_ms integer,updated_unix_ms integer);create table auto_step_run(id integer primary key,run_id text,sequence integer,step_key text,status text,attempt integer,started_unix_ms integer,finished_unix_ms integer,summary text,error text,commit_sha text,head_sha text,plan_run_id text);create table auto_output_line(step_run_id integer,line_number integer,time_unix_ms integer,kind text,text text);insert into auto_run values('active-auto','/repo','running',4,5);insert into auto_step_run values(1,'active-auto',1,'implement','running',1,4,null,null,null,null,'abc','done-plan');pragma user_version=9;").unwrap();
        drop(source);
        let ledger = RunLedger::open(root.join("workflow.db")).unwrap();
        let first = import_repositories(&ledger, [source_path.clone()]).unwrap();
        assert_eq!(first.imported_runs, 2);
        let second = import_repositories(&ledger, [source_path]).unwrap();
        assert_eq!(second.imported_runs, 0);
        assert_eq!(second.already_imported_runs, 2);
        let runs = ledger.list(10).unwrap();
        assert!(
            runs.iter()
                .any(|run| run.definition == "legacy:plan" && run.state.label() == "completed")
        );
        assert!(
            runs.iter()
                .any(|run| run.definition == "legacy:auto"
                    && run.state.label() == "recovery_required")
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
