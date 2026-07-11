use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::github::{PrCheckState, PrDetails, PrSummary};
use crate::observability::{self, LogLevel};
use crate::plan::PlanExecution;
use crate::plan_run::{
    DEFAULT_OUTPUT_LINES_PER_STEP, PlanAgentEvent, PlanExecutorConfig, PlanRunMode, PlanRunStatus,
    execute_plan_parallel, execute_plan_sequential, load_plan_run, prepare_plan_plugin_config,
    prepare_plan_run_for_resume, request_plan_run_pause,
    retry_failed_steps as retry_plan_failed_steps, retry_from_step as retry_plan_from_step,
    save_plan_run,
};
use crate::repo::Repository;
use crate::review::{ReviewFeedback, ReviewFeedbackFilter, actionable_review_feedback};
use crate::verify::{VerifyMode, VerifyResult};

mod agent_step;
mod control;
mod model;
mod non_agent;
mod output;
mod prompts;
mod runner;
pub(crate) mod stabilization_execute;
pub mod stabilization_model;
pub(crate) mod stabilization_observe;
pub(crate) mod stabilization_plan;
mod state_machine;
mod storage;
mod support;

#[cfg(test)]
mod tests;

pub use control::{
    abort_auto_step, archive_auto_run, fail_auto_run, prepare_auto_run_for_resume,
    reconcile_stale_auto_run, request_auto_run_pause, resume_paused_auto_run, retry_auto_from_step,
    retry_failed_auto_step,
};
pub use model::{
    AutoEvent, AutoExecutorConfig, AutoImplementationSource, AutoLaunch, AutoLaunchOptions,
    AutoOutputKind, AutoOutputLine, AutoRun, AutoRunMode, AutoRunStatus, AutoStatusCounts,
    AutoStepKey, AutoStepRun, AutoStepStatus, PersistedAutoRun, aggregate_step_status,
};
pub use output::{
    append_auto_event, append_output_line, append_output_line_limited, load_output_lines,
};
pub use runner::execute_auto_initial_step;
pub use state_machine::{append_step_run, append_step_run_with_work_guard};
pub use storage::{load_auto_run, load_recent_active_runs_for_repo, migrate_schema, save_auto_run};

pub(crate) use support::unix_ms;

use agent_step::*;
use control::*;
use model::*;
use non_agent::*;
use output::*;
use prompts::*;
use runner::*;
use state_machine::*;
use storage::*;
use support::*;
