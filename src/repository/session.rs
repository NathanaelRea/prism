#![allow(
    dead_code,
    reason = "session queries support optional prompt restoration"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::agent::AgentState;
use crate::config::Config;
use crate::git::git_status_label;
use crate::json::json_string_field;
use crate::observability::{self, LogLevel};
use crate::opencode::OpencodeStatus;
use crate::remote::{PrCache, load_pr_cache_for_branch};
use crate::repo::Repository;
use crate::util::{safe_branch_filename, status_count, truncate};

static NEXT_REPOSITORY_INCARNATION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct WorktreeRepositoryKey {
    pub root: PathBuf,
    incarnation: u64,
}

impl WorktreeRepositoryKey {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self {
            root,
            incarnation: NEXT_REPOSITORY_INCARNATION.fetch_add(1, Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum SessionClassification {
    #[default]
    Work,
    Planning,
    Exploration,
}

impl SessionClassification {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Work => "work",
            Self::Planning => "planning",
            Self::Exploration => "exploration",
        }
    }

    fn sort_rank(self) -> u8 {
        match self {
            Self::Work => 0,
            Self::Planning => 1,
            Self::Exploration => 2,
        }
    }

    fn parse(value: &str) -> Self {
        match value.trim() {
            "planning" => Self::Planning,
            "exploration" => Self::Exploration,
            _ => Self::Work,
        }
    }
}

#[derive(Debug)]
pub struct Session {
    pub repo_index: usize,
    pub repo_label: String,
    pub repo_key: Option<char>,
    pub path: PathBuf,
    pub(crate) incarnation: String,
    pub path_display: String,
    pub branch: String,
    pub prompt_summary: String,
    pub classification: SessionClassification,
    pub visibility: i16,
    pub adopted: bool,
    pub hidden: bool,
    pub status_label: String,
    pub agent_state: AgentState,
    pub opencode_status: Option<OpencodeStatus>,
    pub pr: PrCache,
    pub wt_columns: BTreeMap<String, String>,
    pub unseen_comments: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ArchivedWorktree {
    pub branch: String,
    pub worktree_path: String,
    pub classification: SessionClassification,
}

impl Session {
    pub(crate) fn is_default_branch(&self, config: &Config) -> bool {
        config.is_default_branch(&self.branch)
    }

    pub(crate) fn is_detached(&self) -> bool {
        self.branch == "(detached)"
    }

    pub(crate) fn is_task_branch(&self, config: &Config) -> bool {
        !self.is_default_branch(config) && !self.is_detached()
    }

    pub(crate) fn identity_key(&self, repository: &WorktreeRepositoryKey) -> WorktreeSessionKey {
        WorktreeSessionKey {
            repository: repository.clone(),
            path: self.path.clone(),
            branch: self.branch.clone(),
            incarnation: self.incarnation.clone(),
        }
    }

    pub(crate) fn matches_branch(&self, repo_index: usize, branch: &str) -> bool {
        self.repo_index == repo_index && self.branch == branch
    }

    pub(crate) fn apply_repo_identity(
        &mut self,
        repo_index: usize,
        repo_label: String,
        repo_key: Option<char>,
    ) {
        self.repo_index = repo_index;
        self.repo_label = repo_label;
        self.repo_key = repo_key;
    }

    pub(crate) fn preserve_refresh_state_from(&mut self, previous: Session, config: &Config) {
        crate::agent_session::reconcile_session_refresh(
            &mut self.agent_state,
            previous.agent_state,
        );
        crate::opencode::reconcile_session_refresh(
            &mut self.opencode_status,
            previous.opencode_status,
        );
        self.wt_columns = previous.wt_columns;
        let pr_eligible = PrCache::structurally_eligible(&self.branch, config, self.hidden);
        self.pr
            .reconcile_session_refresh(previous.pr, &self.branch, config, self.hidden);
        if pr_eligible {
            self.unseen_comments = previous.unseen_comments;
        } else {
            self.unseen_comments = false;
        }
    }

    pub(crate) fn preserve_concurrent_refresh_state_from(
        &mut self,
        current: &Session,
        baseline: &Session,
    ) {
        if current.prompt_summary != baseline.prompt_summary {
            self.prompt_summary = current.prompt_summary.clone();
        }
        if current.classification != baseline.classification {
            self.classification = current.classification;
        }
        if current.visibility != baseline.visibility {
            self.visibility = current.visibility;
        }
        if current.adopted != baseline.adopted {
            self.adopted = current.adopted;
        }
        if current.hidden != baseline.hidden {
            self.hidden = current.hidden;
        }
        if current.status_label != baseline.status_label {
            self.status_label = current.status_label.clone();
        }
    }

    pub(crate) fn mark_adopted_with_prompt(&mut self, initial_prompt: &str) {
        self.adopted = true;
        self.prompt_summary = prompt_summary_from_text(initial_prompt);
    }

    pub(crate) fn background_job_snapshot(&self) -> Self {
        Self {
            repo_index: self.repo_index,
            repo_label: self.repo_label.clone(),
            repo_key: self.repo_key,
            path: self.path.clone(),
            incarnation: self.incarnation.clone(),
            path_display: self.path_display.clone(),
            branch: self.branch.clone(),
            prompt_summary: self.prompt_summary.clone(),
            classification: self.classification,
            visibility: self.visibility,
            adopted: self.adopted,
            hidden: self.hidden,
            status_label: self.status_label.clone(),
            agent_state: self.agent_state,
            opencode_status: self.opencode_status.clone(),
            pr: self.pr.clone(),
            wt_columns: self.wt_columns.clone(),
            unseen_comments: self.unseen_comments,
        }
    }

    pub(crate) fn deletion_warnings(&self) -> Vec<String> {
        let mut warnings = self.deferred_cleanup_warnings_for_status(&self.status_label);
        if let Some(summary) = self.pr.summary()
            && !summary.merged
        {
            warnings.push(format!("open PR #{} still exists", summary.number));
        }
        warnings
    }

    fn deferred_cleanup_warnings_for_status(&self, status_label: &str) -> Vec<String> {
        let mut warnings = Vec::new();
        if status_count(status_label, "dirty").is_some() {
            warnings.push("dirty worktree: uncommitted changes will be deleted".to_string());
        }
        if status_count(status_label, "ahead").is_some() {
            warnings.push("branch is ahead of upstream: unpushed commits may be lost".to_string());
        }
        if status_count(status_label, "behind").is_some() {
            warnings.push("branch is behind upstream".to_string());
        }
        if !self.adopted {
            warnings.push("session was not created by Prism".to_string());
        }
        if self.is_detached() {
            warnings.push("detached worktree: no local branch will be deleted".to_string());
        }
        if matches!(self.agent_state, AgentState::Attached | AgentState::Running) {
            warnings.push("agent is still running".to_string());
        }
        warnings
    }

    pub(crate) fn archive_warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if status_count(&self.status_label, "dirty").is_some() {
            warnings.push("dirty worktree: uncommitted changes stay on disk".to_string());
        }
        if status_count(&self.status_label, "ahead").is_some() {
            warnings.push("branch is ahead of upstream: unpushed commits stay local".to_string());
        }
        if status_count(&self.status_label, "behind").is_some() {
            warnings.push("branch is behind upstream".to_string());
        }
        if !self.adopted {
            warnings.push("session was not created by Prism".to_string());
        }
        if self.is_detached() {
            warnings.push("detached worktree: no local branch is associated".to_string());
        }
        if matches!(self.agent_state, AgentState::Attached | AgentState::Running) {
            warnings.push("agent is still running".to_string());
        }
        if let Some(summary) = self.pr.summary()
            && !summary.merged
        {
            warnings.push(format!("open PR #{} still exists", summary.number));
        }
        warnings
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CreateWorktreeOutcome {
    Created,
    Restored,
    CreatedMetadataFailed { error: String },
}

#[derive(Debug)]
pub(crate) enum CreateWorktreeFailure {
    Worktrunk(crate::worktrunk::WorktrunkFailure),
    Other(String),
}

impl CreateWorktreeFailure {
    pub(crate) fn approval_required(&self) -> bool {
        matches!(self, Self::Worktrunk(failure) if failure.approval_required())
    }
}

impl std::fmt::Display for CreateWorktreeFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Worktrunk(failure) => failure.fmt(formatter),
            Self::Other(error) => formatter.write_str(error),
        }
    }
}

pub(crate) async fn create_worktree_session(
    repo: &Repository,
    config: &Config,
    branch: &str,
) -> Result<CreateWorktreeOutcome, CreateWorktreeFailure> {
    create_or_checkout_worktree_session(repo, config, branch, false).await
}

pub(crate) async fn checkout_worktree_session(
    repo: &Repository,
    config: &Config,
    branch: &str,
) -> Result<CreateWorktreeOutcome, CreateWorktreeFailure> {
    create_or_checkout_worktree_session(repo, config, branch, true).await
}

