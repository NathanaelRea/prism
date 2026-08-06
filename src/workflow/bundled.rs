use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::operations::{DefinitionSnapshot, LaunchWorkflow, WorkflowOperations, WorkflowStep};
use crate::WorkflowOperationError;

pub const PLAN_DEFINITION_ID: &str = "bundled-plan-v1";
pub const CODING_DEFINITION_ID: &str = "bundled-coding-v1";

const PLAN_DEFINITION: &str =
    r#"{"kind":"plan","version":1,"steps":"materialized phases","implementation":"harness"}"#;
const CODING_DEFINITION: &str = r#"{"kind":"coding","version":1,"steps":"materialized coding stages","implementation":"harness"}"#;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundledPlanLaunch {
    pub repository: String,
    pub scope_path: PathBuf,
    pub plan_path: PathBuf,
    pub step_name: String,
    pub start_step: usize,
    pub total_steps: usize,
    pub parallel: bool,
    pub harness_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundledCodingLaunch {
    pub repository: String,
    pub worktree_path: PathBuf,
    pub task: String,
    pub plan_path: Option<PathBuf>,
    pub draft_plan: bool,
    pub harness_id: String,
    pub variant: Option<String>,
}

#[derive(Serialize)]
struct HarnessStepInput<'a> {
    repository: &'a str,
    cwd: &'a Path,
    harness_id: &'a str,
    prompt: String,
    title: String,
    variant: Option<&'a str>,
}

pub async fn install(operations: &WorkflowOperations) -> Result<(), WorkflowOperationError> {
    let now = now_ms();
    for (id, name, body, digest) in [
        (
            PLAN_DEFINITION_ID,
            "plan",
            PLAN_DEFINITION,
            "bundled-plan-v1",
        ),
        (
            CODING_DEFINITION_ID,
            "coding",
            CODING_DEFINITION,
            "bundled-coding-v1",
        ),
    ] {
        operations
            .register_definition(DefinitionSnapshot {
                id,
                name,
                revision: "1",
                source: "bundled",
                trusted: true,
                body_json: body,
                digest,
                now_unix_ms: now,
            })
            .await?;
    }
    Ok(())
}

pub async fn launch_plan(
    operations: &WorkflowOperations,
    launch: BundledPlanLaunch,
) -> Result<String, WorkflowOperationError> {
    let now = now_ms();
    let run_id = format!(
        "plan-{:016x}-{now}",
        stable_hash(&format!(
            "{}:{}",
            launch.scope_path.display(),
            launch.plan_path.display()
        ))
    );
    let mut steps = Vec::new();
    for phase in launch.start_step..=launch.total_steps {
        let id = format!("{run_id}:phase:{phase}");
        let dependencies = if launch.parallel || phase == launch.start_step {
            Vec::new()
        } else {
            vec![format!("{run_id}:phase:{}", phase - 1)]
        };
        let prompt = format!(
            "Implement {} {} from `{}`. Complete only this phase, including its tests and verification. Do not commit, push, create a pull request, or merge.",
            launch.step_name,
            phase,
            launch.plan_path.display()
        );
        steps.push(WorkflowStep {
            id,
            key: format!("phase-{phase}"),
            implementation: "harness".into(),
            target_id: "local".into(),
            input_json: serde_json::to_string(&HarnessStepInput {
                repository: &launch.repository,
                cwd: &launch.scope_path,
                harness_id: &launch.harness_id,
                prompt,
                title: format!("Plan phase {phase}"),
                variant: None,
            })
            .expect("bundled plan input is serializable"),
            dependencies,
            resources: vec![format!("workspace:{}", launch.scope_path.display())],
        });
    }
    operations
        .launch_materialized(
            LaunchWorkflow {
                run_id: &run_id,
                definition_snapshot_id: PLAN_DEFINITION_ID,
                repository: Some(&launch.repository),
                idempotency_key: &run_id,
                now_unix_ms: now,
            },
            steps,
        )
        .await
}

