#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("unsupported Prism target OS; Prism supports only Linux and macOS");

mod actions;
mod agent;
mod agent_session;
mod args;
pub mod auto_flow;
mod ci;
pub mod cli;
#[cfg(test)]
#[path = "../test-support/compact_runtime.rs"]
mod compact_runtime;
mod config;
mod desktop_notification;
mod durability;
mod execution;
pub mod file_persistence;
mod flight_recorder;
mod git;
mod github;
mod harness;
mod input;
mod json;
mod lifecycle;
mod observability;
mod opencode;
mod plan;
pub mod plan_run;
mod platform;
mod process;
mod repo;
mod review;
mod run_marker;
mod session;
mod setup;
pub mod storage;
mod terminal;
#[cfg(test)]
mod test_support;
mod tmux;
mod tui;
mod tui_jobs;
mod tui_runtime;
mod tui_signal;
mod ui_state;
mod util;
mod verify;
mod view;
mod worker;
mod workspace;
mod workspace_state;
mod worktrunk;

#[cfg(test)]
mod platform_contract_tests;
