use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{OptionalExtension, params};

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
    pub(crate) worktree_session_id: String,
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
            worktree_session_id: self.worktree_session_id.clone(),
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
            worktree_session_id: self.worktree_session_id.clone(),
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
        let mut warnings = Vec::new();
        if status_count(&self.status_label, "dirty").is_some() {
            warnings.push("dirty worktree: uncommitted changes will be deleted".to_string());
        }
        if status_count(&self.status_label, "ahead").is_some() {
            warnings.push("branch is ahead of upstream: unpushed commits may be lost".to_string());
        }
        if status_count(&self.status_label, "behind").is_some() {
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
        if let Some(summary) = self.pr.summary()
            && !summary.merged
        {
            warnings.push(format!("open PR #{} still exists", summary.number));
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

pub(crate) fn create_worktree_session(
    repo: &Repository,
    config: &Config,
    branch: &str,
) -> Result<CreateWorktreeOutcome, CreateWorktreeFailure> {
    create_or_checkout_worktree_session(repo, config, branch, false)
}

pub(crate) fn checkout_worktree_session(
    repo: &Repository,
    config: &Config,
    branch: &str,
) -> Result<CreateWorktreeOutcome, CreateWorktreeFailure> {
    create_or_checkout_worktree_session(repo, config, branch, true)
}

fn create_or_checkout_worktree_session(
    repo: &Repository,
    config: &Config,
    branch: &str,
    checkout: bool,
) -> Result<CreateWorktreeOutcome, CreateWorktreeFailure> {
    if hidden_session_exists(repo, branch).map_err(CreateWorktreeFailure::Other)?
        && crate::lifecycle::branch_has_worktree(repo, config, branch)
            .map_err(CreateWorktreeFailure::Other)?
    {
        unarchive_worktree_session(repo, branch).map_err(CreateWorktreeFailure::Other)?;
        return Ok(CreateWorktreeOutcome::Restored);
    }
    let switch = if checkout {
        crate::lifecycle::checkout_worktree(repo, config, branch)
            .map_err(CreateWorktreeFailure::Worktrunk)?
    } else {
        crate::lifecycle::create_worktree(repo, config, branch)
            .map_err(CreateWorktreeFailure::Worktrunk)?
    };
    if let Err(error) = crate::lifecycle::verify_switch_outcome(repo, config, branch, &switch) {
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

fn load_pending_worktree_deletion(
    repo: &Repository,
    branch: &str,
) -> Result<Option<PendingWorktreeDeletion>, String> {
    observability::with_writable_db(repo, |conn| {
        conn.query_row(
            "select worktree_path, worktree_incarnation, branch_oid, worktree_removed, branch_deleted
             from pending_worktree_deletion where branch = ?1",
            params![branch],
            |row| {
                Ok(PendingWorktreeDeletion {
                    worktree_path: row.get(0)?,
                    worktree_incarnation: row.get(1)?,
                    branch_oid: row.get(2)?,
                    worktree_removed: row.get(3)?,
                    branch_deleted: row.get(4)?,
                })
            },
        )
        .optional()
        .map_err(|error| format!("load pending worktree deletion: {error}"))
    })
}

fn load_pending_worktree_deletions(
    repo: &Repository,
) -> Result<Vec<(String, PendingWorktreeDeletion)>, String> {
    observability::with_writable_db(repo, |conn| {
        let mut statement = conn
            .prepare(
                "select branch, worktree_path, worktree_incarnation, branch_oid,
                        worktree_removed, branch_deleted
                 from pending_worktree_deletion",
            )
            .map_err(|error| format!("prepare pending worktree deletions: {error}"))?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    PendingWorktreeDeletion {
                        worktree_path: row.get(1)?,
                        worktree_incarnation: row.get(2)?,
                        branch_oid: row.get(3)?,
                        worktree_removed: row.get(4)?,
                        branch_deleted: row.get(5)?,
                    },
                ))
            })
            .map_err(|error| format!("query pending worktree deletions: {error}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("read pending worktree deletions: {error}"))
    })
}

fn save_pending_worktree_deletion(
    repo: &Repository,
    path: &Path,
    branch: &str,
    incarnation: &str,
    branch_oid: Option<&str>,
) -> Result<(), String> {
    observability::with_writable_db(repo, |conn| {
        conn.execute(
            "insert into pending_worktree_deletion (
                branch, worktree_path, worktree_incarnation, branch_oid,
                worktree_removed, branch_deleted, updated_unix_ms
             ) values (?1, ?2, ?3, ?4, 0, 0, ?5)",
            params![
                branch,
                path.display().to_string(),
                incarnation,
                branch_oid,
                unix_seconds(),
            ],
        )
        .map_err(|error| format!("save pending worktree deletion: {error}"))?;
        Ok(())
    })
}

fn mark_pending_deletion_phase(
    repo: &Repository,
    branch: &str,
    column: &str,
) -> Result<(), String> {
    observability::with_writable_db(repo, |conn| {
        conn.execute(
            &format!(
                "update pending_worktree_deletion
                 set {column} = 1, updated_unix_ms = ?1 where branch = ?2"
            ),
            params![unix_seconds(), branch],
        )
        .map_err(|error| format!("record pending worktree deletion phase: {error}"))?;
        Ok(())
    })
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

pub(crate) fn delete_worktree_session_if_current(
    repo: &Repository,
    config: &Config,
    path: &Path,
    branch: &str,
    expected_incarnation: Option<&str>,
) -> Result<DeleteWorktreeOutcome, String> {
    let path_display = path.display().to_string();
    let worktree_session_id = observability::with_writable_db(repo, |conn| {
        conn.query_row(
            "select worktree_session_id from active_worktree_session
             where branch = ?1 and worktree_path = ?2",
            params![branch, path_display.as_str()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| format!("load deletion Worktree Session identity: {error}"))
    })?;
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
    let live_before_removal = crate::lifecycle::list_worktrees(repo, config)?;
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
        None => Some(crate::lifecycle::branch_oid(repo, config, branch)?),
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
        match crate::lifecycle::remove_worktree(repo, config, path) {
            Ok(removal) => (removal, None),
            Err(error) => {
                let live = crate::lifecycle::list_worktrees(repo, config)?;
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
    let live_after_removal = crate::lifecycle::list_worktrees(repo, config)?;
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
        && !crate::lifecycle::branch_exists(repo, config, branch)?
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
    let cleanup_error = remove_deleted_worktree_owned_state(
        repo,
        config,
        path,
        branch,
        worktree_session_id.as_deref(),
    )
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

fn shutdown_worktree_session_resources(
    repo: &Repository,
    config: &Config,
    branch: &str,
    worktree_session_id: Option<&str>,
    runtimes: &[crate::opencode::OpencodeRuntime],
) -> Result<(), String> {
    let mut errors = Vec::new();
    let shutdown = match worktree_session_id {
        Some(id) => crate::agent_session::shutdown_worktree_session(repo, config, branch, id),
        None => crate::agent_session::shutdown(repo, config, branch),
    };
    if let Err(error) = shutdown {
        errors.push(error);
    }
    if let Err(error) =
        crate::opencode::shutdown_worktree_session_runtime_processes_with_lock_held(repo, runtimes)
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
    pub worktree_session_id: String,
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

pub(crate) fn refresh_worktree_sessions(
    repositories: &[WorktreeSessionRepository<'_>],
    previous_repository_identities: &BTreeMap<usize, WorktreeRepositoryKey>,
    current: &mut Vec<Session>,
) -> Result<(), String> {
    let mut discovered_by_repository = Vec::new();
    for repository in repositories {
        discovered_by_repository.push(discover_sessions(repository.repo, repository.config)?);
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

pub(crate) fn discover_sessions(
    repo: &Repository,
    config: &Config,
) -> Result<Vec<Session>, String> {
    let inventory = crate::lifecycle::list_worktrees(repo, config)?;
    let hidden = load_hidden_sessions(repo)?;
    let mut sessions = Vec::new();

    for entry in inventory {
        if entry.path.exists() {
            let mut session = build_session(repo, entry.path, entry.branch, config)?;
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
            worktree_session_id: observability::with_writable_db(repo, |conn| {
                conn.query_row(
                    "select worktree_session_id from active_worktree_session
                     where branch = ?1 and worktree_path = ?2",
                    params![branch.as_str(), pending.worktree_path.as_str()],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(|error| format!("load pending Worktree Session identity: {error}"))
            })?
            .unwrap_or_else(|| {
                format!(
                    "legacy-pending-{:016x}",
                    crate::util::stable_hash(Path::new(&pending.worktree_path))
                )
            }),
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

pub(crate) fn reconcile_worktree_state(repo: &Repository, config: &Config) -> Result<(), String> {
    crate::lifecycle::prune_worktrees(repo, config)?;
    let live = crate::lifecycle::list_worktrees(repo, config)?;
    let persisted = observability::with_writable_db(repo, |conn| {
        let mut statement = conn
            .prepare(
                "select branch, worktree
                 from task_metadata
                 where branch not in (select branch from archived_worktree)",
            )
            .map_err(|error| format!("prepare worktree state inventory: {error}"))?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    PathBuf::from(row.get::<_, String>(1)?),
                ))
            })
            .map_err(|error| format!("query worktree state inventory: {error}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("read worktree state inventory: {error}"))
    })?;

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
                crate::opencode::shutdown_worktree_session_runtimes(repo, &branch, path)?;
            }
            let old_path = paths[0].display().to_string();
            let replacement_path = replacement.path.display().to_string();
            let replacement_incarnation = worktree_incarnation(&replacement.path);
            observability::with_writable_db(repo, |conn| {
                conn.execute(
                    "update task_metadata set worktree = ?1
                      where branch = ?2 and worktree = ?3",
                    params![replacement_path, branch, old_path],
                )
                .map_err(|error| format!("repoint moved worktree metadata: {error}"))?;
                conn.execute(
                    "update worktree_harness
                     set worktree_path = ?1, worktree_incarnation = ?2, updated_unix_ms = ?3
                     where branch = ?4 and worktree_path = ?5",
                    params![
                        replacement_path,
                        replacement_incarnation,
                        unix_seconds(),
                        branch,
                        old_path,
                    ],
                )
                .map_err(|error| format!("repoint moved worktree harness: {error}"))?;
                Ok(())
            })?;
        } else {
            let path = &paths[0];
            remove_worktree_session_owned_state(repo, config, path, &branch)?;
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

    let (runtime_sessions, agent_branches) = observability::with_writable_db(repo, |conn| {
        let mut runtime_statement = conn
            .prepare(
                "select branch, worktree_path from opencode_runtime
                 where branch not in (select branch from task_metadata)
                   and branch not in (select branch from archived_worktree)",
            )
            .map_err(|error| format!("prepare non-adopted runtime inventory: {error}"))?;
        let runtime_sessions = runtime_statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    PathBuf::from(row.get::<_, String>(1)?),
                ))
            })
            .map_err(|error| format!("query non-adopted runtime inventory: {error}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("read non-adopted runtime inventory: {error}"))?;
        let mut agent_statement = conn
            .prepare(
                "select branch from agent_state
                 where branch not in (select branch from task_metadata)
                   and branch not in (select branch from archived_worktree)",
            )
            .map_err(|error| format!("prepare non-adopted Agent Session inventory: {error}"))?;
        let agent_branches = agent_statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|error| format!("query non-adopted Agent Session inventory: {error}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("read non-adopted Agent Session inventory: {error}"))?;
        Ok((runtime_sessions, agent_branches))
    })?;
    let mut cleaned_branches = BTreeSet::new();
    for (branch, path) in runtime_sessions {
        if live
            .iter()
            .any(|entry| entry.branch == branch && entry.path == path)
        {
            continue;
        }
        if live.iter().any(|entry| entry.branch == branch) {
            crate::opencode::shutdown_worktree_session_runtimes(repo, &branch, &path)?;
            continue;
        }
        remove_worktree_session_owned_state(repo, config, &path, &branch)?;
        cleaned_branches.insert(branch);
    }
    for branch in agent_branches {
        if cleaned_branches.contains(&branch) || live.iter().any(|entry| entry.branch == branch) {
            continue;
        }
        crate::agent_session::shutdown(repo, config, &branch)?;
        crate::agent_session::remove_owned_log(repo, &branch)?;
        observability::with_writable_db(repo, |conn| {
            crate::agent_session::remove_state_with_conn(conn, &branch)
        })?;
    }
    Ok(())
}

fn remove_worktree_session_owned_state(
    repo: &Repository,
    config: &Config,
    path: &Path,
    branch: &str,
) -> Result<(), String> {
    remove_worktree_owned_state(repo, config, path, branch, None)
}

fn remove_deleted_worktree_owned_state(
    repo: &Repository,
    config: &Config,
    path: &Path,
    branch: &str,
    worktree_session_id: Option<&str>,
) -> Result<(), String> {
    remove_worktree_owned_state(repo, config, path, branch, worktree_session_id)
}

fn remove_worktree_owned_state(
    repo: &Repository,
    config: &Config,
    path: &Path,
    branch: &str,
    worktree_session_id: Option<&str>,
) -> Result<(), String> {
    if worktree_session_id.is_none() && !worktree_incarnation(path).is_empty() {
        return Err(format!(
            "retained state for {branch}: a live worktree now exists at {}",
            path.display()
        ));
    }
    let worktree_path = path.display().to_string();
    observability::with_writable_db(repo, |conn| {
        ensure_cleanup_ownership(conn, branch, &worktree_path, worktree_session_id)
    })?;
    let _server_lock = crate::opencode::lock_repository_server(repo)?;
    let mut runtimes = crate::opencode::load_runtimes_for_worktree_session(repo, branch, path)?;
    if let Some(worktree_session_id) = worktree_session_id {
        runtimes
            .retain(|runtime| runtime.worktree_session_id.as_deref() == Some(worktree_session_id));
    }
    shutdown_worktree_session_resources(repo, config, branch, worktree_session_id, &runtimes)?;
    observability::with_writable_db(repo, |conn| {
        let transaction =
            crate::flight_recorder::TransactionTrace::begin("session.remove_owned_state");
        conn.execute_batch("begin immediate transaction")
            .map_err(|error| format!("begin worktree session cleanup transaction: {error}"))?;
        let result = (|| {
            ensure_cleanup_ownership(conn, branch, &worktree_path, worktree_session_id)?;
            crate::remote::remove_pr_cache_with_conn(conn, branch)?;
            crate::agent_session::remove_state_with_conn(conn, branch)?;
            crate::opencode::remove_worktree_session_runtimes_with_conn(conn, &runtimes)?;
            conn.execute(
                "delete from task_metadata where branch = ?1 and worktree = ?2",
                params![branch, worktree_path],
            )
            .map_err(|error| format!("remove Worktree Session metadata: {error}"))?;
            conn.execute(
                "delete from worktree_harness where branch = ?1 and worktree_path = ?2",
                params![branch, worktree_path],
            )
            .map_err(|error| format!("remove worktree harness association: {error}"))?;
            clear_hidden_session_marker_with_conn(conn, branch)?;
            conn.execute(
                "delete from archived_worktree where branch = ?1 and worktree_path = ?2",
                params![branch, worktree_path],
            )
            .map_err(|error| format!("remove archived worktree metadata: {error}"))?;
            conn.execute(
                "delete from pending_worktree_deletion where branch = ?1 and worktree_path = ?2",
                params![branch, worktree_path],
            )
            .map_err(|error| format!("complete pending worktree deletion: {error}"))?;
            if let Some(worktree_session_id) = worktree_session_id {
                let removed = conn
                    .execute(
                        "delete from active_worktree_session where worktree_session_id = ?1",
                        params![worktree_session_id],
                    )
                    .map_err(|error| format!("retire deleted Worktree Session: {error}"))?;
                if removed != 1 {
                    return Err(
                        "deleted Worktree Session identity changed before cleanup".to_string()
                    );
                }
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                conn.execute_batch("commit").map_err(|error| {
                    format!("commit worktree session cleanup transaction: {error}")
                })?;
                transaction.committed();
                Ok(())
            }
            Err(error) => {
                let _ = conn.execute_batch("rollback");
                Err(error)
            }
        }
    })
}

fn ensure_cleanup_ownership(
    conn: &rusqlite::Connection,
    branch: &str,
    worktree_path: &str,
    worktree_session_id: Option<&str>,
) -> Result<(), String> {
    if let Some(worktree_session_id) = worktree_session_id {
        require_active_worktree_session_owner(conn, worktree_session_id, branch)?;
        let current_path = conn
            .query_row(
                "select worktree_path from active_worktree_session
                 where worktree_session_id = ?1",
                params![worktree_session_id],
                |row| row.get::<_, String>(0),
            )
            .map_err(|error| format!("inspect Worktree Session cleanup path: {error}"))?;
        if current_path != worktree_path {
            return Err(format!(
                "retained state for {branch}: Worktree Session moved to {current_path:?}"
            ));
        }
    }
    let current_path = conn
        .query_row(
            "select worktree from task_metadata where branch = ?1",
            params![branch],
            |row| row.get::<_, String>(0),
        )
        .optional()
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

fn build_session(
    repo: &Repository,
    path: PathBuf,
    branch: String,
    config: &Config,
) -> Result<Session, String> {
    let legacy_metadata_path = path
        .join(".agent/tasks")
        .join(format!("{}.json", safe_branch_filename(&branch)));
    let path_display = path.display().to_string();
    let incarnation = worktree_incarnation(&path);
    let worktree_session_id =
        resolve_worktree_session_identity(repo, &path, &branch, &incarnation)?;
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
    let status_label = git_status_label(&path, config);
    let agent_state = load_agent_state(repo, &branch).unwrap_or(AgentState::Idle);
    let pr = load_pr_cache_for_branch(repo, config, &branch, &path);
    Ok(Session {
        repo_index: 0,
        repo_label: String::new(),
        repo_key: None,
        path,
        worktree_session_id,
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

fn worktree_session_marker_path(worktree: &Path) -> Result<PathBuf, String> {
    let git = worktree.join(".git");
    if git.is_dir() {
        return Ok(git.join("prism-worktree-session-id"));
    }
    let contents = match fs::read_to_string(&git) {
        Ok(contents) => contents,
        #[cfg(test)]
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(worktree
                .join(".prism-test-git")
                .join("prism-worktree-session-id"));
        }
        Err(error) => {
            return Err(format!("read worktree Git link {}: {error}", git.display()));
        }
    };
    let target = contents
        .trim()
        .strip_prefix("gitdir:")
        .map(str::trim)
        .filter(|target| !target.is_empty())
        .ok_or_else(|| format!("invalid worktree Git link {}", git.display()))?;
    let target = PathBuf::from(target);
    let git_dir = if target.is_absolute() {
        target
    } else {
        worktree.join(target)
    };
    Ok(git_dir.join("prism-worktree-session-id"))
}

fn resolve_worktree_session_identity(
    repo: &Repository,
    worktree: &Path,
    branch: &str,
    incarnation: &str,
) -> Result<String, String> {
    if incarnation.is_empty() && !cfg!(test) {
        return Err(format!(
            "cannot identify Worktree Session at {} without Git administrative metadata",
            worktree.display()
        ));
    }
    let test_incarnation;
    let incarnation = if incarnation.is_empty() {
        test_incarnation = format!("test:{:016x}", crate::util::stable_hash(worktree));
        test_incarnation.as_str()
    } else {
        incarnation
    };
    let marker = worktree_session_marker_path(worktree)?;
    let marker_prefix = format!("{:016x}:", crate::util::stable_hash(&repo.root));
    observability::with_writable_db(repo, |conn| {
        conn.execute_batch("begin immediate transaction")
            .map_err(|error| format!("begin Worktree Session identity transaction: {error}"))?;
        let result = (|| {
            let existing = match fs::read_to_string(&marker) {
                Ok(value) => Some(value.trim().to_string()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => {
                    return Err(format!(
                        "read Worktree Session marker {}: {error}",
                        marker.display()
                    ));
                }
            };
            let id = match existing {
                Some(marker_value) => {
                    let Some(id) = marker_value.strip_prefix(&marker_prefix) else {
                        return Err(
                            "Worktree Session marker belongs to another repository".to_string()
                        );
                    };
                    if id.len() != 32 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                        return Err(format!(
                            "invalid Worktree Session marker {}",
                            marker.display()
                        ));
                    }
                    id.to_string()
                }
                None => {
                    let id = conn
                        .query_row("select lower(hex(randomblob(16)))", [], |row| {
                            row.get::<_, String>(0)
                        })
                        .map_err(|error| format!("allocate Worktree Session identity: {error}"))?;
                    crate::file_persistence::update(
                        &marker,
                        crate::file_persistence::UpdateOptions::important_toml(),
                        |_| Ok(((), Some(format!("{marker_prefix}{id}\n").into_bytes()))),
                    )
                    .map_err(|error| format!("write Worktree Session marker: {error}"))?;
                    id
                }
            };
            let repo_root = repo.root.display().to_string();
            let path = worktree.display().to_string();
            let owner = conn
                .query_row(
                    "select repo_root from worktree_session where id = ?1",
                    params![id.as_str()],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(|error| format!("read Worktree Session identity: {error}"))?;
            if owner.as_deref().is_some_and(|owner| owner != repo_root) {
                return Err("Worktree Session marker belongs to another repository".to_string());
            }
            conn.execute(
                "insert or ignore into worktree_session (
                    id, repo_root, initial_branch, initial_worktree_path, created_unix_ms
                 ) values (?1, ?2, ?3, ?4, ?5)",
                params![
                    id.as_str(),
                    repo_root.as_str(),
                    branch,
                    path.as_str(),
                    unix_seconds()
                ],
            )
            .map_err(|error| format!("record Worktree Session identity: {error}"))?;
            let previous_location = conn
                .query_row(
                    "select branch, worktree_path from active_worktree_session
                     where worktree_session_id = ?1",
                    params![id.as_str()],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()
                .map_err(|error| format!("inspect existing Worktree Session location: {error}"))?;
            let displaced_branches = {
                let mut statement = conn
                    .prepare(
                        "select branch from active_worktree_session
                         where repo_root = ?1 and worktree_session_id != ?2
                           and (branch = ?3 or worktree_path = ?4)",
                    )
                    .map_err(|error| format!("prepare replaced Worktree Session query: {error}"))?;
                statement
                    .query_map(
                        params![repo_root.as_str(), id.as_str(), branch, path.as_str()],
                        |row| row.get::<_, String>(0),
                    )
                    .map_err(|error| format!("query replaced Worktree Sessions: {error}"))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| format!("read replaced Worktree Sessions: {error}"))?
            };
            for displaced_branch in displaced_branches {
                retire_branch_owned_active_state(conn, &displaced_branch)?;
            }
            if let Some((previous_branch, previous_path)) = previous_location.as_ref()
                && (previous_branch != branch || previous_path != &path)
            {
                migrate_worktree_session_location(
                    conn,
                    previous_branch,
                    previous_path,
                    branch,
                    &path,
                    &repo_root,
                    &id,
                )?;
            }
            conn.execute(
                "delete from active_worktree_session
                 where repo_root = ?1 and worktree_session_id != ?2
                   and (branch = ?3 or worktree_path = ?4)",
                params![repo_root.as_str(), id.as_str(), branch, path.as_str()],
            )
            .map_err(|error| format!("retire replaced Worktree Session: {error}"))?;
            conn.execute(
                "insert into active_worktree_session (
                    worktree_session_id, repo_root, branch, worktree_path,
                    worktree_incarnation, observed_unix_ms
                 ) values (?1, ?2, ?3, ?4, ?5, ?6)
                 on conflict(worktree_session_id) do update set
                    repo_root = excluded.repo_root,
                    branch = excluded.branch,
                    worktree_path = excluded.worktree_path,
                    worktree_incarnation = excluded.worktree_incarnation,
                    observed_unix_ms = excluded.observed_unix_ms",
                params![
                    id.as_str(),
                    repo_root,
                    branch,
                    path,
                    incarnation,
                    unix_seconds()
                ],
            )
            .map_err(|error| format!("activate Worktree Session identity: {error}"))?;
            Ok(id)
        })();
        match result {
            Ok(id) => {
                conn.execute_batch("commit")
                    .map_err(|error| format!("commit Worktree Session identity: {error}"))?;
                Ok(id)
            }
            Err(error) => {
                let _ = conn.execute_batch("rollback");
                Err(error)
            }
        }
    })
}

#[cfg(test)]
fn ensure_worktree_session_identity(
    repo: &Repository,
    worktree: &Path,
    branch: &str,
) -> Result<String, String> {
    let incarnation = worktree_incarnation(worktree);
    resolve_worktree_session_identity(repo, worktree, branch, &incarnation)
}

fn retire_branch_owned_active_state(
    conn: &rusqlite::Connection,
    branch: &str,
) -> Result<(), String> {
    crate::remote::remove_pr_cache_with_conn(conn, branch)?;
    crate::agent_session::remove_state_with_conn(conn, branch)?;
    for (table, context) in [
        ("task_metadata", "retire task metadata"),
        ("hidden_session", "retire hidden marker"),
        ("archived_worktree", "retire archived worktree marker"),
        ("worktree_harness", "retire harness association"),
    ] {
        conn.execute(
            &format!("delete from {table} where branch = ?1"),
            params![branch],
        )
        .map_err(|error| format!("{context}: {error}"))?;
    }
    Ok(())
}

fn migrate_worktree_session_location(
    conn: &rusqlite::Connection,
    previous_branch: &str,
    previous_path: &str,
    branch: &str,
    worktree_path: &str,
    repo_root: &str,
    worktree_session_id: &str,
) -> Result<(), String> {
    for (table, context) in [
        ("task_metadata", "migrate task metadata"),
        ("hidden_session", "migrate hidden marker"),
        ("archived_worktree", "migrate archived worktree marker"),
        ("agent_state", "migrate agent state"),
        ("worktree_harness", "migrate harness association"),
        ("pr_cache", "migrate pull request cache"),
        ("pr_details_cache", "migrate pull request details cache"),
    ] {
        conn.execute(
            &format!("update {table} set branch = ?1 where branch = ?2"),
            params![branch, previous_branch],
        )
        .map_err(|error| format!("{context}: {error}"))?;
    }
    conn.execute(
        "update task_metadata set worktree = ?1 where branch = ?2",
        params![worktree_path, branch],
    )
    .map_err(|error| format!("migrate task metadata path: {error}"))?;
    conn.execute(
        "update worktree_harness set worktree_path = ?1 where branch = ?2",
        params![worktree_path, branch],
    )
    .map_err(|error| format!("migrate harness association path: {error}"))?;
    conn.execute(
        "delete from opencode_runtime
         where repo_root = ?1 and branch = ?2 and worktree_path = ?3
           and worktree_session_id is not ?4",
        params![repo_root, branch, worktree_path, worktree_session_id],
    )
    .map_err(|error| format!("retire displaced OpenCode runtime association: {error}"))?;
    conn.execute(
        "update opencode_runtime set branch = ?1, worktree_path = ?2
         where branch = ?3 and worktree_path = ?4 and worktree_session_id = ?5",
        params![
            branch,
            worktree_path,
            previous_branch,
            previous_path,
            worktree_session_id
        ],
    )
    .map_err(|error| format!("migrate OpenCode runtime association: {error}"))?;
    for (kind, table) in [("auto", "auto_run"), ("plan", "plan_run")] {
        conn.execute(
            &format!(
                "update workflow_execution set
                   dispatch_state = 'recovery_pending',
                   worker_id = null,
                   daemon_instance_id = null,
                   lease_expires_unix_ms = null,
                   heartbeat_unix_ms = null,
                   executor_pid = null,
                   executor_process_identity = null,
                   requeue_requested = 0,
                   interruption_generation = interruption_generation + 1,
                   fencing_token = fencing_token + 1,
                   updated_unix_ms = ?1
                 where workflow_kind = ?2 and dispatch_state = 'claimed'
                   and run_id in (
                     select id from {table} where worktree_session_id = ?3
                   )"
            ),
            params![
                unix_seconds().saturating_mul(1000),
                kind,
                worktree_session_id
            ],
        )
        .map_err(|error| format!("interrupt {kind} execution for location migration: {error}"))?;
    }
    conn.execute(
        "update auto_run set
           branch = ?1,
           worktree_path = ?2,
           plan_path = case
             when plan_path = ?3 then ?2
             when substr(plan_path, 1, length(?3) + 1) = ?3 || '/'
               then ?2 || substr(plan_path, length(?3) + 1)
             else plan_path
           end
         where worktree_session_id = ?4 and status not in ('done', 'aborted')",
        params![branch, worktree_path, previous_path, worktree_session_id],
    )
    .map_err(|error| format!("migrate active Auto Flow location: {error}"))?;
    conn.execute(
        "update plan_run set
           scope_path = ?1,
           plan_path = case
             when plan_path = ?2 then ?1
             when substr(plan_path, 1, length(?2) + 1) = ?2 || '/'
               then ?1 || substr(plan_path, length(?2) + 1)
             else plan_path
           end
         where worktree_session_id = ?3 and status not in ('done', 'aborted')",
        params![worktree_path, previous_path, worktree_session_id],
    )
    .map_err(|error| format!("migrate active Plan location: {error}"))?;
    Ok(())
}

pub(crate) fn worktree_session_is_active(
    conn: &rusqlite::Connection,
    worktree_session_id: &str,
) -> Result<bool, String> {
    #[cfg(test)]
    if !table_exists_for_identity(conn, "active_worktree_session")? {
        return Ok(true);
    }
    conn.query_row(
        "select exists(select 1 from active_worktree_session where worktree_session_id = ?1)",
        params![worktree_session_id],
        |row| row.get(0),
    )
    .map_err(|error| format!("validate active Worktree Session: {error}"))
}

#[cfg(test)]
fn table_exists_for_identity(conn: &rusqlite::Connection, table: &str) -> Result<bool, String> {
    conn.query_row(
        "select exists(select 1 from sqlite_master where type = 'table' and name = ?1)",
        params![table],
        |row| row.get(0),
    )
    .map_err(|error| format!("inspect Worktree Session identity schema: {error}"))
}

pub(crate) fn require_active_worktree_session_owner(
    conn: &rusqlite::Connection,
    worktree_session_id: &str,
    branch: &str,
) -> Result<(), String> {
    let owned = conn
        .query_row(
            "select exists(
               select 1 from active_worktree_session
               where worktree_session_id = ?1 and branch = ?2
             )",
            params![worktree_session_id, branch],
            |row| row.get::<_, bool>(0),
        )
        .map_err(|error| format!("validate Worktree Session ownership: {error}"))?;
    if owned {
        Ok(())
    } else {
        Err("Worktree Session is no longer active for this branch".to_string())
    }
}

pub(crate) fn worktree_incarnation(path: &Path) -> String {
    let git_link = path.join(".git");
    let Ok(metadata) = fs::metadata(&git_link) else {
        return String::new();
    };
    if metadata.is_dir() {
        use std::os::unix::fs::MetadataExt;
        return format!("directory:{}:{}", metadata.dev(), metadata.ino());
    }
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let target = fs::read_to_string(&git_link).unwrap_or_default();
    let file_id = {
        use std::os::unix::fs::MetadataExt;
        metadata.ino()
    };
    format!("{file_id}:{modified}:{}:{target}", metadata.len())
}

fn write_task_metadata(
    repo: &Repository,
    session: &Session,
    initial_prompt: &str,
) -> Result<(), String> {
    let summary = prompt_summary_from_text(initial_prompt);
    observability::with_writable_db(repo, |conn| {
        conn.execute(
            "insert into task_metadata (
                branch, prompt_summary, initial_prompt, worktree, classification, visibility, updated_unix_ms
             ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             on conflict(branch) do update set
                prompt_summary = excluded.prompt_summary,
                initial_prompt = excluded.initial_prompt,
                worktree = excluded.worktree,
                classification = excluded.classification,
                visibility = excluded.visibility,
                updated_unix_ms = excluded.updated_unix_ms",
            params![
                session.branch.as_str(),
                summary.as_str(),
                initial_prompt,
                session.path_display.as_str(),
                session.classification.label(),
                session.visibility,
                unix_seconds(),
            ],
        )
        .map_err(|error| format!("write task metadata: {error}"))?;
        Ok(())
    })
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
    observability::with_writable_db(repo, |conn| {
        conn.execute(
            "insert into task_metadata (
                branch, prompt_summary, initial_prompt, worktree, classification, visibility, updated_unix_ms
             ) values (?1, ?2, '', ?3, ?4, ?5, ?6)
             on conflict(branch) do update set
                worktree = excluded.worktree,
                classification = excluded.classification,
                visibility = excluded.visibility,
                updated_unix_ms = excluded.updated_unix_ms",
            params![
                session.branch.as_str(),
                session.prompt_summary.as_str(),
                session.path_display.as_str(),
                session.classification.label(),
                visibility,
                unix_seconds(),
            ],
        )
        .map_err(|error| format!("write worktree visibility: {error}"))?;
        Ok(())
    })
}

pub(crate) fn migrate_worktree_session_schema(conn: &rusqlite::Connection) -> Result<(), String> {
    conn.execute_batch(
        "
        create table if not exists task_metadata (
          branch text primary key,
          prompt_summary text not null,
          initial_prompt text not null,
          worktree text not null,
          classification text not null default 'work',
          visibility integer not null default 0,
          updated_unix_ms integer not null
        );

        create table if not exists worktree_session (
          id text primary key,
          repo_root text not null,
          initial_branch text not null,
          initial_worktree_path text not null,
          created_unix_ms integer not null
        ) without rowid;

        create table if not exists active_worktree_session (
          worktree_session_id text primary key references worktree_session(id),
          repo_root text not null,
          branch text not null,
          worktree_path text not null,
          worktree_incarnation text not null,
          observed_unix_ms integer not null,
          unique(repo_root, branch),
          unique(repo_root, worktree_path)
        ) without rowid;
        create index if not exists active_worktree_session_location_idx
          on active_worktree_session(repo_root, branch, worktree_path);

        create table if not exists hidden_session (
          branch text primary key,
          hidden_unix_ms integer not null
        );

        create table if not exists archived_worktree (
          branch text primary key,
          repo_root text not null,
          worktree_path text not null,
          archived_unix_ms integer not null,
          classification text not null default 'work'
        );

        create table if not exists agent_state (
          branch text primary key,
          state text not null,
          updated_unix_ms integer not null
        );

        create table if not exists worktree_harness (
          branch text primary key,
          worktree_path text not null,
          worktree_incarnation text not null,
          harness_id text not null,
          migration_policy text not null default 'ask',
          updated_unix_ms integer not null
        );

        create table if not exists pending_worktree_deletion (
          branch text primary key,
          worktree_path text not null,
          worktree_incarnation text not null,
          branch_oid text,
          worktree_removed integer not null default 0,
          branch_deleted integer not null default 0,
          updated_unix_ms integer not null
        );
        ",
    )
    .map_err(|error| format!("create worktree session schema: {error}"))?;
    add_column_if_missing(
        conn,
        "task_metadata",
        "classification",
        "alter table task_metadata add column classification text not null default 'work'",
    )?;
    add_column_if_missing(
        conn,
        "task_metadata",
        "visibility",
        "alter table task_metadata add column visibility integer not null default 0",
    )?;
    add_column_if_missing(
        conn,
        "pending_worktree_deletion",
        "branch_deleted",
        "alter table pending_worktree_deletion add column branch_deleted integer not null default 0",
    )?;
    Ok(())
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
    observability::with_writable_db(repo, |conn| {
        let stored = conn
            .query_row(
                "select worktree_path, worktree_incarnation, harness_id, migration_policy
                 from worktree_harness where branch = ?1",
                params![session.branch.as_str()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| format!("load worktree harness: {error}"))?;
        if let Some((path, incarnation, harness_id, policy)) = stored
            && path == session.path_display
            && worktree_incarnations_match(&incarnation, &session.incarnation)
        {
            let keep = policy == "keep";
            if incarnation != session.incarnation {
                set_worktree_harness_with_conn(conn, session, &harness_id, keep)?;
            }
            return Ok(WorktreeHarnessAssociation { harness_id, keep });
        }
        // Before multi-harness support every existing Agent Session was OpenCode.
        set_worktree_harness_with_conn(conn, session, "opencode", false)?;
        Ok(WorktreeHarnessAssociation {
            harness_id: "opencode".to_string(),
            keep: false,
        })
    })
}

fn worktree_incarnations_match(stored: &str, current: &str) -> bool {
    if stored == current {
        return true;
    }
    let Some(current_inode) = current
        .strip_prefix("directory:")
        .and_then(|identity| identity.rsplit(':').next())
    else {
        return false;
    };
    let mut legacy = stored.splitn(4, ':');
    let (Some(legacy_inode), Some(modified), Some(length), Some(target)) =
        (legacy.next(), legacy.next(), legacy.next(), legacy.next())
    else {
        return false;
    };
    legacy_inode == current_inode
        && modified.parse::<u128>().is_ok()
        && length.parse::<u64>().is_ok()
        && target.is_empty()
}

pub(crate) fn set_worktree_harness(
    repo: &Repository,
    session: &Session,
    harness_id: &str,
    keep: bool,
) -> Result<(), String> {
    observability::with_writable_db(repo, |conn| {
        set_worktree_harness_with_conn(conn, session, harness_id, keep)
    })
}

fn set_worktree_harness_with_conn(
    conn: &rusqlite::Connection,
    session: &Session,
    harness_id: &str,
    keep: bool,
) -> Result<(), String> {
    conn.execute(
        "insert into worktree_harness (
           branch, worktree_path, worktree_incarnation, harness_id, migration_policy, updated_unix_ms
         ) values (?1, ?2, ?3, ?4, ?5, ?6)
         on conflict(branch) do update set
           worktree_path = excluded.worktree_path,
           worktree_incarnation = excluded.worktree_incarnation,
           harness_id = excluded.harness_id,
           migration_policy = excluded.migration_policy,
           updated_unix_ms = excluded.updated_unix_ms",
        params![
            session.branch.as_str(),
            session.path_display.as_str(),
            session.incarnation.as_str(),
            harness_id,
            if keep { "keep" } else { "ask" },
            unix_seconds(),
        ],
    )
    .map_err(|error| format!("write worktree harness: {error}"))?;
    Ok(())
}

pub(crate) fn archive_worktree_session(repo: &Repository, session: &Session) -> Result<(), String> {
    observability::with_writable_db(repo, |conn| {
        let transaction = crate::flight_recorder::TransactionTrace::begin("session.archive");
        conn.execute_batch("begin transaction")
            .map_err(|error| format!("begin archive transaction: {error}"))?;
        let result = (|| -> Result<(), String> {
            conn.execute(
                "insert into hidden_session (branch, hidden_unix_ms)
                 values (?1, ?2)
                 on conflict(branch) do update set hidden_unix_ms = excluded.hidden_unix_ms",
                params![session.branch.as_str(), unix_seconds()],
            )
            .map_err(|error| format!("write hidden marker: {error}"))?;
            conn.execute(
                "insert into archived_worktree (
                    branch, repo_root, worktree_path, archived_unix_ms, classification
                 ) values (?1, ?2, ?3, ?4, ?5)
                 on conflict(branch) do update set
                    repo_root = excluded.repo_root,
                    worktree_path = excluded.worktree_path,
                    archived_unix_ms = excluded.archived_unix_ms,
                    classification = excluded.classification",
                params![
                    session.branch.as_str(),
                    repo.root.display().to_string(),
                    session.path_display.as_str(),
                    unix_seconds(),
                    session.classification.label(),
                ],
            )
            .map_err(|error| format!("write archived worktree metadata: {error}"))?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                conn.execute_batch("commit")
                    .map_err(|error| format!("commit archive transaction: {error}"))?;
                transaction.committed();
                Ok(())
            }
            Err(error) => {
                let _ = conn.execute_batch("rollback");
                Err(error)
            }
        }
    })
}

fn clear_hidden_session_marker_with_conn(
    conn: &rusqlite::Connection,
    branch: &str,
) -> Result<(), String> {
    conn.execute(
        "delete from hidden_session where branch = ?1",
        params![branch],
    )
    .map_err(|error| format!("remove hidden marker: {error}"))?;
    Ok(())
}

fn unarchive_worktree_session(repo: &Repository, branch: &str) -> Result<(), String> {
    observability::with_writable_db(repo, |conn| {
        let transaction = crate::flight_recorder::TransactionTrace::begin("session.unarchive");
        conn.execute_batch("begin transaction")
            .map_err(|error| format!("begin unarchive transaction: {error}"))?;
        let result = (|| -> Result<(), String> {
            clear_hidden_session_marker_with_conn(conn, branch)?;
            conn.execute(
                "delete from archived_worktree where branch = ?1",
                params![branch],
            )
            .map_err(|error| format!("remove archived worktree metadata: {error}"))?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                conn.execute_batch("commit")
                    .map_err(|error| format!("commit unarchive transaction: {error}"))?;
                transaction.committed();
                Ok(())
            }
            Err(error) => {
                let _ = conn.execute_batch("rollback");
                Err(error)
            }
        }
    })
}

pub(crate) fn list_archived_worktrees(repo: &Repository) -> Result<Vec<ArchivedWorktree>, String> {
    let path = observability::db_path(repo);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let conn = crate::storage::open_readonly(&path).map_err(|error| error.to_string())?;
    let table_count = conn
        .query_row(
            "select count(*) from sqlite_master where type = 'table' and name = 'archived_worktree'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| format!("inspect archived worktree table: {error}"))?;
    if table_count == 0 {
        return Ok(Vec::new());
    }
    let mut statement = conn
        .prepare(
            "select branch, worktree_path, classification
             from archived_worktree
             order by archived_unix_ms desc, branch asc",
        )
        .map_err(|error| format!("prepare archived worktree query: {error}"))?;
    let rows = statement
        .query_map([], |row| {
            Ok(ArchivedWorktree {
                branch: row.get(0)?,
                worktree_path: row.get(1)?,
                classification: SessionClassification::parse(&row.get::<_, String>(2)?),
            })
        })
        .map_err(|error| format!("read archived worktrees: {error}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read archived worktree row: {error}"))
}

fn hidden_session_exists(repo: &Repository, branch: &str) -> Result<bool, String> {
    let path = observability::db_path(repo);
    if !path.exists() {
        return Ok(false);
    }
    let conn = crate::storage::open_readonly(&path).map_err(|error| error.to_string())?;
    let table_count = conn
        .query_row(
            "select count(*) from sqlite_master where type = 'table' and name = 'hidden_session'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| format!("inspect hidden marker table: {error}"))?;
    if table_count == 0 {
        return Ok(false);
    }
    let count = conn
        .query_row(
            "select count(*) from hidden_session where branch = ?1",
            params![branch],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| format!("read hidden marker: {error}"))?;
    Ok(count > 0)
}

pub(crate) fn save_agent_state(
    repo: &Repository,
    worktree_session_id: &str,
    branch: &str,
    state: AgentState,
) -> Result<(), String> {
    observability::with_writable_db(repo, |conn| {
        require_active_worktree_session_owner(conn, worktree_session_id, branch)?;
        conn.execute(
            "insert into agent_state (branch, state, updated_unix_ms)
             values (?1, ?2, ?3)
             on conflict(branch) do update set
                state = excluded.state,
                updated_unix_ms = excluded.updated_unix_ms",
            params![branch, state.label(), unix_seconds()],
        )
        .map_err(|error| format!("write process state: {error}"))?;
        Ok(())
    })
}

pub(crate) fn remove_agent_state(
    repo: &Repository,
    worktree_session_id: &str,
    branch: &str,
) -> Result<(), String> {
    observability::with_writable_db(repo, |conn| {
        let current = conn
            .query_row(
                "select worktree_session_id from active_worktree_session where branch = ?1",
                params![branch],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| format!("inspect Agent Session state owner: {error}"))?;
        if current
            .as_deref()
            .is_some_and(|current| current != worktree_session_id)
        {
            return Ok(());
        }
        crate::agent_session::remove_state_with_conn(conn, branch)
    })
}

fn load_agent_state(repo: &Repository, branch: &str) -> Option<AgentState> {
    let state = observability::with_writable_db(repo, |conn| {
        conn.query_row(
            "select state from agent_state where branch = ?1",
            params![branch],
            |row| row.get::<_, String>(0),
        )
        .map_err(|error| format!("read process state: {error}"))
    })
    .ok()?;
    AgentState::parse(&state)
}

struct TaskMetadata {
    prompt_summary: String,
    classification: SessionClassification,
    visibility: i16,
}

fn load_task_metadata(repo: &Repository, branch: &str) -> Result<Option<TaskMetadata>, String> {
    observability::with_writable_db(repo, |conn| {
        conn.query_row(
            "select prompt_summary, classification, visibility from task_metadata where branch = ?1",
            params![branch],
            |row| {
                Ok(TaskMetadata {
                    prompt_summary: row.get(0)?,
                    classification: SessionClassification::parse(&row.get::<_, String>(1)?),
                    visibility: row.get(2)?,
                })
            },
        )
        .optional()
        .map_err(|error| format!("read task metadata: {error}"))
    })
}

pub(crate) fn load_task_initial_prompt(
    repo: &Repository,
    branch: &str,
) -> Result<Option<String>, String> {
    observability::with_writable_db(repo, |conn| {
        conn.query_row(
            "select initial_prompt from task_metadata where branch = ?1",
            params![branch],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| format!("read task initial prompt: {error}"))
    })
}

fn load_hidden_sessions(repo: &Repository) -> Result<BTreeMap<String, i64>, String> {
    observability::with_writable_db(repo, |conn| {
        let mut statement = conn
            .prepare("select branch, hidden_unix_ms from hidden_session")
            .map_err(|error| format!("read hidden sessions: {error}"))?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(|error| format!("read hidden sessions: {error}"))?;
        let mut hidden = BTreeMap::new();
        for row in rows {
            let (branch, hidden_unix_ms) =
                row.map_err(|error| format!("read hidden session: {error}"))?;
            hidden.insert(branch, hidden_unix_ms);
        }
        Ok(hidden)
    })
}

fn add_column_if_missing(
    conn: &rusqlite::Connection,
    table: &str,
    column: &str,
    sql: &str,
) -> Result<(), String> {
    let mut statement = conn
        .prepare(&format!("pragma table_info({table})"))
        .map_err(|error| format!("inspect {table} schema: {error}"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| format!("inspect {table} schema: {error}"))?;
    for value in columns {
        if value.map_err(|error| format!("inspect {table} schema: {error}"))? == column {
            return Ok(());
        }
    }
    conn.execute_batch(sql)
        .map_err(|error| format!("migrate {table}.{column}: {error}"))
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
    use crate::test_support::write_executable;

    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn recreated_same_branch_and_path_gets_new_durable_identity_and_no_active_state() {
        let temp = unique_temp_dir("prism-worktree-session-identity-test");
        let worktree = temp.join("worktree");
        fs::create_dir_all(worktree.join(".git")).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        let first_incarnation = worktree_incarnation(&worktree);
        let first =
            resolve_worktree_session_identity(&repo, &worktree, "feature", &first_incarnation)
                .unwrap();
        let repeated =
            resolve_worktree_session_identity(&repo, &worktree, "feature", &first_incarnation)
                .unwrap();
        assert_eq!(repeated, first);
        observability::with_writable_db(&repo, |conn| {
            conn.execute(
                "insert into task_metadata (
                    branch, prompt_summary, initial_prompt, worktree, updated_unix_ms
                 ) values ('feature', 'old prompt', 'old prompt', ?1, 0)",
                params![worktree.display().to_string()],
            )
            .map_err(|error| error.to_string())?;
            conn.execute(
                "insert into agent_state (branch, state, updated_unix_ms)
                 values ('feature', 'running', 0)",
                [],
            )
            .map_err(|error| error.to_string())?;
            Ok(())
        })
        .unwrap();

        fs::remove_dir_all(&worktree).unwrap();
        fs::create_dir_all(worktree.join(".git")).unwrap();
        let replacement_incarnation = worktree_incarnation(&worktree);
        let replacement = resolve_worktree_session_identity(
            &repo,
            &worktree,
            "feature",
            &replacement_incarnation,
        )
        .unwrap();

        assert_ne!(replacement, first);
        observability::with_writable_db(&repo, |conn| {
            assert!(!worktree_session_is_active(conn, &first)?);
            assert!(worktree_session_is_active(conn, &replacement)?);
            let metadata: i64 = conn
                .query_row("select count(*) from task_metadata", [], |row| row.get(0))
                .map_err(|error| error.to_string())?;
            let agent_state: i64 = conn
                .query_row("select count(*) from agent_state", [], |row| row.get(0))
                .map_err(|error| error.to_string())?;
            assert_eq!(metadata, 0);
            assert_eq!(agent_state, 0);
            Ok(())
        })
        .unwrap();
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn stale_agent_state_removal_cannot_delete_replacement_state() {
        let temp = unique_temp_dir("prism-stale-agent-state-removal-test");
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        observability::with_writable_db(&repo, |conn| {
            conn.execute(
                "insert into worktree_session (
                    id, repo_root, initial_branch, initial_worktree_path, created_unix_ms
                 ) values ('replacement', ?1, 'feature', '/repo/feature', 1)",
                params![repo.root.display().to_string()],
            )
            .map_err(|error| error.to_string())?;
            conn.execute(
                "insert into active_worktree_session (
                    worktree_session_id, repo_root, branch, worktree_path,
                    worktree_incarnation, observed_unix_ms
                 ) values ('replacement', ?1, 'feature', '/repo/feature', 'new', 1)",
                params![repo.root.display().to_string()],
            )
            .map_err(|error| error.to_string())?;
            conn.execute(
                "insert into agent_state (branch, state, updated_unix_ms)
                 values ('feature', 'running', 1)",
                [],
            )
            .map_err(|error| error.to_string())?;
            Ok(())
        })
        .unwrap();

        remove_agent_state(&repo, "retired", "feature").unwrap();

        assert_eq!(
            load_agent_state(&repo, "feature"),
            Some(AgentState::Running)
        );
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn owned_state_cleanup_rolls_back_all_branch_rows_on_late_failure() {
        let temp = unique_temp_dir("prism-session-atomic-cleanup-test");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        let path = temp.join("worktree");
        let branch = "feature/replaced";
        let tmux = temp.join("tmux");
        write_executable(&tmux, "#!/bin/sh\nexit 0\n");
        let mut config = test_config();
        config
            .tools
            .insert("tmux".to_string(), tmux.display().to_string());
        observability::with_writable_db(&repo, |conn| {
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
                    branch, number, title, url, state, review_decision, head_ref, base_ref,
                    head_sha, updated_at, check_status, merged, draft, last_refreshed,
                    refreshed_unix_ms
                 ) values (?1, 42, '', '', 'OPEN', '', ?1, 'main', 'head', '', '', 0, 0, '', 0)",
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

        let error = remove_worktree_owned_state(&repo, &config, &path, branch, None).unwrap_err();

        assert!(error.contains("archived worktree metadata"));
        observability::with_writable_db(&repo, |conn| {
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

    #[test]
    fn discover_sessions_skips_missing_worktree_paths() {
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

        let sessions = discover_sessions(&repo, &config).unwrap();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].path, repo_path);
        assert_eq!(sessions[0].branch, "main");

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn reconcile_worktree_state_removes_only_stale_persisted_sessions() {
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
        observability::with_writable_db(&repo, |conn| {
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

        reconcile_worktree_state(&repo, &config).unwrap();

        observability::with_writable_db(&repo, |conn| {
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

    #[test]
    fn reconcile_external_branch_rename_removes_only_old_adopted_state() {
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
        observability::with_writable_db(&repo, |conn| {
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

        reconcile_worktree_state(&repo, &config).unwrap();

        assert_eq!(count_rows(&repo, "task_metadata", "old-name"), 0);
        assert_eq!(count_rows(&repo, "agent_state", "old-name"), 0);
        assert_eq!(count_rows(&repo, "opencode_runtime", "old-name"), 0);
        assert_eq!(count_rows(&repo, "opencode_runtime", "new-name"), 1);
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn durable_identity_migrates_branch_owned_state_across_rename() {
        let temp = unique_temp_dir("prism-durable-identity-branch-rename-test");
        let worktree = temp.join("worktree");
        fs::create_dir_all(&worktree).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        let id = ensure_worktree_session_identity(&repo, &worktree, "old-name").unwrap();
        let mut auto = crate::auto_flow::AutoLaunch::new(
            &repo.root,
            &worktree,
            "old-name",
            "continue after rename",
        )
        .unwrap()
        .with_worktree_session_id(id.clone())
        .create_run();
        observability::with_writable_db(&repo, |conn| {
            crate::auto_flow::submit_auto_run(conn, &mut auto)?;
            conn.execute(
                "update workflow_execution set
                   dispatch_state = 'claimed', worker_id = 'worker', daemon_instance_id = 'daemon',
                   lease_expires_unix_ms = 9999999999999, fencing_token = 1
                 where workflow_kind = 'auto' and run_id = ?1",
                params![auto.run.id],
            )
            .map_err(|error| error.to_string())?;
            conn.execute(
                "insert into task_metadata (
                    branch, prompt_summary, initial_prompt, worktree, updated_unix_ms
                 ) values ('old-name', 'summary', 'prompt', ?1, 0)",
                params![worktree.display().to_string()],
            )
            .map_err(|error| error.to_string())?;
            conn.execute(
                "insert into agent_state (branch, state, updated_unix_ms)
                 values ('old-name', 'running', 0)",
                [],
            )
            .map_err(|error| error.to_string())?;
            conn.execute(
                "insert into opencode_runtime (
                    repo_root, branch, worktree_path, server_port, server_url,
                    generation, updated_unix_ms, worktree_session_id
                 ) values (?1, 'old-name', ?2, 41000, 'http://127.0.0.1:41000', 1, 0, ?3)",
                params![
                    repo.root.display().to_string(),
                    worktree.display().to_string(),
                    id
                ],
            )
            .map_err(|error| error.to_string())?;
            Ok(())
        })
        .unwrap();

        let renamed_id = ensure_worktree_session_identity(&repo, &worktree, "new-name").unwrap();

        assert_eq!(renamed_id, id);
        assert_eq!(count_rows(&repo, "task_metadata", "old-name"), 0);
        assert_eq!(count_rows(&repo, "task_metadata", "new-name"), 1);
        assert_eq!(count_rows(&repo, "agent_state", "new-name"), 1);
        assert_eq!(count_rows(&repo, "opencode_runtime", "new-name"), 1);
        observability::with_writable_db(&repo, |conn| {
            let active_branch: String = conn
                .query_row(
                    "select branch from active_worktree_session where worktree_session_id = ?1",
                    params![id],
                    |row| row.get(0),
                )
                .map_err(|error| error.to_string())?;
            assert_eq!(active_branch, "new-name");
            let migrated: (String, String, String) = conn
                .query_row(
                    "select r.branch, r.worktree_path, e.dispatch_state
                     from auto_run r join workflow_execution e
                       on e.workflow_kind = 'auto' and e.run_id = r.id
                     where r.id = ?1",
                    params![auto.run.id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .map_err(|error| error.to_string())?;
            assert_eq!(
                migrated,
                (
                    "new-name".to_string(),
                    worktree.display().to_string(),
                    "recovery_pending".to_string()
                )
            );
            Ok(())
        })
        .unwrap();
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn reconcile_removes_stale_non_adopted_agent_and_runtime_state() {
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
        observability::with_writable_db(&repo, |conn| {
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

        reconcile_worktree_state(&repo, &config).unwrap();

        assert_eq!(count_rows(&repo, "agent_state", "stale"), 0);
        assert_eq!(count_rows(&repo, "opencode_runtime", "stale"), 0);
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn reconcile_moved_adopted_branch_keeps_branch_state_and_retires_old_path_runtime() {
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
        observability::with_writable_db(&repo, |conn| {
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

        reconcile_worktree_state(&repo, &config).unwrap();

        let (metadata_path, old_runtime, new_runtime, agent_state) =
            observability::with_writable_db(&repo, |conn| {
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

    #[test]
    fn reconcile_moved_non_adopted_branch_keeps_branch_only_retry_state() {
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
        observability::with_writable_db(&repo, |conn| {
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

        reconcile_worktree_state(&repo, &config).unwrap();

        assert_eq!(count_rows(&repo, "agent_state", "feature"), 1);
        assert_eq!(count_rows(&repo, "opencode_runtime", "feature"), 1);
        assert!(
            !tmux_log.exists(),
            "moved-path cleanup shut down the live branch"
        );
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn cleanup_shutdown_failure_keeps_rows_for_successful_retry() {
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
        observability::with_writable_db(&repo, |conn| {
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

        let outcome =
            delete_worktree_session_if_current(&repo, &config, &path, branch, None).unwrap();

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
        let retried =
            delete_worktree_session_if_current(&repo, &config, &path, branch, None).unwrap();
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

    #[test]
    fn recreated_same_path_and_branch_after_git_removal_retains_resources_and_state() {
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
        observability::with_writable_db(&repo, |conn| {
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
        .unwrap_err();

        assert!(error.contains("recreated"));
        assert_eq!(count_rows(&repo, "task_metadata", branch), 1);
        assert_eq!(count_rows(&repo, "agent_state", branch), 1);
        assert!(!tmux_log.exists(), "replacement resources were shut down");
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn worktree_incarnation_ignores_git_directory_activity_but_detects_replacement() {
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

    #[test]
    fn worktree_session_default_branch_sorts_first() {
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

    #[test]
    fn planning_and_exploration_sessions_sort_below_work_sessions() {
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

    #[test]
    fn hidden_sessions_sort_below_focused_sessions() {
        let config = test_config();
        let focused = test_session("feature-a", "/repo/a");
        let mut hidden = test_session("feature-b", "/repo/b");
        hidden.hidden = true;

        assert_eq!(
            session_discovery_order(&config, &focused, &hidden),
            std::cmp::Ordering::Less
        );
    }

    #[test]
    fn archived_worktree_metadata_records_restore_details_and_hides_session() {
        let temp = unique_temp_dir("prism-archive-worktree-test");
        let repo_path = temp.join("repo");
        let worktree = temp.join("worktree");
        fs::create_dir_all(&repo_path).unwrap();
        fs::create_dir_all(&worktree).unwrap();
        let repo = Repository::with_config_dir_for_test(repo_path.clone(), temp.join("config"));
        let mut session = test_session("feature", &worktree.display().to_string());
        session.classification = SessionClassification::Planning;

        archive_worktree_session(&repo, &session).unwrap();

        let row = observability::with_writable_db(&repo, |conn| {
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

    #[test]
    fn worktree_harness_binding_isolated_by_incarnation_and_can_be_pinned() {
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

    #[test]
    fn worktree_harness_migrates_legacy_directory_incarnation() {
        let temp = unique_temp_dir("prism-worktree-harness-incarnation-migration-test");
        let worktree = temp.join("worktree");
        fs::create_dir_all(worktree.join(".git")).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        let mut session = test_session("main", &worktree.display().to_string());
        session.incarnation = worktree_incarnation(&worktree);
        set_worktree_harness(&repo, &session, "codex", true).unwrap();
        let inode = session.incarnation.rsplit(':').next().unwrap();
        let legacy_incarnation = format!("{inode}:123:40:");
        observability::with_writable_db(&repo, |conn| {
            conn.execute(
                "update worktree_harness set worktree_incarnation = ?1 where branch = ?2",
                params![legacy_incarnation, session.branch.as_str()],
            )
            .map_err(|error| error.to_string())?;
            Ok(())
        })
        .unwrap();

        assert_eq!(
            worktree_harness(&repo, &session).unwrap(),
            WorktreeHarnessAssociation {
                harness_id: "codex".to_string(),
                keep: true,
            }
        );
        let migrated = observability::with_writable_db(&repo, |conn| {
            conn.query_row(
                "select worktree_incarnation from worktree_harness where branch = ?1",
                params![session.branch.as_str()],
                |row| row.get::<_, String>(0),
            )
            .map_err(|error| error.to_string())
        })
        .unwrap();
        assert_eq!(migrated, session.incarnation);

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn unarchive_worktree_session_clears_hidden_and_archived_markers() {
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

    #[test]
    fn archive_failure_does_not_leave_visible_session_archived() {
        let temp = unique_temp_dir("prism-archive-atomic-failure-test");
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        let session = test_session("feature", "/repo/feature");
        observability::with_writable_db(&repo, |conn| {
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

    #[test]
    fn unarchive_failure_keeps_hidden_and_archived_state_coherent() {
        let temp = unique_temp_dir("prism-unarchive-atomic-failure-test");
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        let session = test_session("feature", "/repo/feature");
        archive_worktree_session(&repo, &session).unwrap();
        observability::with_writable_db(&repo, |conn| {
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

    #[test]
    fn hidden_session_exists_missing_db_is_false_without_creating_db() {
        let temp = unique_temp_dir("prism-hidden-session-missing-db-test");
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        let db = observability::db_path(&repo);

        assert!(!hidden_session_exists(&repo, "feature").unwrap());
        assert!(!db.exists());

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn hidden_session_exists_missing_table_is_false() {
        let temp = unique_temp_dir("prism-hidden-session-missing-table-test");
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        let db = observability::db_path(&repo);
        fs::create_dir_all(db.parent().unwrap()).unwrap();
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute_batch("create table unrelated (id integer primary key)")
            .unwrap();

        assert!(!hidden_session_exists(&repo, "feature").unwrap());

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn phase_1_same_path_changed_branch_does_not_inherit_agent_session_or_pr_cache_facts() {
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

    #[test]
    fn recreated_worktree_at_same_path_and_branch_has_new_identity() {
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

    #[test]
    fn detached_session_discovery_refresh_preserves_matching_session() {
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
        previous.worktree_session_id = resolve_worktree_session_identity(
            &repo,
            &worktree,
            "(detached)",
            &worktree_incarnation(&worktree),
        )
        .unwrap();
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
        .unwrap();

        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].is_detached());
        assert_eq!(sessions[0].agent_state, AgentState::Running);
        assert!(sessions[0].pr.display_error().is_none());
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn refresh_rejects_one_worktree_marker_claimed_by_different_repositories() {
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

        let error = refresh_worktree_sessions(
            &repositories,
            &BTreeMap::from([(0, identity_a.clone()), (1, identity_b.clone())]),
            &mut sessions,
        )
        .unwrap_err();

        assert!(error.contains("belongs to another repository"), "{error}");
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn persistence_read_failure_preserves_previous_safe_session_facts() {
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
            .is_err()
        );
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].adopted);
        assert_eq!(sessions[0].agent_state, AgentState::Running);
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn task_metadata_read_failure_does_not_replace_adopted_session_with_absence() {
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
        observability::with_writable_db(&repo, |conn| {
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
            .is_err()
        );
        assert!(sessions[0].adopted);
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn archived_worktree_read_failure_is_not_reported_as_an_empty_archive() {
        let temp = unique_temp_dir("prism-archive-read-failure-test");
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        observability::with_writable_db(&repo, |conn| {
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

    #[test]
    fn mark_adopted_with_prompt_updates_local_metadata_facts() {
        let mut session = test_session("feature", "/repo/feature");

        session.mark_adopted_with_prompt("first line\nsecond line with extra text");

        assert!(session.adopted);
        assert_eq!(
            session.prompt_summary,
            "first line second line with extra text"
        );
    }

    #[test]
    fn adoption_reports_partial_success_without_marking_session_adopted() {
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

    #[test]
    fn creation_reports_partial_success_when_metadata_restoration_fails() {
        let temp = unique_temp_dir("prism-session-creation-partial-test");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        let db = observability::db_path(&repo);
        let wt = temp.join("wt");
        write_executable(
            &wt,
            &format!(
                "#!/bin/sh\nmkdir -p '{}'\nprintf '%s' '{{\"action\":\"created\",\"branch\":\"feature\",\"path\":\"/repo/worktree\",\"created_branch\":true}}'\n",
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

        let outcome = create_worktree_session(&repo, &config, "feature").unwrap();

        assert!(matches!(
            outcome,
            CreateWorktreeOutcome::CreatedMetadataFailed { .. }
        ));
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn deletion_warnings_describe_worktree_session_local_risks() {
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

    #[test]
    fn archive_warnings_describe_non_destructive_hiding() {
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

    fn test_session(branch: &str, path: &str) -> Session {
        Session {
            repo_index: 0,
            repo_label: "repo".to_string(),
            repo_key: None,
            path: PathBuf::from(path),
            worktree_session_id: format!("test-{branch}"),
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
        observability::with_writable_db(repo, |conn| count_rows_with_conn(conn, table, branch))
            .unwrap()
    }

    fn count_rows_with_conn(
        conn: &rusqlite::Connection,
        table: &str,
        branch: &str,
    ) -> Result<i64, String> {
        conn.query_row(
            &format!("select count(*) from {table} where branch = ?1"),
            params![branch],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| error.to_string())
    }
}
