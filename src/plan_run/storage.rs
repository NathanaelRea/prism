use super::*;

pub struct PlanRunStore {
    path: PathBuf,
}

impl PlanRunStore {
    pub fn open(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn validate_claim(&self) -> Result<(), String> {
        Ok(())
    }
}

pub fn save_plan_run(store: &PlanRunStore, persisted: &PersistedPlanRun) -> Result<(), String> {
    store.validate_claim()?;
    let transaction = crate::flight_recorder::TransactionTrace::begin("plan_run.save");
    crate::persistence::plan_run::save(store.path(), persisted, false)
        .map_err(|error| crate::execution::claim_write_error("save plan run", error))?;
    transaction.committed();
    Ok(())
}

pub fn submit_plan_run(store: &PlanRunStore, persisted: &PersistedPlanRun) -> Result<(), String> {
    store.validate_claim()?;
    let transaction = crate::flight_recorder::TransactionTrace::begin("plan_run.submit");
    crate::persistence::plan_run::save(store.path(), persisted, true)
        .map_err(|error| crate::execution::claim_write_error("submit plan run", error))?;
    transaction.committed();
    Ok(())
}

pub fn load_plan_run(
    store: &PlanRunStore,
    run_id: &str,
) -> Result<Option<PersistedPlanRun>, String> {
    crate::persistence::plan_run::load(store.path(), run_id)
        .map_err(|error| format!("load plan run: {error}"))
}

pub fn load_recent_plan_runs_for_repo(
    store: &PlanRunStore,
    repo_root: &Path,
    limit: usize,
) -> Result<Vec<PersistedPlanRun>, String> {
    crate::persistence::plan_run::recent(store.path(), repo_root, limit)
        .map_err(|error| format!("load recent plan runs: {error}"))
}

pub fn load_resumable_plan_run(
    store: &PlanRunStore,
    launch: &PlanLaunch,
) -> Result<Option<PersistedPlanRun>, String> {
    crate::persistence::plan_run::resumable(store.path(), launch)
        .map_err(|error| format!("load resumable plan run: {error}"))
}

pub fn save_plan_step(store: &PlanRunStore, step: &PlanStepRun) -> Result<(), String> {
    save_step_with_store(store, step)
}

pub(super) fn save_run_with_store(store: &PlanRunStore, run: &PlanRun) -> Result<(), String> {
    store.validate_claim()?;
    crate::persistence::plan_run::save_run(store.path(), run)
        .map_err(|error| crate::execution::claim_write_error("write plan run", error))
}

pub(super) fn save_step_with_store(store: &PlanRunStore, step: &PlanStepRun) -> Result<(), String> {
    store.validate_claim()?;
    crate::persistence::plan_run::save_step(store.path(), step)
        .map_err(|error| crate::execution::claim_write_error("write plan step run", error))
}

pub(super) fn load_run_with_store(
    store: &PlanRunStore,
    run_id: &str,
) -> Result<Option<PlanRun>, String> {
    Ok(load_plan_run(store, run_id)?.map(|persisted| persisted.run))
}
