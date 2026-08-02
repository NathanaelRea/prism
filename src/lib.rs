#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("unsupported Prism target OS; Prism supports only Linux and macOS");

mod actions;
mod agent_runtime;
pub(crate) use agent_runtime::{agent, agent_session, harness, opencode, tmux};
mod application;
pub use application::cli;
pub(crate) use application::{args, config, setup};
pub mod auto_flow;
pub mod plan_run;
mod remote;
mod repository;
pub(crate) use repository::{git, lifecycle, repo, session, workspace, workspace_state, worktrunk};
mod system;
pub(crate) use system::{
    desktop_notification, durability, json, platform, process, terminal, util,
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
pub(crate) use workflow::{ci, execution, plan, review, verify, worker};
