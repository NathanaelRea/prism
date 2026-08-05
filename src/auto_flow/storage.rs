use super::*;

pub struct AutoFlowStore {
    plan_store: crate::plan_run::PlanRunStore,
}

impl AutoFlowStore {
    pub fn open(path: impl Into<PathBuf>) -> Self {
        Self {
            plan_store: crate::plan_run::PlanRunStore::open(path),
        }
    }

    pub fn path(&self) -> &Path {
        self.plan_store.path()
    }

    pub fn validate_claim(&self) -> Result<(), String> {
        Ok(())
    }
}

impl std::ops::Deref for AutoFlowStore {
    type Target = crate::plan_run::PlanRunStore;

    fn deref(&self) -> &Self::Target {
        &self.plan_store
    }
}

pub fn save_auto_run(conn: &AutoFlowStore, persisted: &mut PersistedAutoRun) -> Result<(), String> {
    let transaction = crate::flight_recorder::TransactionTrace::begin("auto_run.save");
    persistence::save(conn.path(), persisted, false, None)
        .map_err(|error| crate::execution::claim_write_error("save Auto Flow run", error))?;
    emit_auto_run_log(&persisted.run);
    for step in &persisted.steps {
        emit_auto_step_log(step);
    }
    transaction.committed();
    Ok(())
}

pub fn submit_auto_run(
    conn: &AutoFlowStore,
    persisted: &mut PersistedAutoRun,
) -> Result<(), String> {
    let transaction = crate::flight_recorder::TransactionTrace::begin("auto_run.submit");
    persistence::save(conn.path(), persisted, true, None)
        .map_err(|error| crate::execution::claim_write_error("submit Auto Flow run", error))?;
    emit_auto_run_log(&persisted.run);
    for step in &persisted.steps {
        emit_auto_step_log(step);
    }
    transaction.committed();
    Ok(())
}

pub(super) fn save_auto_run_selecting_step(
    conn: &AutoFlowStore,
    persisted: &mut PersistedAutoRun,
    selected_step_index: usize,
) -> Result<i64, String> {
    persistence::save(conn.path(), persisted, false, Some(selected_step_index)).map_err(
        |error| crate::execution::claim_write_error("save selected Auto Flow step", error),
    )?;
    let id = persisted.steps[selected_step_index]
        .id
        .ok_or_else(|| "selected Auto Flow step was not allocated".to_string())?;
    emit_auto_run_log(&persisted.run);
    emit_auto_step_log(&persisted.steps[selected_step_index]);
    Ok(id)
}

pub fn load_auto_run(
    conn: &AutoFlowStore,
    run_id: &str,
) -> Result<Option<PersistedAutoRun>, String> {
    let Some(mut run) = load_run_with_conn(conn, run_id)? else {
        return Ok(None);
    };
    if normalize_active_run(&mut run) {
        run.updated_unix_ms = unix_ms();
        save_run_with_conn(conn, &run)?;
    }
    let steps = load_steps_with_conn(conn, run_id)?;
    Ok(Some(PersistedAutoRun { run, steps }))
}

pub fn load_auto_run_snapshot(
    conn: &AutoFlowStore,
    run_id: &str,
) -> Result<Option<PersistedAutoRun>, String> {
    let Some(mut run) = load_run_with_conn(conn, run_id)? else {
        return Ok(None);
    };
    normalize_active_run(&mut run);
    let steps = load_steps_with_conn(conn, run_id)?;
    Ok(Some(PersistedAutoRun { run, steps }))
}

pub fn load_recent_active_runs_for_repo(
    conn: &AutoFlowStore,
    repo_root: &Path,
    limit: usize,
) -> Result<Vec<PersistedAutoRun>, String> {
    let mut runs = persistence::recent_active(conn.path(), repo_root, limit)
        .map_err(|error| format!("load active Auto Flow runs: {error}"))?;
    for persisted in &mut runs {
        if normalize_active_run(&mut persisted.run) {
            persisted.run.updated_unix_ms = unix_ms();
            save_run_with_conn(conn, &persisted.run)?;
        }
    }
    Ok(runs)
}

