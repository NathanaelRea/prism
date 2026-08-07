#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("unsupported Prism target OS; Prism supports only Linux and macOS");

mod actions;
mod agent_runtime;
pub(crate) use agent_runtime::{agent, agent_session, harness, opencode, tmux};
mod application;
pub use application::cli;
pub(crate) use application::{args, config, setup};
pub mod extension;
pub mod package;
mod persistence;
mod remote;
mod repository;
pub mod resource;
pub(crate) use repository::{git, lifecycle, repo, session, workspace, workspace_state, worktrunk};
mod system;
pub(crate) use system::{
    async_runtime, desktop_notification, durability, json, notification, platform, process,
    terminal, util,
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
pub use workflow::definition::{
    Binding as WorkflowBinding, CatalogDefinition, CompiledRepeat, CompiledStep, ConditionError,
    ConditionExpr, ConditionValue, DefinitionAuthoringOperations, DefinitionCatalog,
    DefinitionError, DefinitionMigrationPreview, DefinitionSnapshot as CompiledDefinitionSnapshot,
    DefinitionUpdate, ExecutableResolution, ExhaustedPolicy, LaunchCompatibility, LaunchMode,
    PortDefinition, SnapshotDefinition, SourceDiagnostic, WorkflowDefinition,
    commented_template as workflow_definition_template, diagnose_source,
    schema_json as workflow_definition_schema,
};
pub use workflow::effect::{
    EffectContractError, Evidence as WorkflowEvidence, ProtectedEffectKind, ReconciliationStatus,
    protected_effect, validate_effect_request,
};
pub use workflow::engine::{
    ArtifactContent, ArtifactPublication, EffectIntent, EffectReconciler, EffectReconciliation,
    ExecutionClass, ExecutionContext, ReconciliationFuture, ReconciliationResult, WorkerConfig,
    WorkerError, WorkflowWorker,
};
pub use workflow::operations::{
    ApprovalDecision, ArtifactIntegrityFailure, ControlPlaneMetric, DefinitionSnapshot,
    EvidenceBoundApproval, LaunchAdmittedImplementation, LaunchWorkflow,
    ProviderObservationProjection, TriggerDoctorDiagnostic, TriggerHistoryProjection,
    TriggerProjection, WorkflowApprovalProjection, WorkflowArtifactProjection,
    WorkflowAttemptProjection, WorkflowAuditEvent, WorkflowCommand, WorkflowControlScope,
    WorkflowEffectProjection, WorkflowGateProjection, WorkflowHealthReport, WorkflowOperationError,
    WorkflowOperations, WorkflowOutputProjection, WorkflowProjection, WorkflowStep,
    WorkflowStepProjection,
};
pub use workflow::runtime::{CatalogRegistrationError, register_catalog_snapshots};
pub use workflow::trigger::{
    AdmissionDecision as TriggerAdmissionDecision, AdmissionEvaluation, AdmissionOutcome,
    AdmissionPolicy, OverlapPolicy, ProviderItemKind as TriggerProviderItemKind,
    ProviderItemObservation as TriggerProviderItemObservation, ProviderPollAdapter,
    ProviderPollBatch, ProviderPollError, ProviderPollFuture, ProviderPollPage,
    ProviderPollRequest, TriggerContractError, TriggerOccurrenceStatus, TriggerRegistration,
    TriggerSchedule,
};
pub(crate) use workflow::worker;