async fn create_or_checkout_worktree_session(
    repo: &Repository,
    config: &Config,
    branch: &str,
    checkout: bool,
) -> Result<CreateWorktreeOutcome, CreateWorktreeFailure> {
    if hidden_session_exists(repo, branch).map_err(CreateWorktreeFailure::Other)?
        && crate::lifecycle::branch_has_worktree(repo, config, branch)
            .await
            .map_err(CreateWorktreeFailure::Other)?
    {
        unarchive_worktree_session(repo, branch).map_err(CreateWorktreeFailure::Other)?;
        return Ok(CreateWorktreeOutcome::Restored);
    }
    let switch = if checkout {
        crate::lifecycle::checkout_worktree(repo, config, branch)
            .await
            .map_err(CreateWorktreeFailure::Worktrunk)?
    } else {
        crate::lifecycle::create_worktree(repo, config, branch)
            .await
            .map_err(CreateWorktreeFailure::Worktrunk)?
    };
    if let Err(error) = crate::lifecycle::verify_switch_outcome(repo, config, branch, &switch).await
    {
        return Ok(CreateWorktreeOutcome::CreatedMetadataFailed { error });
    }
    match unarchive_worktree_session(repo, branch) {
        Ok(()) => Ok(CreateWorktreeOutcome::Created),
        Err(error) => Ok(CreateWorktreeOutcome::CreatedMetadataFailed { error }),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DeleteWorktreeOutcome {
    Deleted,
    BranchRetained {
        error: String,
        owned_state_removed: bool,
    },
    DeletedWithWarnings {
        errors: Vec<String>,
        owned_state_removed: bool,
    },
}

#[derive(Clone, Debug)]
struct PendingWorktreeDeletion {
    worktree_path: String,
    worktree_incarnation: String,
    branch_oid: Option<String>,
    worktree_removed: bool,
    branch_deleted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DeferredMergeCleanupStatus {
    NotScheduled,
    Safe,
    Unsafe(String),
}

pub(crate) async fn deferred_merge_cleanup_warnings(
    config: &Config,
    session: &Session,
) -> Vec<String> {
    let status_label = git_status_label(&session.path, config).await;
    session.deferred_cleanup_warnings_for_status(&status_label)
}

pub(crate) async fn schedule_deferred_merge_cleanup(
    repo: &Repository,
    config: &Config,
    session: &Session,
    approved_warnings: &[String],
) -> Result<(), String> {
    let current_warnings = deferred_merge_cleanup_warnings(config, session).await;
    if current_warnings != approved_warnings {
        return Err(
            "worktree safety warnings changed while deletion approval was open; deletion was not scheduled"
                .to_string(),
        );
    }
    let warnings_json = serde_json::to_string(approved_warnings)
        .map_err(|error| format!("encode deferred merge cleanup warnings: {error}"))?;
    let cleanup = crate::persistence::session::DeferredMergeCleanup {
        branch: session.branch.clone(),
        worktree_path: session.path_display.clone(),
        worktree_incarnation: session.incarnation.clone(),
        branch_oid: crate::lifecycle::branch_oid(repo, config, &session.branch).await?,
        warnings_json,
    };
    session_store(repo)?
        .save_deferred_merge_cleanup(&cleanup, unix_seconds())
        .map_err(|error| format!("save deferred merge cleanup: {error}"))
}

pub(crate) fn cancel_deferred_merge_cleanup(
    repo: &Repository,
    branch: &str,
) -> Result<bool, String> {
    let store = session_store(repo)?;
    let scheduled = store
        .load_deferred_merge_cleanup(branch)
        .map_err(|error| format!("load deferred merge cleanup: {error}"))?
        .is_some();
    if scheduled {
        store
            .delete_deferred_merge_cleanup(branch)
            .map_err(|error| format!("delete deferred merge cleanup: {error}"))?;
    }
    Ok(scheduled)
}

pub(crate) async fn deferred_merge_cleanup_status(
    repo: &Repository,
    config: &Config,
    session: &Session,
) -> Result<DeferredMergeCleanupStatus, String> {
    let Some(cleanup) = session_store(repo)?
        .load_deferred_merge_cleanup(&session.branch)
        .map_err(|error| format!("load deferred merge cleanup: {error}"))?
    else {
        return Ok(DeferredMergeCleanupStatus::NotScheduled);
    };
    if cleanup.worktree_path != session.path_display
        || cleanup.worktree_incarnation != session.incarnation
    {
        return Ok(DeferredMergeCleanupStatus::Unsafe(
            "the Worktree Session identity changed while merge was pending".to_string(),
        ));
    }
    let branch_oid = crate::lifecycle::branch_oid(repo, config, &session.branch).await?;
    if cleanup.branch_oid != branch_oid {
        return Ok(DeferredMergeCleanupStatus::Unsafe(
            "the branch advanced while merge was pending".to_string(),
        ));
    }
    let approved_warnings = serde_json::from_str::<Vec<String>>(&cleanup.warnings_json)
        .map_err(|error| format!("decode deferred merge cleanup warnings: {error}"))?;
    let current_warnings = deferred_merge_cleanup_warnings(config, session).await;
    if current_warnings != approved_warnings {
        return Ok(DeferredMergeCleanupStatus::Unsafe(
            "worktree deletion warning facts changed while merge was pending".to_string(),
        ));
    }
    Ok(DeferredMergeCleanupStatus::Safe)
}

pub(crate) async fn delete_deferred_merge_cleanup_if_current(
    repo: &Repository,
    config: &Config,
    session: &Session,
) -> Result<DeleteWorktreeOutcome, String> {
    match deferred_merge_cleanup_status(repo, config, session).await? {
        DeferredMergeCleanupStatus::Safe => {}
        DeferredMergeCleanupStatus::NotScheduled => {
            return Err("deferred worktree deletion is no longer scheduled".to_string());
        }
        DeferredMergeCleanupStatus::Unsafe(reason) => {
            cancel_deferred_merge_cleanup(repo, &session.branch)?;
            return Err(format!("deferred worktree deletion was canceled: {reason}"));
        }
    }
    let result = delete_worktree_session_if_current(
        repo,
        config,
        &session.path,
        &session.branch,
        Some(&session.incarnation),
    )
    .await;
    let should_cancel = match &result {
        Ok(DeleteWorktreeOutcome::Deleted) => false,
        Ok(
            DeleteWorktreeOutcome::BranchRetained {
                owned_state_removed,
                ..
            }
            | DeleteWorktreeOutcome::DeletedWithWarnings {
                owned_state_removed,
                ..
            },
        ) => !owned_state_removed,
        Err(_) => true,
    };
    if should_cancel {
        cancel_deferred_merge_cleanup(repo, &session.branch)?;
    }
    result
}

fn load_pending_worktree_deletion(
    repo: &Repository,
    branch: &str,
) -> Result<Option<PendingWorktreeDeletion>, String> {
    session_store(repo)?
        .load_pending_deletion(branch)
        .map(|pending| pending.map(pending_deletion_from_record))
        .map_err(|error| format!("load pending worktree deletion: {error}"))
}

fn load_pending_worktree_deletions(
    repo: &Repository,
) -> Result<Vec<(String, PendingWorktreeDeletion)>, String> {
    session_store(repo)?
        .list_pending_deletions()
        .map(|rows| {
            rows.into_iter()
                .map(|row| {
                    let branch = row.branch.clone();
                    (branch, pending_deletion_from_record(row))
                })
                .collect()
        })
        .map_err(|error| format!("load pending worktree deletions: {error}"))
}

fn pending_deletion_from_record(
    row: crate::persistence::session::PendingDeletion,
) -> PendingWorktreeDeletion {
    PendingWorktreeDeletion {
        worktree_path: row.worktree_path,
        worktree_incarnation: row.worktree_incarnation,
        branch_oid: row.branch_oid,
        worktree_removed: row.worktree_removed,
        branch_deleted: row.branch_deleted,
    }
}

fn save_pending_worktree_deletion(
    repo: &Repository,
    path: &Path,
    branch: &str,
    incarnation: &str,
    branch_oid: Option<&str>,
) -> Result<(), String> {
    session_store(repo)?
        .save_pending_deletion(
            branch,
            &path.display().to_string(),
            incarnation,
            branch_oid,
            unix_seconds(),
        )
        .map_err(|error| format!("save pending worktree deletion: {error}"))
}

fn mark_pending_deletion_phase(
    repo: &Repository,
    branch: &str,
    column: &str,
) -> Result<(), String> {
    let worktree_removed = match column {
        "worktree_removed" => true,
        "branch_deleted" => false,
        _ => return Err(format!("unknown pending deletion phase: {column}")),
    };
    session_store(repo)?
        .mark_pending_phase(branch, worktree_removed, unix_seconds())
        .map_err(|error| format!("record pending worktree deletion phase: {error}"))
}

pub(crate) fn worktree_deletion_is_pending(
    repo: &Repository,
    path: &Path,
    branch: &str,
    expected_incarnation: &str,
) -> Result<bool, String> {
    Ok(
        load_pending_worktree_deletion(repo, branch)?.is_some_and(|pending| {
            pending.worktree_path == path.display().to_string()
                && pending.worktree_incarnation == expected_incarnation
        }),
    )
}

pub(crate) fn worktree_removal_is_complete(
    repo: &Repository,
    path: &Path,
    branch: &str,
    expected_incarnation: &str,
) -> Result<bool, String> {
    Ok(
        load_pending_worktree_deletion(repo, branch)?.is_some_and(|pending| {
            pending.worktree_path == path.display().to_string()
                && pending.worktree_incarnation == expected_incarnation
                && pending.worktree_removed
        }),
    )
}

pub(crate) async fn delete_worktree_session_if_current(
    repo: &Repository,
    config: &Config,
    path: &Path,
    branch: &str,
    expected_incarnation: Option<&str>,
) -> Result<DeleteWorktreeOutcome, String> {
    let path_display = path.display().to_string();
    let pending = load_pending_worktree_deletion(repo, branch)?;
    if let Some(pending) = &pending
        && (pending.worktree_path != path_display
            || expected_incarnation
                .is_some_and(|expected| pending.worktree_incarnation != expected))
    {
        return Err(format!(
            "deletion of branch {branch} belongs to a different worktree identity; retained the current worktree"
        ));
    }
    let deletion_incarnation = pending
        .as_ref()
        .map(|pending| pending.worktree_incarnation.as_str())
        .or(expected_incarnation);
    let current_incarnation = worktree_incarnation(path);
    if !current_incarnation.is_empty()
        && deletion_incarnation.is_some_and(|expected| current_incarnation != expected)
    {
        return Err(format!(
            "worktree {branch} was replaced while deletion was pending; retained the replacement"
        ));
    }
    let live_before_removal = crate::lifecycle::list_worktrees(repo, config).await?;
    let current = live_before_removal
        .into_iter()
        .find(|entry| crate::worktrunk::paths_equivalent(&entry.path, path));
    if current.is_none() && expected_incarnation.is_some() && pending.is_none() {
        return Err(format!(
            "worktree {branch} is no longer present at {}; retained its branch and Prism state",
            path.display()
        ));
    }
    if let Some(current) = &current
        && current.branch != branch
    {
        return Err(format!(
            "worktree changed from branch {branch} to {}; retained the current worktree",
            current.branch
        ));
    }
    let branch_incarnation = match &pending {
        Some(pending) => pending.branch_oid.clone(),
        None if branch == "(detached)" => None,
        None => Some(crate::lifecycle::branch_oid(repo, config, branch).await?),
    };
    if pending.is_none() {
        save_pending_worktree_deletion(
            repo,
            path,
            branch,
            deletion_incarnation.unwrap_or(&current_incarnation),
            branch_incarnation.as_deref(),
        )?;
    }
    let already_removed = pending.is_some() && current.is_none() && current_incarnation.is_empty();
    let (removal, removal_warning) = if already_removed {
        (
            crate::worktrunk::RemoveOutcome {
                path: path.to_path_buf(),
                branch: (branch != "(detached)").then(|| branch.to_string()),
            },
            None,
        )
    } else {
        match crate::lifecycle::remove_worktree(repo, config, path).await {
            Ok(removal) => (removal, None),
            Err(error) => {
                let live = crate::lifecycle::list_worktrees(repo, config).await?;
                if live
                    .iter()
                    .any(|entry| crate::worktrunk::paths_equivalent(&entry.path, path))
                {
                    return Err(error.to_string());
                }
                let warning = format!(
                    "Worktrunk removed the worktree but reported failure; retained the branch and Prism state: {}",
                    error.safe_summary()
                );
                mark_pending_deletion_phase(repo, branch, "worktree_removed")?;
                return Ok(DeleteWorktreeOutcome::BranchRetained {
                    error: warning,
                    owned_state_removed: false,
                });
            }
        }
    };
    if branch != "(detached)" && removal.branch.as_deref() != Some(branch) {
        return Err(format!(
            "Worktrunk removed {} but reported branch {:?} instead of {branch:?}; retained the branch and Prism state",
            removal.path.display(),
            removal.branch
        ));
    }
    let live_after_removal = crate::lifecycle::list_worktrees(repo, config).await?;
    if live_after_removal
        .iter()
        .any(|entry| crate::worktrunk::paths_equivalent(&entry.path, path))
    {
        return Err(format!(
            "worktree {branch} was recreated at {} during deletion; retained its resources and state",
            path.display()
        ));
    }
    if branch != "(detached)"
        && live_after_removal
            .iter()
            .any(|entry| entry.branch == branch)
    {
        return Ok(DeleteWorktreeOutcome::BranchRetained {
            error: format!("branch {branch} is attached to a new worktree and was retained"),
            owned_state_removed: false,
        });
    }
    if !worktree_incarnation(path).is_empty() {
        return Err(format!(
            "worktree {branch} was recreated at {} during deletion; retained its resources and state",
            path.display()
        ));
    }

    let mut errors = removal_warning.into_iter().collect::<Vec<_>>();
    errors.extend(mark_pending_deletion_phase(repo, branch, "worktree_removed").err());
    let mut branch_deleted = pending
        .as_ref()
        .is_some_and(|pending| pending.branch_deleted);
    if !branch_deleted
        && pending
            .as_ref()
            .is_some_and(|pending| pending.worktree_removed)
        && branch != "(detached)"
        && !crate::lifecycle::branch_exists(repo, config, branch).await?
    {
        mark_pending_deletion_phase(repo, branch, "branch_deleted")?;
        branch_deleted = true;
    }
    if !branch_deleted
        && let Err(error) = crate::lifecycle::delete_branch_if_same_incarnation(
            repo,
            config,
            branch,
            branch_incarnation.as_deref(),
        )
        .await
    {
        if errors.is_empty() {
            return Ok(DeleteWorktreeOutcome::BranchRetained {
                error,
                owned_state_removed: false,
            });
        }
        errors.push(error);
        return Ok(DeleteWorktreeOutcome::DeletedWithWarnings {
            errors,
            owned_state_removed: false,
        });
    }
    if !branch_deleted {
        errors.extend(mark_pending_deletion_phase(repo, branch, "branch_deleted").err());
    }
    let cleanup_error = remove_deleted_worktree_owned_state(repo, config, path, branch)
        .await
        .err();
    let owned_state_removed = cleanup_error.is_none();
    errors.extend(cleanup_error);
    if errors.is_empty() {
        Ok(DeleteWorktreeOutcome::Deleted)
    } else {
        Ok(DeleteWorktreeOutcome::DeletedWithWarnings {
            errors,
            owned_state_removed,
        })
    }
}

async fn shutdown_worktree_session_resources(
    repo: &Repository,
    config: &Config,
    branch: &str,
    runtimes: &[crate::opencode::OpencodeRuntime],
) -> Result<(), String> {
    let mut errors = Vec::new();
    if let Err(error) = crate::agent_session::shutdown(repo, config, branch).await {
        errors.push(error);
    }
    if let Err(error) =
        crate::opencode::shutdown_worktree_session_runtime_processes_with_lock_held(repo, runtimes)
            .await
    {
        errors.push(error);
    }
    if let Err(error) = crate::agent_session::remove_owned_log(repo, branch) {
        errors.push(error);
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct WorktreeSessionKey {
    pub repository: WorktreeRepositoryKey,
    pub path: PathBuf,
    pub branch: String,
    pub incarnation: String,
}

pub(crate) struct WorktreeSessionRepository<'a> {
    pub repo_index: usize,
    pub repo: &'a Repository,
    pub config: &'a Config,
    pub label: &'a str,
    pub key: Option<char>,
    pub identity: &'a WorktreeRepositoryKey,
}

pub(crate) async fn refresh_worktree_sessions(
    repositories: &[WorktreeSessionRepository<'_>],
    previous_repository_identities: &BTreeMap<usize, WorktreeRepositoryKey>,
    current: &mut Vec<Session>,
) -> Result<(), String> {
    let mut discovered_by_repository = Vec::new();
    for repository in repositories {
        discovered_by_repository.push(discover_sessions(repository.repo, repository.config).await?);
    }
    let mut previous = std::mem::take(current)
        .into_iter()
        .filter_map(|session| {
            let repository = previous_repository_identities.get(&session.repo_index)?;
            Some((session.identity_key(repository), session))
        })
        .collect::<BTreeMap<_, _>>();
    let mut refreshed = Vec::new();
    for (repository, mut discovered) in repositories.iter().zip(discovered_by_repository) {
        for session in &mut discovered {
            session.apply_repo_identity(
                repository.repo_index,
                repository.label.to_string(),
                repository.key,
            );
            let identity = session.identity_key(repository.identity);
            if let Some(old) = previous.remove(&identity) {
                session.preserve_refresh_state_from(old, repository.config);
            }
        }
        refreshed.extend(discovered);
    }
    for (identity, mut session) in previous {
        let Some(repository) = repositories
            .iter()
            .find(|repository| *repository.identity == identity.repository)
        else {
            continue;
        };
        if worktree_deletion_is_pending(
            repository.repo,
            &session.path,
            &session.branch,
            &session.incarnation,
        )? {
            session.apply_repo_identity(
                repository.repo_index,
                repository.label.to_string(),
                repository.key,
            );
            session.hidden = false;
            session.status_label = "deletion pending".to_string();
            refreshed.push(session);
        }
    }
    *current = refreshed;
    Ok(())
}

pub(crate) async fn discover_sessions(
    repo: &Repository,
    config: &Config,
) -> Result<Vec<Session>, String> {
    let inventory = crate::lifecycle::list_worktrees(repo, config).await?;
    let hidden = load_hidden_sessions(repo)?;
    let mut sessions = Vec::new();

    for entry in inventory {
        if entry.path.exists() {
            let mut session = build_session(repo, entry.path, entry.branch, config).await?;
            session.hidden = hidden.contains_key(&session.branch);
            if session.hidden {
                session.pr = PrCache::default();
                session.unseen_comments = false;
                observability::emit(observability::EventInput {
                    level: LogLevel::Debug,
                    target: "session",
                    action: "unfocused_worktree",
                    operation_id: None,
                    parent_operation_id: None,
                    branch: Some(session.branch.clone()),
                    session: Some(session.path.display().to_string()),
                    message: format!("worktree is unfocused {}", session.path.display()),
                    data_json: None,
                });
            }
            sessions.push(session);
        } else {
            observability::emit(observability::EventInput {
                level: LogLevel::Warn,
                target: "session",
                action: "skip_missing_worktree",
                operation_id: None,
                parent_operation_id: None,
                branch: Some(entry.branch),
                session: Some(entry.path.display().to_string()),
                message: format!("skipping missing worktree {}", entry.path.display()),
                data_json: None,
            });
        }
    }

    for (branch, pending) in load_pending_worktree_deletions(repo)? {
        let path = PathBuf::from(&pending.worktree_path);
        if sessions
            .iter()
            .any(|session| session.path == path && session.branch == branch)
        {
            continue;
        }
        let metadata = load_task_metadata(repo, &branch)?;
        sessions.push(Session {
            repo_index: 0,
            repo_label: String::new(),
            repo_key: None,
            path: path.clone(),
            incarnation: pending.worktree_incarnation,
            path_display: pending.worktree_path,
            branch: branch.clone(),
            prompt_summary: metadata
                .as_ref()
                .map(|metadata| metadata.prompt_summary.clone())
                .unwrap_or_default(),
            classification: metadata
                .as_ref()
                .map(|metadata| metadata.classification)
                .unwrap_or_default(),
            visibility: metadata
                .as_ref()
                .map(|metadata| metadata.visibility)
                .unwrap_or_default(),
            adopted: metadata.is_some(),
            hidden: false,
            status_label: "deletion pending".to_string(),
            agent_state: load_agent_state(repo, &branch).unwrap_or(AgentState::Idle),
            opencode_status: None,
            pr: PrCache::default(),
            wt_columns: BTreeMap::new(),
            unseen_comments: false,
        });
    }

    sessions.sort_by(|a, b| session_discovery_order(config, a, b));
    Ok(sessions)
}

pub(crate) async fn reconcile_worktree_state(
    repo: &Repository,
    config: &Config,
) -> Result<(), String> {
    crate::lifecycle::prune_worktrees(repo, config).await?;
    let live = crate::lifecycle::list_worktrees(repo, config).await?;
    let persisted = session_store(repo)?
        .persisted_worktrees()
        .map_err(|error| format!("read worktree state inventory: {error}"))?
        .into_iter()
        .map(|(branch, path)| (branch, PathBuf::from(path)))
        .collect::<Vec<_>>();

    let mut persisted_by_branch = BTreeMap::<String, Vec<PathBuf>>::new();
    for (branch, path) in persisted {
        persisted_by_branch.entry(branch).or_default().push(path);
    }
    for (branch, paths) in persisted_by_branch {
        if load_pending_worktree_deletion(repo, &branch)?.is_some() {
            continue;
        }
        let exact_live = live
            .iter()
            .any(|entry| entry.branch == branch && paths.contains(&entry.path));
        if exact_live {
            continue;
        }
        if let Some(replacement) = live.iter().find(|entry| entry.branch == branch) {
            for path in &paths {
                crate::opencode::shutdown_worktree_session_runtimes(repo, &branch, path).await?;
            }
            let old_path = paths[0].display().to_string();
            let replacement_path = replacement.path.display().to_string();
            let replacement_incarnation = worktree_incarnation(&replacement.path);
            session_store(repo)?
                .repoint_worktree(
                    &branch,
                    &old_path,
                    &replacement_path,
                    &replacement_incarnation,
                    unix_seconds(),
                )
                .map_err(|error| format!("repoint moved worktree state: {error}"))?;
        } else {
            let path = &paths[0];
            remove_worktree_session_owned_state(repo, config, path, &branch).await?;
            observability::emit(observability::EventInput {
                level: LogLevel::Info,
                target: "session",
                action: "remove_stale_worktree",
                operation_id: None,
                parent_operation_id: None,
                branch: Some(branch),
                session: Some(path.display().to_string()),
                message: format!("removed stale worktree state for {}", path.display()),
                data_json: None,
            });
        }
    }

    let (runtime_sessions, agent_branches) = session_store(repo)?
        .unadopted_state()
        .map_err(|error| format!("read non-adopted session state: {error}"))?;
    let runtime_sessions = runtime_sessions
        .into_iter()
        .map(|(branch, path)| (branch, PathBuf::from(path)))
        .collect::<Vec<_>>();
    let mut cleaned_branches = BTreeSet::new();
    for (branch, path) in runtime_sessions {
        if live
            .iter()
            .any(|entry| entry.branch == branch && entry.path == path)
        {
            continue;
        }
        if live.iter().any(|entry| entry.branch == branch) {
            crate::opencode::shutdown_worktree_session_runtimes(repo, &branch, &path).await?;
            continue;
        }
        remove_worktree_session_owned_state(repo, config, &path, &branch).await?;
        cleaned_branches.insert(branch);
    }
    for branch in agent_branches {
        if cleaned_branches.contains(&branch) || live.iter().any(|entry| entry.branch == branch) {
            continue;
        }
        crate::agent_session::shutdown(repo, config, &branch).await?;
        crate::agent_session::remove_owned_log(repo, &branch)?;
        crate::agent_session::remove_state(repo, &branch)?;
    }
    Ok(())
}

async fn remove_worktree_session_owned_state(
    repo: &Repository,
    config: &Config,
    path: &Path,
    branch: &str,
) -> Result<(), String> {
    remove_worktree_owned_state(repo, config, path, branch).await
}

async fn remove_deleted_worktree_owned_state(
    repo: &Repository,
    config: &Config,
    path: &Path,
    branch: &str,
) -> Result<(), String> {
    remove_worktree_owned_state(repo, config, path, branch).await
}

async fn remove_worktree_owned_state(
    repo: &Repository,
    config: &Config,
    path: &Path,
    branch: &str,
) -> Result<(), String> {
    let worktree_path = path.display().to_string();
    ensure_cleanup_ownership(repo, branch, &worktree_path)?;
    let _server_lock = crate::opencode::lock_repository_server(repo)?;
    let runtimes = crate::opencode::load_runtimes_for_worktree_session(repo, branch, path)?;
    shutdown_worktree_session_resources(repo, config, branch, &runtimes).await?;
    let transaction = crate::flight_recorder::TransactionTrace::begin("session.remove_owned_state");
    session_store(repo)?
        .remove_owned_state(branch, &worktree_path, &runtimes)
        .map_err(|error| format!("remove worktree session state: {error}"))?;
    crate::workflow::ai::remove_drafts_for_worktree(repo, path)?;
    transaction.committed();
    Ok(())
}

fn ensure_cleanup_ownership(
    repo: &Repository,
    branch: &str,
    worktree_path: &str,
) -> Result<(), String> {
    let current_path = session_store(repo)?
        .cleanup_owner(branch)
        .map_err(|error| format!("inspect Worktree Session cleanup ownership: {error}"))?;
    if current_path
        .as_deref()
        .is_some_and(|current| current != worktree_path)
    {
        Err(format!(
            "retained state for {branch}: it now belongs to worktree {current_path:?}"
        ))
    } else {
        Ok(())
    }
}

fn session_discovery_order(config: &Config, a: &Session, b: &Session) -> std::cmp::Ordering {
    a.hidden
        .cmp(&b.hidden)
        .then_with(|| {
            b.is_default_branch(config)
                .cmp(&a.is_default_branch(config))
        })
        .then_with(|| {
            a.classification
                .sort_rank()
                .cmp(&b.classification.sort_rank())
        })
        .then_with(|| a.branch.cmp(&b.branch))
        .then_with(|| a.path.cmp(&b.path))
}

async fn build_session(
    repo: &Repository,
    path: PathBuf,
    branch: String,
    config: &Config,
) -> Result<Session, String> {
    let legacy_metadata_path = path
        .join(".agent/tasks")
        .join(format!("{}.json", safe_branch_filename(&branch)));
    let metadata = load_task_metadata(repo, &branch)?;
    let prompt_summary = metadata
        .as_ref()
        .map(|metadata| metadata.prompt_summary.clone())
        .or_else(|| read_prompt_summary(&legacy_metadata_path))
        .unwrap_or_default();
    let classification = metadata
        .as_ref()
        .map(|metadata| metadata.classification)
        .unwrap_or_default();
    let visibility = metadata
        .as_ref()
        .map(|metadata| metadata.visibility)
        .unwrap_or_default();
    let adopted = metadata.is_some() || legacy_metadata_path.exists();
    let status_label = git_status_label(&path, config).await;
    let path_display = path.display().to_string();
    let incarnation = worktree_incarnation(&path);
    let agent_state = load_agent_state(repo, &branch).unwrap_or(AgentState::Idle);
    let pr = load_pr_cache_for_branch(repo, config, &branch, &path).await;
    Ok(Session {
        repo_index: 0,
        repo_label: String::new(),
        repo_key: None,
        path,
        incarnation,
        path_display,
        branch,
        prompt_summary,
        classification,
        visibility,
        adopted,
        hidden: false,
        status_label,
        agent_state,
        opencode_status: None,
        pr,
        wt_columns: BTreeMap::new(),
        unseen_comments: false,
    })
}

pub(crate) fn worktree_incarnation(path: &Path) -> String {
    let git_link = path.join(".git");
    let Ok(metadata) = fs::metadata(&git_link) else {
        return String::new();
    };
    if metadata.is_dir() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            return format!("directory:{}:{}", metadata.dev(), metadata.ino());
        }
        #[cfg(windows)]
        return file_id::get_file_id(&git_link)
            .map(|identity| format!("directory:{identity:?}"))
            .unwrap_or_default();
    }
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let target = fs::read_to_string(&git_link).unwrap_or_default();
    #[cfg(unix)]
    let file_id = {
        use std::os::unix::fs::MetadataExt;
        metadata.ino().to_string()
    };
    #[cfg(windows)]
    let file_id = file_id::get_file_id(&git_link)
        .map(|identity| format!("{identity:?}"))
        .unwrap_or_default();
    format!("{file_id}:{modified}:{}:{target}", metadata.len())
}

fn write_task_metadata(
    repo: &Repository,
    session: &Session,
    initial_prompt: &str,
) -> Result<(), String> {
    let summary = prompt_summary_from_text(initial_prompt);
    session_store(repo)?
        .write_task_metadata(&crate::persistence::session::TaskMetadataInput {
            branch: &session.branch,
            prompt_summary: &summary,
            initial_prompt,
            worktree: &session.path_display,
            classification: session.classification.label(),
            visibility: i64::from(session.visibility),
            updated_unix_ms: unix_seconds(),
        })
        .map_err(|error| format!("write task metadata: {error}"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AdoptWorktreeOutcome {
    Adopted,
    WorktreeCreatedMetadataFailed { error: String },
}

pub(crate) fn adopt_worktree_session(
    repo: &Repository,
    session: &mut Session,
    initial_prompt: &str,
) -> AdoptWorktreeOutcome {
    match write_task_metadata(repo, session, initial_prompt) {
        Ok(()) => {
            session.mark_adopted_with_prompt(initial_prompt);
            AdoptWorktreeOutcome::Adopted
        }
        Err(error) => AdoptWorktreeOutcome::WorktreeCreatedMetadataFailed { error },
    }
}

pub(crate) fn set_worktree_visibility(
    repo: &Repository,
    session: &Session,
    visibility: i16,
) -> Result<(), String> {
    session_store(repo)?
        .set_visibility(&crate::persistence::session::TaskMetadataInput {
            branch: &session.branch,
            prompt_summary: &session.prompt_summary,
            initial_prompt: "",
            worktree: &session.path_display,
            classification: session.classification.label(),
            visibility: i64::from(visibility),
            updated_unix_ms: unix_seconds(),
        })
        .map_err(|error| format!("write worktree visibility: {error}"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorktreeHarnessAssociation {
    pub harness_id: String,
    pub keep: bool,
}

pub(crate) fn worktree_harness(
    repo: &Repository,
    session: &Session,
) -> Result<WorktreeHarnessAssociation, String> {
    let store = session_store(repo)?;
    let stored = store
        .load_harness(&session.branch)
        .map_err(|error| format!("load worktree harness: {error}"))?;
    if let Some(stored) = stored
        && stored.worktree_path == session.path_display
        && stored.worktree_incarnation == session.incarnation
    {
        return Ok(WorktreeHarnessAssociation {
            harness_id: stored.harness_id,
            keep: stored.migration_policy == "keep",
        });
    }
    set_worktree_harness(repo, session, "opencode", false)?;
    Ok(WorktreeHarnessAssociation {
        harness_id: "opencode".to_string(),
        keep: false,
    })
}

pub(crate) fn set_worktree_harness(
    repo: &Repository,
    session: &Session,
    harness_id: &str,
    keep: bool,
) -> Result<(), String> {
    session_store(repo)?
        .set_harness(&crate::persistence::session::WorktreeHarnessInput {
            branch: &session.branch,
            worktree_path: &session.path_display,
            worktree_incarnation: &session.incarnation,
            harness_id,
            migration_policy: if keep { "keep" } else { "ask" },
            updated_unix_ms: unix_seconds(),
        })
        .map_err(|error| format!("write worktree harness: {error}"))
}

pub(crate) fn archive_worktree_session(repo: &Repository, session: &Session) -> Result<(), String> {
    let transaction = crate::flight_recorder::TransactionTrace::begin("session.archive");
    let archived_unix_ms = unix_seconds();
    let repo_root = repo.root.display().to_string();
    session_store(repo)?
        .archive(&crate::persistence::session::ArchiveInput {
            branch: &session.branch,
            repo_root: &repo_root,
            worktree_path: &session.path_display,
            archived_unix_ms,
            classification: session.classification.label(),
        })
        .map_err(|error| format!("archive worktree session: {error}"))?;
    transaction.committed();
    Ok(())
}

fn unarchive_worktree_session(repo: &Repository, branch: &str) -> Result<(), String> {
    let transaction = crate::flight_recorder::TransactionTrace::begin("session.unarchive");
    session_store(repo)?
        .unarchive(branch)
        .map_err(|error| format!("unarchive worktree session: {error}"))?;
    transaction.committed();
    Ok(())
}

pub(crate) fn list_archived_worktrees(repo: &Repository) -> Result<Vec<ArchivedWorktree>, String> {
    session_store(repo)?
        .list_archived()
        .map(|rows| {
            rows.into_iter()
                .map(|row| ArchivedWorktree {
                    branch: row.branch,
                    worktree_path: row.worktree_path,
                    classification: SessionClassification::parse(&row.classification),
                })
                .collect()
        })
        .map_err(|error| format!("read archived worktrees: {error}"))
}

fn hidden_session_exists(repo: &Repository, branch: &str) -> Result<bool, String> {
    session_store(repo)?
        .hidden_exists(branch)
        .map_err(|error| format!("read hidden marker: {error}"))
}

pub(crate) fn save_agent_state(
    repo: &Repository,
    branch: &str,
    state: AgentState,
) -> Result<(), String> {
    session_store(repo)?
        .save_agent_state(branch, state.label(), unix_seconds())
        .map_err(|error| format!("write process state: {error}"))
}

fn load_agent_state(repo: &Repository, branch: &str) -> Option<AgentState> {
    let state = session_store(repo).ok()?.load_agent_state(branch).ok()??;
    AgentState::parse(&state)
}

struct TaskMetadata {
    prompt_summary: String,
    classification: SessionClassification,
    visibility: i16,
}

fn load_task_metadata(repo: &Repository, branch: &str) -> Result<Option<TaskMetadata>, String> {
    session_store(repo)?
        .load_task_metadata(branch)
        .map(|row| {
            row.map(|row| TaskMetadata {
                prompt_summary: row.prompt_summary,
                classification: SessionClassification::parse(&row.classification),
                visibility: i16::try_from(row.visibility).unwrap_or_default(),
            })
        })
        .map_err(|error| format!("read task metadata: {error}"))
}

#[cfg(test)]
pub(crate) fn load_task_initial_prompt(
    repo: &Repository,
    branch: &str,
) -> Result<Option<String>, String> {
    session_store(repo)?
        .load_initial_prompt(branch)
        .map_err(|error| format!("read task initial prompt: {error}"))
}

fn load_hidden_sessions(repo: &Repository) -> Result<BTreeMap<String, i64>, String> {
    session_store(repo)?
        .hidden_sessions()
        .map(|rows| rows.into_iter().collect())
        .map_err(|error| format!("read hidden sessions: {error}"))
}

fn session_store(repo: &Repository) -> Result<crate::persistence::session::SessionStore, String> {
    crate::persistence::session::SessionStore::open(&observability::db_path(repo))
        .map_err(|error| format!("open session persistence: {error}"))
}

fn read_prompt_summary(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    for key in ["prompt_summary", "summary", "initial_prompt", "prompt"] {
        if let Some(value) = json_string_field(&text, key) {
            return Some(truncate(&value.replace('\n', " "), 50));
        }
    }
    None
}

fn prompt_summary_from_text(text: &str) -> String {
    truncate(&text.replace('\n', " "), 50)
}

fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::persistence::database::TestDatabase;
    use crate::sqlx_test_params as params;
    #[cfg(unix)]
    use crate::test_support::write_executable;

    use std::collections::BTreeMap;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[tokio::test(flavor = "multi_thread")]
    async fn owned_state_cleanup_rolls_back_all_branch_rows_on_late_failure() {
        let temp = unique_temp_dir("prism-session-atomic-cleanup-test");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        let path = temp.join("worktree");
        let branch = "feature/replaced";
        #[cfg(unix)]
        let tmux = {
            let tmux = temp.join("tmux");
            write_executable(&tmux, "#!/bin/sh\nexit 0\n");
            tmux
        };
        #[cfg(windows)]
        let tmux = {
            let tmux = temp.join("tmux.cmd");
            fs::write(&tmux, "@exit /b 0\r\n").unwrap();
            tmux
        };
        let mut config = test_config();
        config
            .tools
            .insert("tmux".to_string(), tmux.display().to_string());
        with_test_database(&repo, |conn| {
            conn.execute(
                "insert into task_metadata (
                    branch, prompt_summary, initial_prompt, worktree, updated_unix_ms
                 ) values (?1, '', '', ?2, 0)",
                params![branch, path.display().to_string()],
            )
            .map_err(|error| error.to_string())?;
            conn.execute(
                "insert into agent_state (branch, state, updated_unix_ms)
                 values (?1, 'running', 0)",
                params![branch],
            )
            .map_err(|error| error.to_string())?;
            conn.execute(
                "insert into pr_cache (
                    branch, number, provider, canonical_host, project_path, native_cr_id,
                    display_number, source_provider, source_canonical_host, source_project_path,
                    target_provider, target_canonical_host, target_project_path,
                    title, url, state, review_decision, head_ref, base_ref,
                    head_sha, updated_at, check_status, merged, draft, last_refreshed,
                    refreshed_unix_ms
                 ) values (?1, 42, 'github', 'github.com', 'org/repo', '42', 42,
                    'github', 'github.com', 'org/repo', 'github', 'github.com', 'org/repo',
                    '', '', 'OPEN', '', ?1, 'main', 'head', '', '', 0, 0, '', 0)",
                params![branch],
            )
            .map_err(|error| error.to_string())?;
            conn.execute(
                "insert into archived_worktree (
                    branch, repo_root, worktree_path, archived_unix_ms, classification
                 ) values (?1, ?2, ?3, 0, 'work')",
                params![
                    branch,
                    repo.root.display().to_string(),
                    path.display().to_string()
                ],
            )
            .map_err(|error| error.to_string())?;
            conn.execute_batch(
                "create trigger fail_archived_worktree_delete
                 before delete on archived_worktree
                 begin
                   select raise(abort, 'injected archived worktree delete failure');
                 end;",
            )
            .map_err(|error| error.to_string())?;
            Ok(())
        })
        .unwrap();

        let error = remove_worktree_owned_state(&repo, &config, &path, branch)
            .await
            .unwrap_err();

        assert!(error.contains("injected archived worktree delete failure"));
        with_test_database(&repo, |conn| {
            for table in ["task_metadata", "agent_state", "pr_cache"] {
                let count = conn
                    .query_row(
                        &format!("select count(*) from {table} where branch = ?1"),
                        params![branch],
                        |row| row.get::<_, i64>(0),
                    )
                    .map_err(|error| error.to_string())?;
                assert_eq!(count, 1, "cleanup partially removed {table}");
            }
            Ok(())
        })
        .unwrap();
        let _ = fs::remove_dir_all(temp);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn discover_sessions_skips_missing_worktree_paths() {
        let temp = unique_temp_dir("prism-session-missing-worktree-test");
        let repo_path = temp.join("repo");
        let missing = temp.join("missing");
        fs::create_dir_all(&repo_path).unwrap();
        let git = temp.join("git");
        fs::write(
            &git,
            format!(
                r###"#!/bin/sh
case "$*" in
  *"worktree list --porcelain"*)
    cat <<'EOF'
worktree {}
HEAD abc
branch refs/heads/main

worktree {}
HEAD def
branch refs/heads/feat/missing

EOF
    exit 0
    ;;
  *"status --short --branch"*)
    echo "## main"
    exit 0
    ;;
esac
exit 0
"###,
                repo_path.display(),
                missing.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&git).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&git, permissions).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));

        let mut config = test_config();
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        let repo = Repository::with_config_dir_for_test(repo_path.clone(), temp.join("config"));

        let sessions = discover_sessions(&repo, &config).await.unwrap();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].path, repo_path);
        assert_eq!(sessions[0].branch, "main");

        let _ = fs::remove_dir_all(temp);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn reconcile_worktree_state_removes_only_stale_persisted_sessions() {
        let temp = unique_temp_dir("prism-session-reconcile-test");
        let repo_path = temp.join("repo");
        let live_path = temp.join("live");
        let stale_path = temp.join("stale");
        let archived_path = temp.join("archived");
        fs::create_dir_all(&repo_path).unwrap();
        fs::create_dir_all(&live_path).unwrap();
        let git = temp.join("git");
        fs::write(
            &git,
            format!(
                "#!/bin/sh\ncase \"$*\" in\n  *\"worktree list --porcelain\"*) printf 'worktree {}\\nHEAD abc\\nbranch refs/heads/live\\n\\n' ;;\nesac\nexit 0\n",
                live_path.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&git).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&git, permissions).unwrap();
        let mut config = test_config();
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        crate::test_support::install_tool(&mut config, &temp, "tmux", "#!/bin/sh\nexit 0\n");
        let repo = Repository::with_config_dir_for_test(repo_path, temp.join("config"));
        with_test_database(&repo, |conn| {
            for (branch, path) in [
                ("live", &live_path),
                ("stale", &stale_path),
                ("archived", &archived_path),
            ] {
                conn.execute(
                    "insert into task_metadata (
                        branch, prompt_summary, initial_prompt, worktree, classification, visibility, updated_unix_ms
                     ) values (?1, '', '', ?2, 'work', 0, 0)",
                    params![branch, path.display().to_string()],
                )
                .map_err(|error| error.to_string())?;
            }
            conn.execute(
                "insert into archived_worktree (
                    branch, repo_root, worktree_path, archived_unix_ms, classification
                 ) values ('archived', ?1, ?2, 0, 'work')",
                params![repo.root.display().to_string(), archived_path.display().to_string()],
            )
            .map_err(|error| error.to_string())?;
            Ok(())
        })
        .unwrap();

        reconcile_worktree_state(&repo, &config).await.unwrap();

        with_test_database(&repo, |conn| {
            let live: i64 = conn
                .query_row(
                    "select count(*) from task_metadata where branch = 'live'",
                    [],
                    |row| row.get(0),
                )
                .map_err(|error| error.to_string())?;
            let stale: i64 = conn
                .query_row(
                    "select count(*) from task_metadata where branch = 'stale'",
                    [],
                    |row| row.get(0),
                )
                .map_err(|error| error.to_string())?;
            let archived_task: i64 = conn
                .query_row(
                    "select count(*) from task_metadata where branch = 'archived'",
                    [],
                    |row| row.get(0),
                )
                .map_err(|error| error.to_string())?;
            let archived_worktree: i64 = conn
                .query_row(
                    "select count(*) from archived_worktree where branch = 'archived'",
                    [],
                    |row| row.get(0),
                )
                .map_err(|error| error.to_string())?;
            assert_eq!(live, 1);
            assert_eq!(stale, 0);
            assert_eq!(archived_task, 1);
            assert_eq!(archived_worktree, 1);
            Ok(())
        })
        .unwrap();

        let _ = fs::remove_dir_all(temp);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn reconcile_external_branch_rename_removes_only_old_adopted_state() {
        let temp = unique_temp_dir("prism-reconcile-external-rename-test");
        let worktree = temp.join("worktree");
        fs::create_dir_all(&worktree).unwrap();
        let git = temp.join("git");
        write_executable(
            &git,
            &format!(
                "#!/bin/sh\ncase \"$*\" in\n  *\"worktree list --porcelain\"*) printf 'worktree {}\\nHEAD abc\\nbranch refs/heads/new-name\\n\\n' ;;\nesac\nexit 0\n",
                worktree.display()
            ),
        );
        let tmux = temp.join("tmux");
        write_executable(&tmux, "#!/bin/sh\nexit 0\n");
        let mut config = test_config();
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        config
            .tools
            .insert("tmux".to_string(), tmux.display().to_string());
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        with_test_database(&repo, |conn| {
            conn.execute(
                "insert into task_metadata (
                    branch, prompt_summary, initial_prompt, worktree, updated_unix_ms
                 ) values ('old-name', '', '', ?1, 0)",
                params![worktree.display().to_string()],
            )
            .map_err(|error| error.to_string())?;
            conn.execute(
                "insert into agent_state (branch, state, updated_unix_ms)
                 values ('old-name', 'running', 0)",
                [],
            )
            .map_err(|error| error.to_string())?;
            for branch in ["old-name", "new-name"] {
                conn.execute(
                    "insert into opencode_runtime (
                        repo_root, branch, worktree_path, server_port, server_url,
                        generation, updated_unix_ms
                     ) values (?1, ?2, ?3, 41000, 'http://127.0.0.1:41000', 1, 0)",
                    params![
                        repo.root.display().to_string(),
                        branch,
                        worktree.display().to_string()
                    ],
                )
                .map_err(|error| error.to_string())?;
            }
            Ok(())
        })
        .unwrap();

        reconcile_worktree_state(&repo, &config).await.unwrap();

        assert_eq!(count_rows(&repo, "task_metadata", "old-name"), 0);
        assert_eq!(count_rows(&repo, "agent_state", "old-name"), 0);
        assert_eq!(count_rows(&repo, "opencode_runtime", "old-name"), 0);
        assert_eq!(count_rows(&repo, "opencode_runtime", "new-name"), 1);
        let _ = fs::remove_dir_all(temp);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn reconcile_removes_stale_non_adopted_agent_and_runtime_state() {
        let temp = unique_temp_dir("prism-reconcile-non-adopted-test");
        let live = temp.join("live");
        let stale = temp.join("stale");
        fs::create_dir_all(&live).unwrap();
        let git = temp.join("git");
        write_executable(
            &git,
            &format!(
                "#!/bin/sh\ncase \"$*\" in\n  *\"worktree list --porcelain\"*) printf 'worktree {}\\nHEAD abc\\nbranch refs/heads/live\\n\\n' ;;\nesac\nexit 0\n",
                live.display()
            ),
        );
        let tmux = temp.join("tmux");
        write_executable(&tmux, "#!/bin/sh\nexit 0\n");
        let mut config = test_config();
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        config
            .tools
            .insert("tmux".to_string(), tmux.display().to_string());
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        with_test_database(&repo, |conn| {
            conn.execute(
                "insert into agent_state (branch, state, updated_unix_ms)
                 values ('stale', 'running', 0)",
                [],
            )
            .map_err(|error| error.to_string())?;
            conn.execute(
                "insert into opencode_runtime (
                    repo_root, branch, worktree_path, server_port, server_url,
                    generation, updated_unix_ms
                 ) values (?1, 'stale', ?2, 41000, 'http://127.0.0.1:41000', 1, 0)",
                params![repo.root.display().to_string(), stale.display().to_string()],
            )
            .map_err(|error| error.to_string())?;
            Ok(())
        })
        .unwrap();

        reconcile_worktree_state(&repo, &config).await.unwrap();

        assert_eq!(count_rows(&repo, "agent_state", "stale"), 0);
        assert_eq!(count_rows(&repo, "opencode_runtime", "stale"), 0);
        let _ = fs::remove_dir_all(temp);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn reconcile_moved_adopted_branch_keeps_branch_state_and_retires_old_path_runtime() {
        let temp = unique_temp_dir("prism-reconcile-moved-adopted-test");
        let old_path = temp.join("old");
        let new_path = temp.join("new");
        fs::create_dir_all(&new_path).unwrap();
        let git = temp.join("git");
        write_executable(
            &git,
            &format!(
                "#!/bin/sh\ncase \"$*\" in\n  *\"worktree list --porcelain\"*) printf 'worktree {}\\nHEAD abc\\nbranch refs/heads/feature\\n\\n' ;;\nesac\nexit 0\n",
                new_path.display()
            ),
        );
        let tmux_log = temp.join("tmux.log");
        let tmux = temp.join("tmux");
        write_executable(
            &tmux,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit 0\n",
                tmux_log.display()
            ),
        );
        let mut config = test_config();
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        config
            .tools
            .insert("tmux".to_string(), tmux.display().to_string());
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        with_test_database(&repo, |conn| {
            conn.execute(
                "insert into task_metadata (
                    branch, prompt_summary, initial_prompt, worktree, updated_unix_ms
                 ) values ('feature', 'summary', 'prompt', ?1, 0)",
                params![old_path.display().to_string()],
            )
            .map_err(|error| error.to_string())?;
            conn.execute(
                "insert into agent_state (branch, state, updated_unix_ms)
                 values ('feature', 'running', 0)",
                [],
            )
            .map_err(|error| error.to_string())?;
            for path in [&old_path, &new_path] {
                conn.execute(
                    "insert into opencode_runtime (
                        repo_root, branch, worktree_path, server_port, server_url,
                        generation, updated_unix_ms
                     ) values (?1, 'feature', ?2, 41000, 'http://127.0.0.1:41000', 1, 0)",
                    params![repo.root.display().to_string(), path.display().to_string()],
                )
                .map_err(|error| error.to_string())?;
            }
            Ok(())
        })
        .unwrap();

        reconcile_worktree_state(&repo, &config).await.unwrap();

        let (metadata_path, old_runtime, new_runtime, agent_state) =
            with_test_database(&repo, |conn| {
                let metadata_path = conn
                    .query_row(
                        "select worktree from task_metadata where branch = 'feature'",
                        [],
                        |row| row.get::<_, String>(0),
                    )
                    .map_err(|error| error.to_string())?;
                let runtime_count = |path: &Path| {
                    conn.query_row(
                        "select count(*) from opencode_runtime
                          where branch = 'feature' and worktree_path = ?1",
                        params![path.display().to_string()],
                        |row| row.get::<_, i64>(0),
                    )
                    .map_err(|error| error.to_string())
                };
                Ok((
                    metadata_path,
                    runtime_count(&old_path)?,
                    runtime_count(&new_path)?,
                    count_rows_with_conn(conn, "agent_state", "feature")?,
                ))
            })
            .unwrap();
        assert_eq!(metadata_path, new_path.display().to_string());
        assert_eq!(old_runtime, 0);
        assert_eq!(new_runtime, 1);
        assert_eq!(agent_state, 1);
        assert!(
            !tmux_log.exists(),
            "moved-path cleanup shut down the live branch"
        );
        let _ = fs::remove_dir_all(temp);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn reconcile_moved_non_adopted_branch_keeps_branch_only_retry_state() {
        let temp = unique_temp_dir("prism-reconcile-moved-non-adopted-test");
        let old_path = temp.join("old");
        let new_path = temp.join("new");
        fs::create_dir_all(&new_path).unwrap();
        let git = temp.join("git");
        write_executable(
            &git,
            &format!(
                "#!/bin/sh\ncase \"$*\" in\n  *\"worktree list --porcelain\"*) printf 'worktree {}\\nHEAD abc\\nbranch refs/heads/feature\\n\\n' ;;\nesac\nexit 0\n",
                new_path.display()
            ),
        );
        let tmux_log = temp.join("tmux.log");
        let tmux = temp.join("tmux");
        write_executable(
            &tmux,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit 0\n",
                tmux_log.display()
            ),
        );
        let mut config = test_config();
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        config
            .tools
            .insert("tmux".to_string(), tmux.display().to_string());
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        with_test_database(&repo, |conn| {
            conn.execute(
                "insert into agent_state (branch, state, updated_unix_ms)
                 values ('feature', 'running', 0)",
                [],
            )
            .map_err(|error| error.to_string())?;
            for path in [&old_path, &new_path] {
                conn.execute(
                    "insert into opencode_runtime (
                        repo_root, branch, worktree_path, server_port, server_url,
                        generation, updated_unix_ms
                     ) values (?1, 'feature', ?2, 41000, 'http://127.0.0.1:41000', 1, 0)",
                    params![repo.root.display().to_string(), path.display().to_string()],
                )
                .map_err(|error| error.to_string())?;
            }
            Ok(())
        })
        .unwrap();

        reconcile_worktree_state(&repo, &config).await.unwrap();

        assert_eq!(count_rows(&repo, "agent_state", "feature"), 1);
        assert_eq!(count_rows(&repo, "opencode_runtime", "feature"), 1);
        assert!(
            !tmux_log.exists(),
            "moved-path cleanup shut down the live branch"
        );
        let _ = fs::remove_dir_all(temp);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn cleanup_shutdown_failure_keeps_rows_for_successful_retry() {
        let temp = unique_temp_dir("prism-cleanup-shutdown-retry-test");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        let path = temp.join("worktree");
        let branch = "feature/retry";
        let fail_marker = temp.join("fail-kill");
        fs::write(&fail_marker, "fail\n").unwrap();
        let runtime = crate::tmux::TmuxAgentSession::for_worktree_session(&repo, branch, 1);
        let tmux = temp.join("tmux");
        write_executable(
            &tmux,
            &format!(
                "#!/bin/sh\ncase \"$1\" in\n  list-sessions) printf '{}\\n'; exit 0 ;;\n  kill-session) test ! -e '{}'; exit $? ;;\nesac\nexit 0\n",
                runtime.name(),
                fail_marker.display()
            ),
        );
        let mut config = test_config();
        config
            .tools
            .insert("tmux".to_string(), tmux.display().to_string());
        let git = temp.join("git");
        write_executable(
            &git,
            "#!/bin/sh\ncase \"$*\" in\n  *\"rev-parse --verify refs/heads/feature/retry\"*) printf 'branch-oid\\n' ;;\nesac\nexit 0\n",
        );
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        let wt = temp.join("wt");
        let wt_called = temp.join("wt-called");
        write_executable(
            &wt,
            &format!(
                "#!/bin/sh\ntest ! -e '{}' || exit 99\ntouch '{}'\nprintf '%s' '[{{\"branch\":\"{branch}\",\"branch_deleted\":false,\"kind\":\"worktree\",\"path\":\"{}\"}}]'\n",
                wt_called.display(),
                wt_called.display(),
                path.display(),
            ),
        );
        config
            .tools
            .insert("wt".to_string(), wt.display().to_string());
        with_test_database(&repo, |conn| {
            conn.execute(
                "insert into task_metadata (
                    branch, prompt_summary, initial_prompt, worktree, updated_unix_ms
                 ) values (?1, '', '', ?2, 0)",
                params![branch, path.display().to_string()],
            )
            .map_err(|error| error.to_string())?;
            conn.execute(
                "insert into agent_state (branch, state, updated_unix_ms)
                 values (?1, 'running', 0)",
                params![branch],
            )
            .map_err(|error| error.to_string())?;
            conn.execute(
                "insert into opencode_runtime (
                    repo_root, branch, worktree_path, server_port, server_url,
                    generation, updated_unix_ms
                 ) values (?1, ?2, ?3, 41000, 'http://127.0.0.1:41000', 1, 0)",
                params![
                    repo.root.display().to_string(),
                    branch,
                    path.display().to_string()
                ],
            )
            .map_err(|error| error.to_string())?;
            Ok(())
        })
        .unwrap();

        let outcome = delete_worktree_session_if_current(&repo, &config, &path, branch, None)
            .await
            .unwrap();

        assert!(matches!(
            outcome,
            DeleteWorktreeOutcome::DeletedWithWarnings { ref errors, .. } if !errors.is_empty()
        ));
        for table in ["task_metadata", "agent_state", "opencode_runtime"] {
            assert_eq!(
                count_rows(&repo, table, branch),
                1,
                "lost retry row in {table}"
            );
        }

        fs::remove_file(&fail_marker).unwrap();
        let retried = delete_worktree_session_if_current(&repo, &config, &path, branch, None)
            .await
            .unwrap();
        assert_eq!(retried, DeleteWorktreeOutcome::Deleted);

        for table in ["task_metadata", "agent_state", "opencode_runtime"] {
            assert_eq!(
                count_rows(&repo, table, branch),
                0,
                "retry retained row in {table}"
            );
        }
        let _ = fs::remove_dir_all(temp);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn recreated_same_path_and_branch_after_git_removal_retains_resources_and_state() {
        let temp = unique_temp_dir("prism-delete-recreated-after-remove-test");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        let path = temp.join("worktree");
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join(".git"), "old git link\n").unwrap();
        let old_incarnation = worktree_incarnation(&path);
        let branch = "feature/recreated";
        let git = temp.join("git");
        write_executable(
            &git,
            &format!(
                "#!/bin/sh\ncase \"$*\" in\n  *\"rev-parse --verify refs/heads/{branch}\"*) printf 'branch-oid\\n'; exit 0 ;;\n  *\"worktree list --porcelain\"*) printf 'worktree {}\\nHEAD branch-oid\\nbranch refs/heads/{branch}\\n\\n'; exit 0 ;;\nesac\nexit 0\n",
                path.display()
            ),
        );
        let wt = temp.join("wt");
        write_executable(
            &wt,
            &format!(
                "#!/bin/sh\nprintf 'new git link\\n' > '{}/.git'\nprintf '%s' '[{{\"branch\":\"{branch}\",\"branch_deleted\":false,\"kind\":\"worktree\",\"path\":\"{}\"}}]'\n",
                path.display(),
                path.display()
            ),
        );
        let tmux_log = temp.join("tmux.log");
        let tmux = temp.join("tmux");
        write_executable(
            &tmux,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit 0\n",
                tmux_log.display()
            ),
        );
        let mut config = test_config();
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        config
            .tools
            .insert("tmux".to_string(), tmux.display().to_string());
        config
            .tools
            .insert("wt".to_string(), wt.display().to_string());
        with_test_database(&repo, |conn| {
            conn.execute(
                "insert into task_metadata (
                    branch, prompt_summary, initial_prompt, worktree, updated_unix_ms
                 ) values (?1, '', '', ?2, 0)",
                params![branch, path.display().to_string()],
            )
            .map_err(|error| error.to_string())?;
            conn.execute(
                "insert into agent_state (branch, state, updated_unix_ms)
                 values (?1, 'running', 0)",
                params![branch],
            )
            .map_err(|error| error.to_string())?;
            Ok(())
        })
        .unwrap();

        let error = delete_worktree_session_if_current(
            &repo,
            &config,
            &path,
            branch,
            Some(&old_incarnation),
        )
        .await
        .unwrap_err();

        assert!(error.contains("recreated"));
        assert_eq!(count_rows(&repo, "task_metadata", branch), 1);
        assert_eq!(count_rows(&repo, "agent_state", branch), 1);
        assert!(!tmux_log.exists(), "replacement resources were shut down");
        let _ = fs::remove_dir_all(temp);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn worktree_incarnation_ignores_git_directory_activity_but_detects_replacement() {
        let temp = unique_temp_dir("prism-worktree-directory-incarnation-test");
        let worktree = temp.join("worktree");
        let git_dir = worktree.join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        let original = worktree_incarnation(&worktree);

        fs::write(git_dir.join("FETCH_HEAD"), "refreshed\n").unwrap();
        assert_eq!(worktree_incarnation(&worktree), original);

        let replacement = worktree.join("replacement.git");
        fs::create_dir(&replacement).unwrap();
        fs::rename(&git_dir, worktree.join("old.git")).unwrap();
        fs::rename(replacement, &git_dir).unwrap();
        assert_ne!(worktree_incarnation(&worktree), original);

        let _ = fs::remove_dir_all(temp);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn worktree_session_default_branch_sorts_first() {
        let mut config = test_config();
        config.default_base = Some("main".to_string());
        let main = test_session("main", "/repo/main");
        let feature = test_session("feature", "/repo/feature");

        assert_eq!(
            session_discovery_order(&config, &main, &feature),
            std::cmp::Ordering::Less
        );
        assert!(main.is_default_branch(&config));
        assert!(feature.is_task_branch(&config));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn planning_and_exploration_sessions_sort_below_work_sessions() {
        let config = test_config();
        let work = test_session("feature-a", "/repo/a");
        let mut planning = test_session("feature-b", "/repo/b");
        planning.classification = SessionClassification::Planning;
        let mut exploration = test_session("feature-c", "/repo/c");
        exploration.classification = SessionClassification::Exploration;

        assert_eq!(
            session_discovery_order(&config, &work, &planning),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            session_discovery_order(&config, &planning, &exploration),
            std::cmp::Ordering::Less
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn hidden_sessions_sort_below_focused_sessions() {
        let config = test_config();
        let focused = test_session("feature-a", "/repo/a");
        let mut hidden = test_session("feature-b", "/repo/b");
        hidden.hidden = true;

        assert_eq!(
            session_discovery_order(&config, &focused, &hidden),
            std::cmp::Ordering::Less
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn archived_worktree_metadata_records_restore_details_and_hides_session() {
        let temp = unique_temp_dir("prism-archive-worktree-test");
        let repo_path = temp.join("repo");
        let worktree = temp.join("worktree");
        fs::create_dir_all(&repo_path).unwrap();
        fs::create_dir_all(&worktree).unwrap();
        let repo = Repository::with_config_dir_for_test(repo_path.clone(), temp.join("config"));
        let mut session = test_session("feature", &worktree.display().to_string());
        session.classification = SessionClassification::Planning;

        archive_worktree_session(&repo, &session).unwrap();

        let row = with_test_database(&repo, |conn| {
            conn.query_row(
                "select repo_root, worktree_path, classification from archived_worktree where branch = ?1",
                params!["feature"],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?)),
            )
            .map_err(|error| format!("read archived metadata: {error}"))
        })
        .unwrap();

        assert_eq!(row.0, repo_path.display().to_string());
        assert_eq!(row.1, worktree.display().to_string());
        assert_eq!(row.2, "planning");
        assert!(load_hidden_sessions(&repo).unwrap().contains_key("feature"));
        let archived = list_archived_worktrees(&repo).unwrap();
        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0].branch, "feature");
        assert_eq!(archived[0].worktree_path, worktree.display().to_string());
        assert_eq!(archived[0].classification, SessionClassification::Planning);

        let _ = fs::remove_dir_all(temp);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn worktree_harness_binding_isolated_by_incarnation_and_can_be_pinned() {
        let temp = unique_temp_dir("prism-worktree-harness-test");
        let repo_path = temp.join("repo");
        let worktree = temp.join("worktree");
        fs::create_dir_all(&repo_path).unwrap();
        fs::create_dir_all(&worktree).unwrap();
        let repo = Repository::with_config_dir_for_test(repo_path, temp.join("config"));
        let mut session = test_session("feature", &worktree.display().to_string());

        assert_eq!(
            worktree_harness(&repo, &session).unwrap().harness_id,
            "opencode"
        );
        set_worktree_harness(&repo, &session, "codex", true).unwrap();
        assert_eq!(
            worktree_harness(&repo, &session).unwrap(),
            WorktreeHarnessAssociation {
                harness_id: "codex".to_string(),
                keep: true,
            }
        );

        session.incarnation = "replacement".to_string();
        assert_eq!(
            worktree_harness(&repo, &session).unwrap(),
            WorktreeHarnessAssociation {
                harness_id: "opencode".to_string(),
                keep: false,
            }
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unarchive_worktree_session_clears_hidden_and_archived_markers() {
        let temp = unique_temp_dir("prism-unarchive-worktree-test");
        let repo_path = temp.join("repo");
        let worktree = temp.join("worktree");
        fs::create_dir_all(&repo_path).unwrap();
        fs::create_dir_all(&worktree).unwrap();
        let repo = Repository::with_config_dir_for_test(repo_path, temp.join("config"));
        let session = test_session("feature", &worktree.display().to_string());
        archive_worktree_session(&repo, &session).unwrap();

        unarchive_worktree_session(&repo, "feature").unwrap();

        assert!(list_archived_worktrees(&repo).unwrap().is_empty());
        assert!(!load_hidden_sessions(&repo).unwrap().contains_key("feature"));

        let _ = fs::remove_dir_all(temp);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn archive_failure_does_not_leave_visible_session_archived() {
        let temp = unique_temp_dir("prism-archive-atomic-failure-test");
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        let session = test_session("feature", "/repo/feature");
        with_test_database(&repo, |conn| {
            conn.execute_batch(
                "create trigger reject_archive before insert on archived_worktree
                 begin select raise(abort, 'archive rejected'); end;",
            )
            .map_err(|error| error.to_string())
        })
        .unwrap();

        let error = archive_worktree_session(&repo, &session).unwrap_err();

        assert!(error.contains("archive rejected"));
        assert!(!hidden_session_exists(&repo, "feature").unwrap());
        assert!(list_archived_worktrees(&repo).unwrap().is_empty());
        let _ = fs::remove_dir_all(temp);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unarchive_failure_keeps_hidden_and_archived_state_coherent() {
        let temp = unique_temp_dir("prism-unarchive-atomic-failure-test");
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        let session = test_session("feature", "/repo/feature");
        archive_worktree_session(&repo, &session).unwrap();
        with_test_database(&repo, |conn| {
            conn.execute_batch(
                "create trigger reject_unarchive before delete on archived_worktree
                 begin select raise(abort, 'unarchive rejected'); end;",
            )
            .map_err(|error| error.to_string())
        })
        .unwrap();

        let error = unarchive_worktree_session(&repo, "feature").unwrap_err();

        assert!(error.contains("unarchive rejected"));
        assert!(hidden_session_exists(&repo, "feature").unwrap());
        assert_eq!(list_archived_worktrees(&repo).unwrap().len(), 1);
        let _ = fs::remove_dir_all(temp);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn phase_1_same_path_changed_branch_does_not_inherit_agent_session_or_pr_cache_facts() {
        let temp = unique_temp_dir("prism-changed-branch-refresh-test");
        let worktree = temp.join("worktree");
        fs::create_dir_all(&worktree).unwrap();
        let git = temp.join("git");
        write_executable(
            &git,
            &format!(
                "#!/bin/sh\ncase \"$*\" in\n  *\"worktree list --porcelain\"*) printf 'worktree {}\\nHEAD new-head\\nbranch refs/heads/new-feature\\n\\n' ;;\n  *\"status --short --branch\"*) printf '## new-feature\\n' ;;\n  *\"remote get-url origin\"*) printf 'git@github.com:owner/repo.git\\n' ;;\nesac\n",
                worktree.display()
            ),
        );
        let mut config = test_config();
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        let repository = WorktreeRepositoryKey::new(repo.root.clone());
        let mut previous = test_session("old-feature", &worktree.display().to_string());
        previous.agent_state = AgentState::Running;
        previous.opencode_status = Some(OpencodeStatus::offline(
            Some("http://127.0.0.1:41000".to_string()),
            Some("old-session".to_string()),
        ));
        previous.pr = PrCache::stale_for_test(
            Some(crate::remote::PrDetails::default()),
            "old branch PR failure",
        );
        previous
            .wt_columns
            .insert("old".to_string(), "branch".to_string());
        previous.unseen_comments = true;
        let mut sessions = vec![previous];
        let repositories = [WorktreeSessionRepository {
            repo_index: 0,
            repo: &repo,
            config: &config,
            label: "repo",
            key: None,
            identity: &repository,
        }];

        refresh_worktree_sessions(
            &repositories,
            &BTreeMap::from([(0, repository.clone())]),
            &mut sessions,
        )
        .await
        .unwrap();

        assert_eq!(sessions.len(), 1);
        let refreshed = &sessions[0];
        assert_eq!(refreshed.path, worktree);
        assert_eq!(refreshed.branch, "new-feature");
        assert_eq!(refreshed.agent_state, AgentState::Idle);
        assert!(refreshed.opencode_status.is_none());
        assert!(refreshed.pr.summary().is_none());
        assert!(refreshed.pr.display_error().is_none());
        assert!(refreshed.pr.details().is_none());
        assert!(refreshed.wt_columns.is_empty());
        assert!(!refreshed.unseen_comments);

        let _ = fs::remove_dir_all(temp);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn recreated_worktree_at_same_path_and_branch_has_new_identity() {
        let repository = WorktreeRepositoryKey::new(PathBuf::from("/repo"));
        let mut previous = test_session("feature", "/repo/worktree");
        previous.incarnation = "old-git-link".to_string();
        let mut recreated = test_session("feature", "/repo/worktree");
        recreated.incarnation = "new-git-link".to_string();

        assert_ne!(
            previous.identity_key(&repository),
            recreated.identity_key(&repository)
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn rebase_detachment_refresh_preserves_branch_session() {
        let temp = unique_temp_dir("prism-rebase-session-refresh-test");
        let worktree = temp.join("worktree");
        fs::create_dir_all(&worktree).unwrap();
        let head_name = temp.join("head-name");
        fs::write(&head_name, "refs/heads/feature\n").unwrap();
        let git = temp.join("git");
        write_executable(
            &git,
            &format!(
                "#!/bin/sh\ncase \"$*\" in\n  *\"worktree list --porcelain\"*) printf 'worktree {}\\nHEAD abc\\ndetached\\n\\n' ;;\n  *\"rev-parse --git-path rebase-merge/head-name\"*) printf '{}\\n' ;;\n  *\"symbolic-ref --quiet HEAD\"*) exit 1 ;;\n  *\"status --short --branch\"*) printf '## HEAD (no branch)\\n' ;;\nesac\n",
                worktree.display(),
                head_name.display()
            ),
        );
        let mut config = test_config();
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        let repository = WorktreeRepositoryKey::new(repo.root.clone());
        let mut previous = test_session("feature", &worktree.display().to_string());
        previous.agent_state = AgentState::Running;
        let mut sessions = vec![previous];
        let repositories = [WorktreeSessionRepository {
            repo_index: 0,
            repo: &repo,
            config: &config,
            label: "repo",
            key: None,
            identity: &repository,
        }];

        refresh_worktree_sessions(
            &repositories,
            &BTreeMap::from([(0, repository.clone())]),
            &mut sessions,
        )
        .await
        .unwrap();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].branch, "feature");
        assert_eq!(sessions[0].agent_state, AgentState::Running);
        let _ = fs::remove_dir_all(temp);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn detached_session_discovery_refresh_preserves_matching_session() {
        let temp = unique_temp_dir("prism-detached-session-refresh-test");
        let worktree = temp.join("worktree");
        fs::create_dir_all(&worktree).unwrap();
        let git = temp.join("git");
        write_executable(
            &git,
            &format!(
                "#!/bin/sh\ncase \"$*\" in\n  *\"worktree list --porcelain\"*) printf 'worktree {}\\nHEAD abc\\ndetached\\n\\n' ;;\n  *\"status --short --branch\"*) printf '## HEAD (no branch)\\n' ;;\nesac\n",
                worktree.display()
            ),
        );
        let mut config = test_config();
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        let repository = WorktreeRepositoryKey::new(repo.root.clone());
        let mut previous = test_session("(detached)", &worktree.display().to_string());
        previous.agent_state = AgentState::Running;
        let mut sessions = vec![previous];
        let repositories = [WorktreeSessionRepository {
            repo_index: 0,
            repo: &repo,
            config: &config,
            label: "repo",
            key: None,
            identity: &repository,
        }];

        refresh_worktree_sessions(
            &repositories,
            &BTreeMap::from([(0, repository.clone())]),
            &mut sessions,
        )
        .await
        .unwrap();

        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].is_detached());
        assert_eq!(sessions[0].agent_state, AgentState::Running);
        assert!(sessions[0].pr.display_error().is_none());
        let _ = fs::remove_dir_all(temp);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn refresh_uses_repository_root_when_different_repositories_report_same_session_identity()
    {
        let temp = unique_temp_dir("prism-session-repository-identity-test");
        let shared_path = temp.join("shared-worktree");
        fs::create_dir_all(&shared_path).unwrap();
        let git = temp.join("git");
        write_executable(
            &git,
            &format!(
                "#!/bin/sh\ncase \"$*\" in\n  *\"worktree list --porcelain\"*) printf 'worktree {}\\nHEAD abc\\nbranch refs/heads/feature\\n\\n' ;;\n  *\"status --short --branch\"*) printf '## feature\\n' ;;\nesac\n",
                shared_path.display()
            ),
        );
        let mut config = test_config();
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        let repo_a =
            Repository::with_config_dir_for_test(temp.join("repo-a"), temp.join("config-a"));
        let repo_b =
            Repository::with_config_dir_for_test(temp.join("repo-b"), temp.join("config-b"));
        let identity_a = WorktreeRepositoryKey::new(repo_a.root.clone());
        let identity_b = WorktreeRepositoryKey::new(repo_b.root.clone());
        let mut a = test_session("feature", &shared_path.display().to_string());
        a.repo_index = 0;
        a.agent_state = AgentState::Running;
        let mut b = test_session("feature", &shared_path.display().to_string());
        b.repo_index = 1;
        b.agent_state = AgentState::NeedsInput;
        let mut sessions = vec![a, b];
        let repositories = [
            WorktreeSessionRepository {
                repo_index: 0,
                repo: &repo_b,
                config: &config,
                label: "b",
                key: None,
                identity: &identity_b,
            },
            WorktreeSessionRepository {
                repo_index: 1,
                repo: &repo_a,
                config: &config,
                label: "a",
                key: None,
                identity: &identity_a,
            },
        ];

        refresh_worktree_sessions(
            &repositories,
            &BTreeMap::from([(0, identity_a.clone()), (1, identity_b.clone())]),
            &mut sessions,
        )
        .await
        .unwrap();

        assert_eq!(sessions[0].repo_label, "b");
        assert_eq!(sessions[0].agent_state, AgentState::NeedsInput);
        assert_eq!(sessions[1].repo_label, "a");
        assert_eq!(sessions[1].agent_state, AgentState::Running);
        let _ = fs::remove_dir_all(temp);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn persistence_read_failure_preserves_previous_safe_session_facts() {
        let temp = unique_temp_dir("prism-session-metadata-read-failure-test");
        let worktree = temp.join("worktree");
        fs::create_dir_all(&worktree).unwrap();
        let git = temp.join("git");
        write_executable(
            &git,
            &format!(
                "#!/bin/sh\ncase \"$*\" in\n  *\"worktree list --porcelain\"*) printf 'worktree {}\\nHEAD abc\\nbranch refs/heads/feature\\n\\n' ;;\nesac\n",
                worktree.display()
            ),
        );
        let mut config = test_config();
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        let identity = WorktreeRepositoryKey::new(repo.root.clone());
        let db = observability::db_path(&repo);
        fs::create_dir_all(db.parent().unwrap()).unwrap();
        fs::create_dir_all(&db).unwrap();
        let mut previous = test_session("feature", &worktree.display().to_string());
        previous.adopted = true;
        previous.agent_state = AgentState::Running;
        let mut sessions = vec![previous];
        let repositories = [WorktreeSessionRepository {
            repo_index: 0,
            repo: &repo,
            config: &config,
            label: "repo",
            key: None,
            identity: &identity,
        }];

        assert!(
            refresh_worktree_sessions(
                &repositories,
                &BTreeMap::from([(0, identity.clone())]),
                &mut sessions,
            )
            .await
            .is_err()
        );
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].adopted);
        assert_eq!(sessions[0].agent_state, AgentState::Running);
        let _ = fs::remove_dir_all(temp);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn task_metadata_read_failure_does_not_replace_adopted_session_with_absence() {
        let temp = unique_temp_dir("prism-task-metadata-read-failure-test");
        let worktree = temp.join("worktree");
        fs::create_dir_all(&worktree).unwrap();
        let git = temp.join("git");
        write_executable(
            &git,
            &format!(
                "#!/bin/sh\ncase \"$*\" in\n  *\"worktree list --porcelain\"*) printf 'worktree {}\\nHEAD abc\\nbranch refs/heads/feature\\n\\n' ;;\nesac\n",
                worktree.display()
            ),
        );
        let mut config = test_config();
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        with_test_database(&repo, |conn| {
            conn.execute_batch(
                "drop table task_metadata; create table task_metadata (branch text primary key);",
            )
            .map_err(|error| error.to_string())
        })
        .unwrap();
        let identity = WorktreeRepositoryKey::new(repo.root.clone());
        let mut previous = test_session("feature", &worktree.display().to_string());
        previous.adopted = true;
        let mut sessions = vec![previous];
        let repositories = [WorktreeSessionRepository {
            repo_index: 0,
            repo: &repo,
            config: &config,
            label: "repo",
            key: None,
            identity: &identity,
        }];

        assert!(
            refresh_worktree_sessions(
                &repositories,
                &BTreeMap::from([(0, identity.clone())]),
                &mut sessions,
            )
            .await
            .is_err()
        );
        assert!(sessions[0].adopted);
        let _ = fs::remove_dir_all(temp);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn archived_worktree_read_failure_is_not_reported_as_an_empty_archive() {
        let temp = unique_temp_dir("prism-archive-read-failure-test");
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        with_test_database(&repo, |conn| {
            conn.execute_batch(
                "drop table archived_worktree; create table archived_worktree (branch text primary key);",
            )
            .map_err(|error| error.to_string())
        })
        .unwrap();

        let error = list_archived_worktrees(&repo).unwrap_err();

        assert!(error.contains("archived worktree"));
        let _ = fs::remove_dir_all(temp);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mark_adopted_with_prompt_updates_local_metadata_facts() {
        let mut session = test_session("feature", "/repo/feature");

        session.mark_adopted_with_prompt("first line\nsecond line with extra text");

        assert!(session.adopted);
        assert_eq!(
            session.prompt_summary,
            "first line second line with extra text"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn adoption_reports_partial_success_without_marking_session_adopted() {
        let temp = unique_temp_dir("prism-session-adoption-partial-test");
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        let db = observability::db_path(&repo);
        fs::create_dir_all(db.parent().unwrap()).unwrap();
        fs::create_dir_all(&db).unwrap();
        let mut session = test_session("feature", "/repo/worktree");
        session.adopted = false;

        let outcome = adopt_worktree_session(&repo, &mut session, "initial prompt");

        assert!(matches!(
            outcome,
            AdoptWorktreeOutcome::WorktreeCreatedMetadataFailed { .. }
        ));
        assert!(!session.adopted);
        let _ = fs::remove_dir_all(temp);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn creation_reports_partial_success_when_metadata_restoration_fails() {
        let temp = unique_temp_dir("prism-session-creation-partial-test");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        let db = observability::db_path(&repo);
        let wt = temp.join("wt");
        write_executable(
            &wt,
            &format!(
                "#!/bin/sh\nrm -f '{}'\nmkdir -p '{}'\nprintf '%s' '{{\"action\":\"created\",\"branch\":\"feature\",\"path\":\"/repo/worktree\",\"created_branch\":true}}'\n",
                db.display(),
                db.display()
            ),
        );
        let mut config = test_config();
        config
            .tools
            .insert("wt".to_string(), wt.display().to_string());
        let git = temp.join("git");
        write_executable(
            &git,
            "#!/bin/sh\nprintf 'worktree /repo/worktree\\nbranch refs/heads/feature\\n\\n'\n",
        );
        config
            .tools
            .insert("git".to_string(), git.display().to_string());

        let outcome = create_worktree_session(&repo, &config, "feature")
            .await
            .unwrap();

        assert!(matches!(
            outcome,
            CreateWorktreeOutcome::CreatedMetadataFailed { .. }
        ));
        let _ = fs::remove_dir_all(temp);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn deletion_warnings_describe_worktree_session_local_risks() {
        let mut session = test_session("(detached)", "/repo/detached");
        session.status_label = "dirty 1 ahead 2 behind 3".to_string();
        session.adopted = false;
        session.agent_state = AgentState::Running;

        let warnings = session.deletion_warnings();

        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("dirty worktree"))
        );
        assert!(warnings.iter().any(|warning| warning.contains("unpushed")));
        assert!(warnings.iter().any(|warning| warning.contains("behind")));
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("not created"))
        );
        assert!(warnings.iter().any(|warning| warning.contains("detached")));
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("agent is still running"))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn archive_warnings_describe_non_destructive_hiding() {
        let mut session = test_session("feature", "/repo/feature");
        session.status_label = "dirty 1 ahead 2".to_string();

        let warnings = session.archive_warnings();

        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("stay on disk"))
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("stay local"))
        );
        assert!(!warnings.iter().any(|warning| warning.contains("deleted")));
        assert!(!warnings.iter().any(|warning| warning.contains("lost")));
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn deferred_merge_cleanup_requires_the_approved_session_and_branch_facts() {
        let temp = unique_temp_dir("prism-deferred-merge-cleanup-test");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        let mut config = test_config();
        let git = crate::test_support::install_tool(
            &mut config,
            &temp,
            "git",
            "#!/bin/sh\ncase \"$*\" in\n  *rev-parse*) echo oid-one ;;\n  *status*) echo '## feature' ;;\nesac\n",
        );
        let mut session = test_session("feature", "/repo/feature");
        session.incarnation = "incarnation-one".to_string();

        let approved_warnings = deferred_merge_cleanup_warnings(&config, &session).await;
        schedule_deferred_merge_cleanup(&repo, &config, &session, &approved_warnings)
            .await
            .unwrap();
        assert_eq!(
            deferred_merge_cleanup_status(&repo, &config, &session)
                .await
                .unwrap(),
            DeferredMergeCleanupStatus::Safe
        );

        session.incarnation = "incarnation-two".to_string();
        assert!(matches!(
            deferred_merge_cleanup_status(&repo, &config, &session)
                .await
                .unwrap(),
            DeferredMergeCleanupStatus::Unsafe(reason) if reason.contains("identity changed")
        ));

        session.incarnation = "incarnation-one".to_string();
        write_executable(
            &git,
            "#!/bin/sh\ncase \"$*\" in\n  *rev-parse*) echo oid-two ;;\n  *status*) echo '## feature' ;;\nesac\n",
        );
        assert!(matches!(
            deferred_merge_cleanup_status(&repo, &config, &session)
                .await
                .unwrap(),
            DeferredMergeCleanupStatus::Unsafe(reason) if reason.contains("branch advanced")
        ));

        write_executable(
            &git,
            "#!/bin/sh\ncase \"$*\" in\n  *rev-parse*) echo oid-one ;;\n  *status*) printf '## feature\\n?? new-file\\n' ;;\nesac\n",
        );
        assert!(matches!(
            deferred_merge_cleanup_status(&repo, &config, &session)
                .await
                .unwrap(),
            DeferredMergeCleanupStatus::Unsafe(reason) if reason.contains("warning facts changed")
        ));

        assert!(cancel_deferred_merge_cleanup(&repo, "feature").unwrap());
        assert_eq!(
            deferred_merge_cleanup_status(&repo, &config, &session)
                .await
                .unwrap(),
            DeferredMergeCleanupStatus::NotScheduled
        );
        fs::remove_dir_all(temp).unwrap();
    }

    fn test_session(branch: &str, path: &str) -> Session {
        Session {
            repo_index: 0,
            repo_label: "repo".to_string(),
            repo_key: None,
            path: PathBuf::from(path),
            incarnation: String::new(),
            path_display: path.to_string(),
            branch: branch.to_string(),
            prompt_summary: String::new(),
            classification: SessionClassification::Work,
            visibility: 0,
            adopted: true,
            hidden: false,
            status_label: "clean".to_string(),
            agent_state: AgentState::Idle,
            opencode_status: None,
            pr: PrCache::default(),
            wt_columns: BTreeMap::new(),
            unseen_comments: false,
        }
    }

    fn test_config() -> Config {
        crate::test_support::test_config()
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }

    fn count_rows(repo: &Repository, table: &str, branch: &str) -> i64 {
        with_test_database(repo, |conn| count_rows_with_conn(conn, table, branch)).unwrap()
    }

    fn count_rows_with_conn(conn: &TestDatabase, table: &str, branch: &str) -> Result<i64, String> {
        conn.query_row(
            &format!("select count(*) from {table} where branch = ?1"),
            params![branch],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| error.to_string())
    }

    fn with_test_database<T>(
        repo: &Repository,
        run: impl FnOnce(&TestDatabase) -> Result<T, String>,
    ) -> Result<T, String> {
        observability::with_writable_db(repo, |path| run(&TestDatabase::open(path)?))
    }
}
