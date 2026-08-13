use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::agent::AgentState;
use crate::agent_session::{AgentSessionSlot, AgentSessionWarmupKey, AgentSessionWarmupResult};
use crate::config::Config;
use crate::git::{branch_behind, git_status_label, pull_branch};
use crate::harness::{HarnessConfig, OutputFormat, PromptTransport};
use crate::lifecycle::{
    WorktrunkApprovalStatus, check_worktrunk_approval_status, run_worktrunk_approval_prompt,
};
use crate::observability::append_runtime_message;
use crate::opencode::{self, OpencodeStatus, load_runtime};
use crate::process::{
    ProcessPolicy, command_exists, parse_command_words, run_output_allow_failure,
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
    DefaultBranchPollResult, DeleteSessionKey, DeleteSessionResult, ManagedRepo,
    OpencodeEventResult, OpencodeListenerKey, OpencodePollKey, OpencodePollResult,
    PrPersistenceRequest, PrPollKey, PrPollResult, PrSummarySessionResult, RemoteActionValue,
    SelectedRepoContext, SessionRefreshResult, SessionRefreshSnapshot, TUI_ACTION_JOB_TIMEOUT, Tui,
    TuiJobKey, TuiJobKind, TuiJobPayload, WtObservation, WtPollResult,
};
use crate::tui_jobs::CoalescedFacet;

use crate::util::status_count;

mod logs;
mod opencode_actions;
mod polling;
mod pull_requests;
mod repositories;
mod tmux_agent;
mod tmux_portal;
mod tools;
mod worktrees;

#[cfg(test)]
mod tests;

#[cfg(test)]
use crate::worktrunk::discover_columns as discover_wt_columns;
#[cfg(test)]
use polling::status_label_with_behind;
#[cfg(test)]
use pull_requests::{
    apply_bulk_review_resolution, open_http_url_in_browser, pr_target_choice_list,
    remote_create_mutation_target, remote_pr_choice_keys, remote_pr_worktree_branch,
    run_browser_opener, unresolved_review_thread_ids,
};
#[cfg(test)]
use repositories::worktree_column_choices;
#[cfg(test)]
use worktrees::archived_picker_overflow_message;
