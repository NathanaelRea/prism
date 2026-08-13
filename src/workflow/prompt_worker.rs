//! Worker-owned service around the durable prompt Workflow scheduler.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::remote::request_coordinator::{
    ObservationFreshness, RemoteCoordinatorConfig, RemoteMutationRequest, RemoteMutationResult,
    RemoteObservationKey, RemoteObservationResult, RemotePriority, RemoteRequestCoordinator,
    SystemRemoteClock,
};

use crate::persistence::remote_coordinator::SqliteRemoteCoordinatorStore;
use crate::persistence::workflow_kernel::DurableWorkflowRunStore;

use super::agent_phase::HarnessAgentExecutor;
use super::kernel::{
    SchedulerProgress, StartPromptWorkflow, WorkflowKernelError, WorkflowRunState,
    WorkflowRunStore, WorkflowScheduler,
};
use super::source::CompiledWorkflow;
use super::standard_remote::{PrismProviderExecutor, ProductionStandardTriggerRemote};
use super::standard_triggers::{
    CiFailureTrigger, MergeConflictTrigger, NeedsReviewTrigger, ProcessStandardGitOperations,
    ReadyToMergeTrigger,
};
use super::step_trigger::{
    ExternalTrigger, ExternalTriggerLimits, TriggerRecoveryPolicy, TriggerRegistry,
    TriggerSnapshotStore, TriggerSubject, pin_workflow_triggers,
};

#[derive(Clone)]
pub struct PromptWorkflowService {
    store: Arc<DurableWorkflowRunStore>,
    scheduler: WorkflowScheduler,
    triggers: TriggerRegistry,
    snapshots: TriggerSnapshotStore,
    coordinator: RemoteRequestCoordinator,
    remote_wakes: Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<String>>>,
    active_ticks: Arc<tokio::sync::Mutex<std::collections::BTreeSet<String>>>,
}

impl PromptWorkflowService {
    pub async fn open(path: &Path, state_root: &Path) -> Result<Self, WorkflowKernelError> {
        let store = Arc::new(DurableWorkflowRunStore::open(path).await?);
        let remote_store = Arc::new(
            SqliteRemoteCoordinatorStore::open(path)
                .await
                .map_err(|error| WorkflowKernelError::Persistence(error.to_string()))?,
        );
        let coordinator = RemoteRequestCoordinator::new(
            Arc::new(PrismProviderExecutor),
            Arc::new(SystemRemoteClock),
            remote_store,
            RemoteCoordinatorConfig::default(),
        )
        .await
        .map_err(|error| WorkflowKernelError::Persistence(error.to_string()))?;
        let (wake_sender, wake_receiver) = tokio::sync::mpsc::unbounded_channel();
        let remote = Arc::new(ProductionStandardTriggerRemote::with_wake_sender(
            coordinator.clone(),
            wake_sender,
        ));
        let git = Arc::new(ProcessStandardGitOperations);
        let triggers = TriggerRegistry::default();
        triggers
            .insert(
                "merge_conflict",
                MergeConflictTrigger::new(remote.clone(), git),
            )
            .map_err(trigger_error)?;
        triggers
            .insert("needs_review", NeedsReviewTrigger::new(remote.clone()))
            .map_err(trigger_error)?;
        triggers
            .insert("ci_failure", CiFailureTrigger::new(remote.clone()))
            .map_err(trigger_error)?;
        triggers
            .insert("ready_to_merge", ReadyToMergeTrigger::new(remote))
            .map_err(trigger_error)?;
        for run in store
            .list_runs(None, 10_000)
            .await?
            .into_iter()
            .filter(|run| !run.status.terminal())
        {
            let workflow = store.load_workflow(&run.workflow_digest).await?;
            register_external_triggers(&triggers, &workflow)?;
        }
        let scheduler = WorkflowScheduler::new(
            store.clone(),
            triggers.clone(),
            Arc::new(HarnessAgentExecutor::default()),
        );
        Ok(Self {
            store,
            scheduler,
            triggers,
            snapshots: TriggerSnapshotStore::new(state_root.join("trigger-snapshots")),
            coordinator,
            remote_wakes: Arc::new(tokio::sync::Mutex::new(wake_receiver)),
            active_ticks: Arc::new(tokio::sync::Mutex::new(std::collections::BTreeSet::new())),
        })
    }

    pub async fn launch(
        &self,
        mut workflow: CompiledWorkflow,
        run_id: &str,
        subject: TriggerSubject,
        now_unix_ms: i64,
    ) -> Result<WorkflowRunState, WorkflowKernelError> {
        let repository = crate::repo::Repository {
            root: subject.repository.clone(),
        };
        let config = crate::config::Config::load(&repository);
        super::source::resolve_workflow_agent_selection(&mut workflow, &config).map_err(
            |items| {
                WorkflowKernelError::Invalid(
                    items
                        .into_iter()
                        .map(|item| item.message)
                        .collect::<Vec<_>>()
                        .join("; "),
                )
            },
        )?;
        pin_workflow_triggers(&mut workflow, &self.snapshots).map_err(trigger_error)?;
        register_external_triggers(&self.triggers, &workflow)?;
        self.scheduler
            .start(StartPromptWorkflow {
                run_id,
                workflow: &workflow,
                subject,
                now_unix_ms,
            })
            .await
    }