pub fn load_recent_active_run_snapshots_for_repo(
    conn: &AutoFlowStore,
    repo_root: &Path,
    limit: usize,
) -> Result<Vec<PersistedAutoRun>, String> {
    let mut runs = persistence::recent_active(conn.path(), repo_root, limit)
        .map_err(|error| format!("load active Auto Flow snapshots: {error}"))?;
    for persisted in &mut runs {
        normalize_active_run(&mut persisted.run);
    }
    Ok(runs)
}

pub fn load_terminal_repair_run_snapshots_for_repo(
    conn: &AutoFlowStore,
    repo_root: &Path,
) -> Result<Vec<PersistedAutoRun>, String> {
    let mut runs = persistence::terminal_repairs(conn.path(), repo_root)
        .map_err(|error| format!("load terminal Auto Flow repairs: {error}"))?;
    for persisted in &mut runs {
        normalize_active_run(&mut persisted.run);
    }
    Ok(runs)
}

fn normalize_active_run(run: &mut AutoRun) -> bool {
    if run.status != AutoRunStatus::Done
        || (run.pending_push.is_none()
            && !run
                .stabilization_status
                .is_some_and(stabilization_model::StabilizationStatus::keeps_run_active))
    {
        return false;
    }
    run.status = AutoRunStatus::Paused;
    if run.pending_push.is_some() {
        run.stabilization_status = Some(stabilization_model::StabilizationStatus::Blocked);
        run.stabilization_blocker = Some(stabilization_model::StabilizationBlocker::PendingPush);
        run.stabilization_next_work =
            Some(stabilization_model::StabilizationWorkKind::PushPendingRepair);
    }
    true
}

pub(super) fn save_run_with_conn(conn: &AutoFlowStore, run: &AutoRun) -> Result<(), String> {
    persistence::save_run(conn.path(), run)
        .map_err(|error| crate::execution::claim_write_error("save Auto Flow run", error))?;
    emit_auto_run_log(run);
    Ok(())
}

pub(super) fn save_step_with_conn(
    conn: &AutoFlowStore,
    step: &mut AutoStepRun,
) -> Result<i64, String> {
    let id = persistence::save_step(conn.path(), step)
        .map_err(|error| crate::execution::claim_write_error("save Auto Flow step", error))?;
    emit_auto_step_log(step);
    Ok(id)
}

pub(super) fn load_run_with_conn(
    conn: &AutoFlowStore,
    run_id: &str,
) -> Result<Option<AutoRun>, String> {
    Ok(persistence::load(conn.path(), run_id)
        .map_err(|error| format!("load Auto Flow run: {error}"))?
        .map(|persisted| persisted.run))
}

pub(super) fn save_observed_change_request_identity(
    conn: &AutoFlowStore,
    run_id: &str,
    identity: Option<&crate::remote::CanonicalChangeRequestIdentity>,
) -> Result<(), String> {
    let identity_json = identity
        .map(|identity| {
            serde_json::to_string(identity)
                .map_err(|error| format!("serialize auto change request identity: {error}"))
        })
        .transpose()?;
    if !persistence::save_identity(conn.path(), run_id, identity_json.as_deref())
        .map_err(|error| crate::execution::claim_write_error("save Auto Flow identity", error))?
    {
        return Err(format!(
            "write auto change request identity: auto flow run not found: {run_id}"
        ));
    }
    Ok(())
}

pub(super) fn load_observed_change_request_identity(
    conn: &AutoFlowStore,
    run_id: &str,
) -> Result<Option<crate::remote::CanonicalChangeRequestIdentity>, String> {
    persistence::load_identity(conn.path(), run_id)
        .map_err(|error| format!("load Auto Flow identity: {error}"))?
        .map(|value| {
            serde_json::from_str(&value)
                .map_err(|error| format!("parse auto change request identity: {error}"))
        })
        .transpose()
}

pub(super) fn load_steps_with_conn(
    conn: &AutoFlowStore,
    run_id: &str,
) -> Result<Vec<AutoStepRun>, String> {
    Ok(persistence::load(conn.path(), run_id)
        .map_err(|error| format!("load Auto Flow steps: {error}"))?
        .map(|persisted| persisted.steps)
        .unwrap_or_default())
}
