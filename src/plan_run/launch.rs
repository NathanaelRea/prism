use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanLaunch {
    pub repo_root: String,
    pub scope_path: PathBuf,
    pub plan_path: PathBuf,
    pub plan_display: String,
    pub step_name: String,
    pub start_step: usize,
    pub total_steps: usize,
    pub mode: PlanRunMode,
}

impl PlanLaunch {
    pub fn new(
        repo_root: &Path,
        scope_path: &Path,
        plan_path: &Path,
        step_name: impl Into<String>,
        start_step: usize,
        total_steps: usize,
        mode: PlanRunMode,
    ) -> Result<Self, String> {
        if start_step == 0 {
            return Err("start step must be greater than zero".to_string());
        }
        if total_steps == 0 {
            return Err("total steps must be greater than zero".to_string());
        }
        if start_step > total_steps {
            return Err("start step cannot be greater than total steps".to_string());
        }
        Ok(Self {
            repo_root: repo_root.display().to_string(),
            scope_path: scope_path.to_path_buf(),
            plan_path: plan_path.to_path_buf(),
            plan_display: display_plan_path(scope_path, plan_path),
            step_name: step_name.into(),
            start_step,
            total_steps,
            mode,
        })
    }

    pub fn create_run(&self) -> PersistedPlanRun {
        let now = unix_ms();
        let id = self.default_run_id(now);
        let run = PlanRun {
            id: id.clone(),
            repo_root: self.repo_root.clone(),
            scope_path: self.scope_path.clone(),
            plan_path: self.plan_path.clone(),
            plan_display: self.plan_display.clone(),
            step_name: self.step_name.clone(),
            start_step: self.start_step,
            total_steps: self.total_steps,
            mode: self.mode,
            status: PlanRunStatus::Queued,
            pause_requested: false,
            selected_step: self.start_step,
            created_unix_ms: now,
            updated_unix_ms: now,
            archived_unix_ms: None,
        };
        let steps = (self.start_step..=self.total_steps)
            .map(|step| {
                PlanStepRun::queued(
                    &id,
                    step,
                    build_task(&self.plan_display, &self.step_name, step),
                )
            })
            .collect();
        PersistedPlanRun { run, steps }
    }

    pub(super) fn default_run_id(&self, now: u64) -> String {
        format!(
            "plan-{:016x}-{}",
            stable_hash(&self.scope_path) ^ stable_hash(&self.plan_path),
            now
        )
    }
}