    pub(crate) async fn remote_observe(
        &self,
        repository: &Path,
        worktree: &Path,
        operation: String,
        subject: String,
        payload: serde_json::Value,
    ) -> Result<RemoteObservationResult<serde_json::Value>, String> {
        let lane = super::standard_remote::lane_for_remote_paths(repository, worktree)?;
        let key = RemoteObservationKey::new(lane, operation, subject)
            .map_err(|error| error.to_string())?;
        self.coordinator
            .observe(
                key,
                ObservationFreshness::any(5_000),
                RemotePriority::BackgroundRefresh,
                payload,
            )
            .await
            .map_err(|error| error.to_string())
    }

    pub(crate) async fn remote_mutate(
        &self,
        repository: &Path,
        worktree: &Path,
        request_id: String,
        operation: String,
        subject: String,
        payload: serde_json::Value,
    ) -> Result<RemoteMutationResult<serde_json::Value>, String> {
        let lane = super::standard_remote::lane_for_remote_paths(repository, worktree)?;
        self.coordinator
            .mutate(RemoteMutationRequest {
                lane,
                request_id,
                operation,
                subject,
                priority: RemotePriority::InteractiveMutation,
                payload,
            })
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn tick(
        &self,
        run_id: &str,
        now_unix_ms: i64,
    ) -> Result<SchedulerProgress, WorkflowKernelError> {
        self.scheduler.tick(run_id, now_unix_ms).await
    }

    pub async fn tick_active(&self, now_unix_ms: i64) -> Result<(), WorkflowKernelError> {
        let pending_wakes = {
            let mut wakes = self.remote_wakes.lock().await;
            let mut pending = Vec::new();
            while let Ok(run_id) = wakes.try_recv() {
                pending.push(run_id);
            }
            pending
        };
        for run_id in pending_wakes {
            let scheduler = self.scheduler.clone();
            tokio::spawn(async move {
                if let Err(error) = scheduler.wake(&run_id, now_unix_ms).await {
                    eprintln!("Prism prompt Workflow {run_id} failed to wake: {error}");
                }
            });
        }
        for run_id in self.store.active_run_ids().await? {
            let mut active = self.active_ticks.lock().await;
            if !active.insert(run_id.clone()) {
                continue;
            }
            drop(active);
            let scheduler = self.scheduler.clone();
            let active_ticks = self.active_ticks.clone();
            tokio::spawn(async move {
                // One lifecycle per pass keeps controls and higher-priority remote work responsive.
                if let Err(error) = scheduler.tick(&run_id, now_unix_ms).await {
                    eprintln!("Prism prompt Workflow {run_id} failed to tick: {error}");
                }
                active_ticks.lock().await.remove(&run_id);
            });
        }
        Ok(())
    }

    pub async fn list(
        &self,
        repository: Option<&Path>,
        limit: usize,
    ) -> Result<Vec<WorkflowRunState>, WorkflowKernelError> {
        self.store.list_runs(repository, limit).await
    }

    pub async fn inspect(
        &self,
        run_id: &str,
    ) -> Result<Option<WorkflowRunState>, WorkflowKernelError> {
        self.store.load_run(run_id).await
    }

    pub async fn pause(&self, run_id: &str, now: i64) -> Result<(), WorkflowKernelError> {
        self.scheduler.pause(run_id, now).await
    }

    pub async fn resume(&self, run_id: &str, now: i64) -> Result<(), WorkflowKernelError> {
        self.scheduler.resume(run_id, now).await
    }

    pub async fn cancel(&self, run_id: &str, now: i64) -> Result<(), WorkflowKernelError> {
        self.scheduler.cancel(run_id, now).await
    }

    pub async fn retry(&self, run_id: &str, now: i64) -> Result<(), WorkflowKernelError> {
        self.scheduler.retry(run_id, now).await
    }

    pub fn database_path() -> PathBuf {
        crate::util::prism_config_dir().join("workflow.db")
    }

    pub fn state_root() -> PathBuf {
        crate::util::prism_config_dir().join("state/prompt-workflow")
    }
}

fn register_external_triggers(
    registry: &TriggerRegistry,
    workflow: &CompiledWorkflow,
) -> Result<(), WorkflowKernelError> {
    for revision in workflow
        .steps
        .iter()
        .filter_map(|step| step.trigger.as_ref())
        .filter(|trigger| trigger.executable.is_some())
    {
        if registry.get(&revision.digest).is_some() {
            continue;
        }
        let executable = revision
            .executable
            .clone()
            .expect("filtered external Trigger executable");
        registry
            .insert(
                revision.digest.clone(),
                ExternalTrigger::new(executable, ExternalTriggerLimits::default())
                    .with_recovery_policy(TriggerRecoveryPolicy {
                        prepare_repeatable: revision.repeatable_prepare,
                        finalize_repeatable: revision.repeatable_finalize,
                    }),
            )
            .map_err(trigger_error)?;
    }
    Ok(())
}

fn trigger_error(error: super::step_trigger::TriggerError) -> WorkflowKernelError {
    WorkflowKernelError::Persistence(error.to_string())
}

pub fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_external_triggers_can_be_rehydrated_from_a_workflow_snapshot() {
        let root =
            std::env::temp_dir().join(format!("prism-trigger-rehydrate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("triggers")).unwrap();
        let executable = root.join("triggers/custom");
        std::fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        let triggers = super::super::source::TriggerCatalog::discover(&root, None, false).unwrap();
        let workflow = super::super::source::compile_workflow(
            std::path::Path::new("custom.toml"),
            "[[step]]\ntrigger='custom'\nprompt='run'\n",
            &triggers,
        )
        .unwrap();
        let revision = workflow.steps[0].trigger.as_ref().unwrap();
        let registry = TriggerRegistry::default();
        register_external_triggers(&registry, &workflow).unwrap();
        assert!(registry.get(&revision.digest).is_some());
        std::fs::remove_dir_all(root).unwrap();
    }
}
