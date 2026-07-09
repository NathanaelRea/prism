use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use serde_json::Value;

use crate::agent::AgentState;
use crate::agent_session::{AgentSessionWarmupKey, AgentSessionWarmupResult};
use crate::auto_flow::{
    AutoExecutorConfig, AutoImplementationSource, AutoLaunch, AutoLaunchOptions, AutoRunMode,
    AutoRunStatus, AutoStepKey, AutoStepStatus, PersistedAutoRun, abort_auto_step, append_step_run,
    archive_auto_run, execute_auto_initial_step, load_auto_run, prepare_auto_run_for_resume,
    request_auto_run_pause, resume_paused_auto_run, retry_auto_from_step,
    retry_failed_auto_step as retry_auto_failed_step, save_auto_run,
    stabilization_execute::{GuardedPushDecision, decide_guarded_push},
    stabilization_observe::build_stabilization_snapshot,
    stabilization_plan::plan as plan_stabilization,
};
use crate::ci::build_ci_failure_prompt;
use crate::config::Config;
use crate::git::{branch_behind, git_status_label, has_upstream, pull_branch, selected_dirty};
use crate::github::{
    PR_SUMMARY_POLL_INTERVAL, PrCacheRepository, apply_pr_details_poll_result,
    fetch_pr_summary_index, github_remote_configured, github_remote_repo, pr_cache_comment_count,
    pr_cache_pollable, pr_cache_render_signature, pr_details_pollable, pr_summary_or_error,
    refresh_pr_cache, refresh_pr_details_cache, refresh_pr_summary_index_for_sessions,
    refresh_repo_policy_cache, wait_for_pr_merged,
};
use crate::json::{json_bool_field, json_object_field, json_string_field, json_top_level_objects};
use crate::lifecycle::{
    WorktrunkApprovalStatus, check_worktrunk_approval_status, create_pull_request,
    create_worktree_session, delete_worktree_session, is_worktrunk_approval_failure,
    merge_pull_request, push_branch, refresh_branch_pr_cache, run_pre_pr_checks,
    run_pre_push_checks, run_worktrunk_approval_prompt,
};
use crate::opencode::{self, OpencodeStatus, load_runtime};
use crate::plan::{PlanExecution, infer_total_phases, open_plan_mode, select_plan_path};
use crate::plan_run::{
    DEFAULT_OUTPUT_LINES_PER_STEP, PlanExecutorConfig, PlanRunMode, PlanRunStatus, PlanStepStatus,
    abort_plan_run, abort_plan_step, archive_plan_run, execute_plan_parallel,
    execute_plan_sequential, load_plan_run, load_resumable_plan_run, prepare_plan_plugin_config,
    prepare_plan_run_for_resume, request_plan_run_pause, resume_paused_plan_run,
    retry_failed_steps, retry_from_step, save_plan_run, skip_plan_step,
};
use crate::process::{command_exists, run_capture};
use crate::repo::Repository;
use crate::review::build_review_fix_prompt;
use crate::session::{
    append_runtime_log, archive_worktree_session, discover_sessions, list_archived_worktrees,
    save_agent_state, unarchive_worktree_session, write_task_metadata,
};
use crate::tmux::TmuxWindow;
use crate::tui::{
    DefaultBranchPollResult, DeleteSessionKey, DeleteSessionResult, ManagedRepo,
    OpencodeEventResult, OpencodePollKey, OpencodePollResult, PlanRunResult, PrPollKey,
    PrPollResult, Tui, WtPollResult,
};

use crate::util::{status_count, yes};

mod auto;
mod opencode_actions;
mod plans;
mod polling;
mod pull_requests;
mod repositories;
mod tmux_agent;
mod tools;
mod worktrees;

#[cfg(test)]
mod tests;

#[cfg(test)]
use polling::{discover_wt_columns, status_label_with_behind};
#[cfg(test)]
use pull_requests::{
    pr_target_choice_list, pr_target_repo_for_choice, run_browser_opener,
    should_prompt_pr_target_choice,
};
#[cfg(test)]
use worktrees::archived_picker_overflow_message;