pub async fn launch_coding(
    operations: &WorkflowOperations,
    launch: BundledCodingLaunch,
) -> Result<String, WorkflowOperationError> {
    let now = now_ms();
    let run_id = format!(
        "coding-{:016x}-{now}",
        stable_hash(&format!(
            "{}:{}",
            launch.worktree_path.display(),
            launch.task
        ))
    );
    let mut steps = Vec::new();
    let mut implementation_dependency = Vec::new();
    if launch.draft_plan {
        let plan_path = launch
            .plan_path
            .clone()
            .unwrap_or_else(|| launch.worktree_path.join("plan.md"));
        let id = format!("{run_id}:draft-plan");
        implementation_dependency.push(id.clone());
        steps.push(harness_step(
            id,
            "draft-plan",
            &launch,
            format!("Create an implementation plan at `{}` for the task below. Do not implement or commit.\n\nTask:\n{}", plan_path.display(), launch.task),
            Vec::new(),
        ));
    }
    let prompt = match &launch.plan_path {
        Some(path) => format!(
            "Implement the plan in `{}` for the task below. Stop after implementation and verification; do not commit, push, create a pull request, or merge.\n\nTask:\n{}",
            path.display(),
            launch.task
        ),
        None => format!(
            "Implement this task in the current worktree. Stop after implementation and verification; do not commit, push, create a pull request, or merge.\n\nTask:\n{}",
            launch.task
        ),
    };
    steps.push(harness_step(
        format!("{run_id}:implement"),
        "implement",
        &launch,
        prompt,
        implementation_dependency,
    ));
    operations
        .launch_materialized(
            LaunchWorkflow {
                run_id: &run_id,
                definition_snapshot_id: CODING_DEFINITION_ID,
                repository: Some(&launch.repository),
                idempotency_key: &run_id,
                now_unix_ms: now,
            },
            steps,
        )
        .await
}

fn harness_step(
    id: String,
    key: &str,
    launch: &BundledCodingLaunch,
    prompt: String,
    dependencies: Vec<String>,
) -> WorkflowStep {
    WorkflowStep {
        id,
        key: key.into(),
        implementation: "harness".into(),
        target_id: "local".into(),
        input_json: serde_json::to_string(&HarnessStepInput {
            repository: &launch.repository,
            cwd: &launch.worktree_path,
            harness_id: &launch.harness_id,
            prompt,
            title: format!("Coding {key}"),
            variant: launch.variant.as_deref(),
        })
        .expect("bundled coding input is serializable"),
        dependencies,
        resources: vec![format!("workspace:{}", launch.worktree_path.display())],
    }
}

fn stable_hash(value: &str) -> u64 {
    crate::util::stable_hash(Path::new(value))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static SEQUENCE: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn bundled_snapshots_are_idempotent_and_materialize_generic_workflows() {
        let path = std::env::temp_dir().join(format!(
            "prism-bundled-workflows-{}-{}.db",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let operations = WorkflowOperations::open(&path).await.unwrap();
                install(&operations).await.unwrap();
                install(&operations).await.unwrap();

                let plan = launch_plan(
                    &operations,
                    BundledPlanLaunch {
                        repository: "/repo".into(),
                        scope_path: "/repo".into(),
                        plan_path: "/repo/plan.md".into(),
                        step_name: "phase".into(),
                        start_step: 2,
                        total_steps: 4,
                        parallel: false,
                        harness_id: "test".into(),
                    },
                )
                .await
                .unwrap();
                let projection = operations.inspect(&plan).await.unwrap().unwrap();
                assert_eq!(projection.definition_name, "plan");
                assert_eq!(projection.steps.len(), 3);
                assert!(
                    projection
                        .steps
                        .iter()
                        .all(|step| step.status == "runnable")
                );

                let coding = launch_coding(
                    &operations,
                    BundledCodingLaunch {
                        repository: "/repo".into(),
                        worktree_path: "/repo".into(),
                        task: "make it work".into(),
                        plan_path: Some("/repo/plan.md".into()),
                        draft_plan: true,
                        harness_id: "test".into(),
                        variant: None,
                    },
                )
                .await
                .unwrap();
                let projection = operations.inspect(&coding).await.unwrap().unwrap();
                assert_eq!(projection.definition_name, "coding");
                assert_eq!(projection.steps.len(), 2);
                assert!(
                    projection
                        .steps
                        .iter()
                        .all(|step| step.status == "runnable")
                );
            });
        let _ = std::fs::remove_file(path);
    }
}
