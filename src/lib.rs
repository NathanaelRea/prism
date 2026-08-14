#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("unsupported Prism target OS; Prism supports only Linux and macOS");

mod actions;
mod agent_runtime;
pub(crate) use agent_runtime::{agent, agent_session, harness, opencode, tmux};
mod application;
pub use application::cli;
pub(crate) use application::{args, setup, workspace_state};
pub(crate) mod config;
mod persistence;
mod remote;
mod repository;
pub mod resource;
pub(crate) use repository::{git, lifecycle, repo, session, workspace, worktrunk};
mod system;
#[allow(unused_imports)]
pub(crate) use system::{
    async_runtime, desktop_notification, durability, json, platform, process, terminal, util,
};
pub use system::{file_persistence, storage};
mod telemetry;
pub(crate) use telemetry::{flight_recorder, observability, run_marker};
#[cfg(test)]
mod testing;
#[cfg(test)]
pub(crate) use testing::{compact_runtime, test_support};
mod tui;
pub(crate) use tui::{
    input, jobs as tui_jobs, runtime as tui_runtime, signal as tui_signal, state as ui_state,
};
mod view;
mod workflow;
pub use persistence::remote_coordinator::SqliteRemoteCoordinatorStore;
pub use persistence::workflow_kernel::DurableWorkflowRunStore;
pub use remote::request_coordinator::{
    CoordinatedRemoteOperation, FakeRemoteClock, FreshObservation, MemoryRemoteCoordinatorStore,
    ObservationFreshness, PersistedRemoteLane, PersistedRemoteMutation,
    PersistedRemoteMutationState, RemoteClock, RemoteCoordinatorConfig, RemoteCoordinatorError,
    RemoteCoordinatorStore, RemoteFuture, RemoteLaneKey, RemoteMutationFailureDisposition,
    RemoteMutationReconciliation, RemoteMutationRequest, RemoteMutationResult,
    RemoteObservationKey, RemoteObservationRequest, RemoteObservationResult,
    RemoteOperationExecutor, RemoteOperationFailure, RemoteOperationOutput, RemotePriority,
    RemoteRequestCoordinator, RemoteWait, SystemRemoteClock,
};
pub use workflow::agent_phase::{
    AgentCancellation, AgentExecutionError, AgentExecutor, AgentFuture, AgentRequest,
    HarnessAgentExecutor, RecordingAgentExecutor, prompt_with_context,
};
pub use workflow::kernel::{
    AttemptStatus as PromptAttemptStatus, MemoryWorkflowRunStore, SchedulerProgress,
    StartPromptWorkflow, StepPhase as PromptStepPhase, StoreFuture, WorkflowAttemptState,
    WorkflowEvent as PromptWorkflowEvent, WorkflowKernelError, WorkflowRunState,
    WorkflowRunStatus as PromptWorkflowRunStatus, WorkflowRunStore, WorkflowScheduler,
    WorkflowStepState as PromptWorkflowStepState,
};
pub use workflow::prompt_worker::{PromptWorkflowService, now_unix_ms as workflow_now_unix_ms};
pub use workflow::source::{
    CompiledWorkflow, CompiledWorkflowInput, CompiledWorkflowStep, DEFAULT_MAX_AGENT_RUNS,
    DiscoveredWorkflow, MULTI_MODEL_REVIEW_EXAMPLE, PROMPT_WORKFLOW_TEMPLATE,
    RepositoryResourceSnapshot, ResolvedAgent, TriggerCatalog as StepTriggerCatalog,
    TriggerRevision, TrustedRepositoryResources, WorkflowCatalog as PromptWorkflowCatalog,
    WorkflowDefaults, WorkflowDiagnostic, WorkflowInputDefault, WorkflowInputSource,
    WorkflowInputType, WorkflowScope, WorkflowSource, WorkflowSourceError, WorkflowStepSource,
    archive_legacy_workflow_sources, bind_workflow_inputs, compile_workflow,
    copy_example as copy_workflow_example, prompt_workflow_schema, repository_resource_revision,
    repository_resources_are_trusted, resolve_workflow_agent_selection, seed_editable_defaults,
    trust_repository_resources, trusted_repository_resources, validate_workflow_agent_selection,
    validate_workflow_file_input, validate_workflow_input, workflow_file_input_candidates,
};
pub use workflow::standard_remote::{PrismProviderExecutor, ProductionStandardTriggerRemote};
pub use workflow::standard_triggers::{
    ChangeRequestObservation, CiFailureTrigger, MergeConflictTrigger, MergePreparation,
    MergeRelation, Mergeability, NeedsReviewTrigger, ProcessStandardGitOperations,
    ReadyToMergeTrigger, RequiredCheck, RequiredCheckState, ReviewThreadObservation,
    StandardGitOperations, StandardMutationResult, StandardObservationResult, StandardProvider,
    StandardRemoteFuture, StandardTriggerRemote,
};
pub use workflow::step_trigger::{
    AgentOutcome, AgentOutcomeStatus, ExternalTrigger, ExternalTriggerLimits, PostStepResult,
    PreparedState, ScriptedTrigger, StepTrigger, TRIGGER_PROTOCOL_VERSION, TriggerContext,
    TriggerError, TriggerExecutableSnapshot, TriggerFuture, TriggerPhaseBody, TriggerPhaseEnvelope,
    TriggerPhaseRequest, TriggerPhaseResponse, TriggerRecoveryPolicy, TriggerRegistry,
    TriggerSnapshotStore, TriggerSubject, pin_workflow_triggers,
};
pub(crate) use workflow::worker;
