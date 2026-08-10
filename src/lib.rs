// Unix-only executable fixture tests are replaced by native Windows capability contracts.
// Some shared helper imports remain after their Unix call sites are cfg'd out.
#![cfg_attr(all(test, windows), allow(dead_code, unused_imports))]

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
compile_error!("unsupported Prism target OS; Prism supports only Linux, macOS, and Windows");

mod actions;
mod agent_runtime;
pub(crate) use agent_runtime::{agent, agent_session, harness, opencode, tmux};
mod application;
pub use application::cli;
pub(crate) use application::{args, config, setup};
pub mod auto_flow;
mod persistence;
pub mod plan_run;
mod remote;
mod repository;
pub(crate) use repository::{git, lifecycle, repo, session, workspace, workspace_state, worktrunk};
mod system;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
pub(crate) use system::desktop_notification;
pub(crate) use system::{
    async_runtime, durability, json, notification, platform, process, terminal, util,
};
pub use system::{file_persistence, storage};
mod telemetry;
pub(crate) use telemetry::{flight_recorder, observability, run_marker};
#[cfg(test)]
mod testing;
#[cfg(all(test, unix))]
pub(crate) use testing::compact_runtime;
#[cfg(test)]
pub(crate) use testing::test_support;
mod tui;
pub(crate) use tui::{
    input, jobs as tui_jobs, runtime as tui_runtime, signal as tui_signal, state as ui_state,
};
mod view;
mod workflow;
pub use workflow::engine::{
    ArtifactContent, ArtifactPublication, EffectIntent, EffectReconciler, EffectReconciliation,
    ExecutionClass, ExecutionContext, ReconciliationFuture, ReconciliationResult, StepFuture,
    StepImplementation, WorkerConfig, WorkerError, WorkflowWorker,
};
pub use workflow::operations::{
    ApprovalDecision, ControlPlaneMetric, DefinitionSnapshot, LaunchWorkflow, LegacyImportSummary,
    WorkflowApprovalProjection, WorkflowArtifactProjection, WorkflowAttemptProjection,
    WorkflowAuditEvent, WorkflowCommand, WorkflowEffectProjection, WorkflowGateProjection,
    WorkflowOperationError, WorkflowOperations, WorkflowOutputProjection, WorkflowProjection,
    WorkflowStep, WorkflowStepProjection,
};
pub(crate) use workflow::{ci, execution, plan, review, verify, worker};
