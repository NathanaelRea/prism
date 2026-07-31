use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde_json::Value;

use crate::agent::AgentState;
use crate::agent_session::{AgentSessionSlot, AgentSessionWarmupKey, AgentSessionWarmupResult};
use crate::auto_flow::{
    AutoExecutorDecision, AutoImplementationSource, AutoLaunch, AutoLaunchOptions,
    AutoRunControlIntent, AutoRunMode, AutoRunStatus, AutoStepKey, AutoStepStatus,
    PersistedAutoRun, apply_auto_run_control, archive_auto_run, load_auto_run,
    prepare_auto_run_for_resume,
};
use crate::config::Config;
use crate::git::{branch_behind, git_status_label, has_upstream, pull_branch, selected_dirty};
use crate::harness::{HarnessConfig, OutputFormat, PromptTransport};
use crate::json::{json_bool_field, json_object_field, json_string_field, json_top_level_objects};
use crate::lifecycle::{
    WorktrunkApprovalStatus, check_worktrunk_approval_status, is_worktrunk_approval_failure,
    push_branch, run_pre_pr_checks, run_pre_push_checks, run_worktrunk_approval_prompt,
};
use crate::observability::append_runtime_message;
use crate::opencode::{self, OpencodeStatus, load_runtime};
use crate::plan::{PlanExecution, infer_total_phases, open_plan_mode, select_plan_path};
use crate::plan_run::{
    DEFAULT_OUTPUT_LINES_PER_STEP, PlanRunMode, PlanRunStatus, PlanStepStatus, abort_plan_run,
    abort_plan_step, archive_plan_run, load_plan_run, load_resumable_plan_run,
    prepare_plan_run_for_resume, request_plan_run_pause, resume_paused_plan_run,
    retry_failed_steps, retry_from_step, save_plan_run, skip_plan_step,
};
use crate::process::{
    ProcessPolicy, command_exists, parse_command_words, run_capture, run_output_allow_failure,
};
use crate::remote::dispatcher::{
    create_change_request as create_pull_request,
    fetch_change_request_branch as fetch_pull_request_branch,
    list_change_requests as fetch_pr_summary_index,
    refresh_change_request_cache as refresh_pr_cache,
    refresh_change_request_details_state as refresh_pr_details_cache_state,
    refresh_repository_policy as refresh_repo_policy_cache,
    repository_project as github_remote_repo, wait_for_change_request_merged as wait_for_pr_merged,
};
use crate::remote::{
    PR_SUMMARY_POLL_INTERVAL, PrCache, apply_pr_details_poll_result, apply_pr_summary_poll_result,
    persist_pr_cache_snapshot, pr_cache_comment_count, pr_cache_render_signature,
    pr_details_pollable, pr_summary_or_error, resolve_pr_summary_for_session,
};
use crate::repo::Repository;
use crate::session::{
    CreateWorktreeOutcome, DeleteWorktreeOutcome, archive_worktree_session,
    checkout_worktree_session, create_worktree_session, list_archived_worktrees,
};
use crate::tmux::TmuxWindow;
use crate::tui::{
    DefaultBranchPollResult, DeleteSessionKey, DeleteSessionResult, GitAction, ManagedRepo,
    OpencodeEventResult, OpencodeListenerKey, OpencodePollKey, OpencodePollResult,
    PrPersistenceRequest, PrPollKey, PrPollResult, PrSummarySessionResult, RemoteActionValue,
    RemoteMergeOutcome, RemotePushPrepared, SessionRefreshResult, SessionRefreshSnapshot,
    TUI_ACTION_JOB_TIMEOUT, Tui, TuiJobKey, TuiJobKind, TuiJobPayload, WtPollResult,
};
use crate::tui_jobs::CoalescedFacet;

use crate::util::status_count;

mod auto;
mod opencode_actions;
mod plans;
mod polling;
mod pull_requests;
mod repositories;
mod tmux_agent;
mod tmux_portal;
mod tools;
mod worktrees;

fn reject_claimed_control(
    repo: &Repository,
    kind: crate::execution::WorkflowKind,
    run_id: &str,
    control: &str,
) -> Result<(), String> {
    let workflow = crate::execution::WorkflowIdentity::new(kind, run_id);
    let state = crate::observability::with_writable_db(repo, |conn| {
        crate::execution::dispatch_state(conn, &workflow)
    })?;
    if state == Some(crate::execution::DispatchState::Claimed) {
        return Err(format!(
            "cannot {control} run {run_id} while its executor is active; pause or abort it first"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
use plans::plan_run_mode_from_parallel_confirmation;
#[cfg(test)]
use polling::{discover_wt_columns, status_label_with_behind};
#[cfg(test)]
use pull_requests::{
    apply_bulk_review_resolution, pr_target_choice_list, pr_target_repo_for_choice,
    remote_pr_choice_keys, remote_pr_worktree_branch, run_browser_opener,
    should_prompt_pr_target_choice, unresolved_review_thread_ids,
};
#[cfg(test)]
use worktrees::archived_picker_overflow_message;
