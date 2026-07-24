use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::mpsc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{OptionalExtension, params};
use serde_json::Value;

use crate::opencode::{OpencodeState, OpencodeStatus};
use crate::plan::{build_task, display_plan_path};
use crate::repo::Repository;
use crate::util::stable_hash;

mod control;
mod events;
mod executor;
mod launch;
mod model;
mod output;
mod plugin;
mod storage;

#[cfg(test)]
mod tests;

#[cfg(test)]
use executor::opencode_run_command;

pub use control::{
    abort_plan_run, abort_plan_step, archive_plan_run, cleanup_stale_archived_plan_runs,
    prepare_plan_run_for_resume, reconcile_stale_plan_run, request_plan_run_pause,
    resume_paused_plan_run, retry_failed_steps, retry_from_step, skip_plan_step,
};
pub use events::{
    ingest_plan_sse_payload, parse_plan_agent_events, reconcile_plan_step_from_opencode_status,
    reconcile_plan_step_from_server,
};
pub use executor::{execute_plan_parallel, execute_plan_sequential};
pub use launch::PlanLaunch;
pub use model::{
    DEFAULT_OUTPUT_LINES_PER_STEP, PersistedPlanRun, PlanAgentEvent, PlanOutputKind,
    PlanOutputLine, PlanRun, PlanRunMode, PlanRunStatus, PlanStatusCounts, PlanStepRun,
    PlanStepStatus, PlanTodo, aggregate_step_status, plan_output_block_key,
};
pub use output::{append_output_line, load_output_lines};
pub use plugin::{
    DEFAULT_PLAN_AGENT_VARIANT, PlanExecutorConfig, PlanPluginConfig, prepare_plan_plugin_config,
};
pub use storage::{
    load_plan_run, load_recent_plan_runs_for_repo, load_resumable_plan_run, migrate_schema,
    save_plan_run, save_plan_step, submit_plan_run,
};

use control::*;
use events::*;
use model::*;
use output::*;
use storage::*;
