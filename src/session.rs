use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::agent::AgentState;
use crate::config::Config;
use crate::git::git_status_label;
use crate::github::{PrCache, load_pr_cache_for_branch};
use crate::json::json_string_field;
use crate::observability::{self, LogLevel};
use crate::opencode::OpencodeStatus;
use crate::process::run_capture;
use crate::repo::Repository;
use crate::util::{safe_branch_filename, status_count, truncate};

#[derive(Clone, Debug, PartialEq, Eq)]
struct WorktreeInventoryEntry {
    path: PathBuf,
    branch: String,
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

    pub(crate) fn identity_key(&self) -> WorktreeSessionKey {
        WorktreeSessionKey {
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
        let pr_eligible = self.is_task_branch(config) && !self.hidden;
        self.pr.reconcile_session_refresh(previous.pr, pr_eligible);
        if pr_eligible {
            self.unseen_comments = previous.unseen_comments;
        } else {
            self.unseen_comments = false;
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
        if self.agent_state == AgentState::Running {
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
        if self.agent_state == AgentState::Running {
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

pub(crate) fn create_worktree_session(
    repo: &Repository,
    config: &Config,
    branch: &str,
) -> Result<CreateWorktreeOutcome, String> {
    create_or_checkout_worktree_session(repo, config, branch, false)
}

pub(crate) fn checkout_worktree_session(
    repo: &Repository,
    config: &Config,
    branch: &str,
) -> Result<CreateWorktreeOutcome, String> {
    create_or_checkout_worktree_session(repo, config, branch, true)
}

fn create_or_checkout_worktree_session(
    repo: &Repository,
    config: &Config,
    branch: &str,
    checkout: bool,
) -> Result<CreateWorktreeOutcome, String> {
    if hidden_session_exists(repo, branch)?
        && crate::lifecycle::branch_has_worktree(repo, config, branch)?
    {
        unarchive_worktree_session(repo, branch)?;
        return Ok(CreateWorktreeOutcome::Restored);
    }
    if checkout {
        crate::lifecycle::checkout_worktree(repo, config, branch)?;
    } else {
        crate::lifecycle::create_worktree(repo, config, branch)?;
    }
    match unarchive_worktree_session(repo, branch) {
        Ok(()) => Ok(CreateWorktreeOutcome::Created),
        Err(error) => Ok(CreateWorktreeOutcome::CreatedMetadataFailed { error }),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DeleteWorktreeOutcome {
    Deleted,
    BranchRetained { error: String },
    DeletedWithWarnings { errors: Vec<String> },
}

pub(crate) fn delete_worktree_session(
    repo: &Repository,
    config: &Config,
    path: &Path,
    branch: &str,
) -> Result<DeleteWorktreeOutcome, String> {
    delete_worktree_session_if_current(repo, config, path, branch, None)
}

pub(crate) fn delete_worktree_session_if_current(
    repo: &Repository,
    config: &Config,
    path: &Path,
    branch: &str,
    expected_incarnation: Option<&str>,
) -> Result<DeleteWorktreeOutcome, String> {
    if expected_incarnation.is_some_and(|expected| worktree_incarnation(path) != expected) {
        return Err(format!(
            "worktree {branch} was replaced while deletion was pending; retained the replacement"
        ));
    }
    if let Some(current) = load_worktree_inventory(repo, config)?
        .into_iter()
        .find(|entry| entry.path == path)
        && current.branch != branch
    {
        return Err(format!(
            "worktree changed from branch {branch} to {}; retained the current worktree",
            current.branch
        ));
    }
    let branch_incarnation = if branch == "(detached)" {
        None
    } else {
        Some(crate::lifecycle::branch_oid(repo, config, branch)?)
    };
    crate::lifecycle::remove_worktree(repo, config, path)?;
    if branch != "(detached)" && crate::lifecycle::branch_has_worktree(repo, config, branch)? {
        return Ok(DeleteWorktreeOutcome::BranchRetained {
            error: format!("branch {branch} is attached to a new worktree and was retained"),
        });
    }
    if let Some(expected_oid) = branch_incarnation.as_deref() {
        match crate::lifecycle::branch_oid(repo, config, branch) {
            Ok(current_oid) if current_oid == expected_oid => {}
            Ok(_) => {
                return Ok(DeleteWorktreeOutcome::BranchRetained {
                    error: format!(
                        "branch {branch} changed while deletion was in progress; retained its Prism state"
                    ),
                });
            }
            Err(error) => {
                return Ok(DeleteWorktreeOutcome::BranchRetained {
                    error: format!(
                        "could not verify branch {branch} after worktree removal; retained its Prism state: {error}"
                    ),
                });
            }
        }
    }

    let mut errors = Vec::new();
    if let Err(error) = shutdown_worktree_session_resources(repo, config, path, branch) {
        errors.push(format!(
            "resource shutdown failed; retained Prism state for retry: {error}"
        ));
    } else if let Err(error) = remove_deleted_worktree_owned_state(repo, path, branch) {
        errors.push(error);
    }
    if let Err(error) = crate::lifecycle::delete_branch_if_same_incarnation(
        repo,
        config,
        branch,
        branch_incarnation.as_deref(),
    ) {
        if errors.is_empty() {
            return Ok(DeleteWorktreeOutcome::BranchRetained { error });
        }
        errors.push(error);
    }
    if errors.is_empty() {
        Ok(DeleteWorktreeOutcome::Deleted)
    } else {
        Ok(DeleteWorktreeOutcome::DeletedWithWarnings { errors })
    }
}

fn shutdown_worktree_session_resources(
    repo: &Repository,
    config: &Config,
    path: &Path,
    branch: &str,
) -> Result<(), String> {
    let mut errors = Vec::new();
    if let Err(error) = crate::agent_session::shutdown(repo, config, branch) {
        errors.push(error);
    }
    if let Err(error) = crate::opencode::shutdown_worktree_session_runtimes(repo, branch, path) {
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
}

pub(crate) fn refresh_worktree_sessions(
    repositories: &[WorktreeSessionRepository<'_>],
    previous_repository_roots: &BTreeMap<usize, PathBuf>,
    current: &mut Vec<Session>,
) -> Result<(), String> {
    let mut discovered_by_repository = Vec::new();
    for repository in repositories {
        discovered_by_repository.push(discover_sessions(repository.repo, repository.config)?);
    }
    let mut previous = std::mem::take(current)
        .into_iter()
        .filter_map(|session| {
            let repo_root = previous_repository_roots.get(&session.repo_index)?;
            Some(((repo_root.clone(), session.identity_key()), session))
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
            let identity = (repository.repo.root.clone(), session.identity_key());
            if let Some(old) = previous.remove(&identity) {
                session.preserve_refresh_state_from(old, repository.config);
            }
        }
        refreshed.extend(discovered);
    }
    *current = refreshed;
    Ok(())
}

pub fn discover_sessions(repo: &Repository, config: &Config) -> Result<Vec<Session>, String> {
    let inventory = load_worktree_inventory(repo, config)?;
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

    sessions.sort_by(|a, b| session_discovery_order(config, a, b));
    Ok(sessions)
}

fn load_worktree_inventory(
    repo: &Repository,
    config: &Config,
) -> Result<Vec<WorktreeInventoryEntry>, String> {
    let output = run_capture(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(&repo.root)
            .args(["worktree", "list", "--porcelain"]),
    )?;
    Ok(parse_worktree_inventory(&output))
}

fn parse_worktree_inventory(output: &str) -> Vec<WorktreeInventoryEntry> {
    let mut entries = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_branch: Option<String> = None;

    for line in output.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            if let Some(path) = current_path.take() {
                let branch = current_branch
                    .take()
                    .unwrap_or_else(|| "(detached)".to_string());
                entries.push(WorktreeInventoryEntry { path, branch });
            }
            continue;
        }
        if let Some(path) = line.strip_prefix("worktree ") {
            current_path = Some(PathBuf::from(path));
        } else if let Some(branch) = line.strip_prefix("branch ") {
            current_branch = Some(
                branch
                    .strip_prefix("refs/heads/")
                    .unwrap_or(branch)
                    .to_string(),
            );
        } else if line.starts_with("detached") {
            current_branch = Some("(detached)".to_string());
        }
    }

    entries
}

pub(crate) fn reconcile_worktree_state(repo: &Repository, config: &Config) -> Result<(), String> {
    run_capture(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(&repo.root)
            .args(["worktree", "prune"]),
    )?;
    let live = load_worktree_inventory(repo, config)?;
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
        let is_live = live.iter().any(|entry| entry.branch == branch);
        if !is_live {
            let path = &paths[0];
            crate::agent_session::shutdown(repo, config, &branch)?;
            crate::opencode::shutdown_worktree_session_runtimes(repo, &branch, path)?;
            remove_worktree_session_owned_state(repo, path, &branch)?;
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
        if live.iter().any(|entry| entry.branch == branch) {
            continue;
        }
        crate::agent_session::shutdown(repo, config, &branch)?;
        crate::opencode::shutdown_worktree_session_runtimes(repo, &branch, &path)?;
        crate::github::remove_pr_cache(repo, &branch)?;
        crate::agent_session::remove_owned_state(repo, &branch)?;
        cleaned_branches.insert(branch);
    }
    for branch in agent_branches {
        if cleaned_branches.contains(&branch) || live.iter().any(|entry| entry.branch == branch) {
            continue;
        }
        crate::agent_session::shutdown(repo, config, &branch)?;
        crate::agent_session::remove_owned_state(repo, &branch)?;
    }
    Ok(())
}

pub(crate) fn remove_worktree_session_owned_state(
    repo: &Repository,
    path: &Path,
    branch: &str,
) -> Result<(), String> {
    remove_worktree_owned_state(repo, path, branch, false)
}

pub(crate) fn remove_deleted_worktree_owned_state(
    repo: &Repository,
    path: &Path,
    branch: &str,
) -> Result<(), String> {
    remove_worktree_owned_state(repo, path, branch, true)
}

fn remove_worktree_owned_state(
    repo: &Repository,
    path: &Path,
    branch: &str,
    _worktree_was_deleted: bool,
) -> Result<(), String> {
    let worktree_path = path.display().to_string();
    observability::with_writable_db(repo, |conn| {
        ensure_cleanup_ownership(conn, branch, &worktree_path)
    })?;
    crate::github::remove_pr_cache(repo, branch)?;
    crate::agent_session::remove_owned_state(repo, branch)?;
    observability::with_writable_db(repo, |conn| {
        conn.execute_batch("begin transaction")
            .map_err(|error| format!("begin worktree session cleanup transaction: {error}"))?;
        let result = (|| -> Result<(), String> {
            ensure_cleanup_ownership(conn, branch, &worktree_path)?;
            conn.execute(
                "delete from task_metadata where branch = ?1 and worktree = ?2",
                params![branch, worktree_path],
            )
            .map_err(|error| format!("remove Worktree Session metadata: {error}"))?;
            clear_hidden_session_marker_with_conn(conn, branch)?;
            conn.execute(
                "delete from archived_worktree where branch = ?1 and worktree_path = ?2",
                params![branch, worktree_path],
            )
            .map_err(|error| format!("remove archived worktree metadata: {error}"))?;
            Ok(())
        })();
        match result {
            Ok(()) => conn
                .execute_batch("commit")
                .map_err(|error| format!("commit worktree session cleanup transaction: {error}")),
            Err(error) => {
                let _ = conn.execute_batch("rollback");
                Err(error)
            }
        }
    })?;
    Ok(())
}

fn ensure_cleanup_ownership(
    conn: &rusqlite::Connection,
    branch: &str,
    worktree_path: &str,
) -> Result<(), String> {
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

pub(crate) fn session_discovery_order(
    config: &Config,
    a: &Session,
    b: &Session,
) -> std::cmp::Ordering {
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
    let path_display = path.display().to_string();
    let incarnation = worktree_incarnation(&path);
    let agent_state = load_agent_state(repo, &branch).unwrap_or(AgentState::Idle);
    let pr = load_pr_cache_for_branch(repo, config, &branch, &path);
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
        metadata.ino()
    };
    #[cfg(not(unix))]
    let file_id = 0;
    format!("{file_id}:{modified}:{}:{target}", metadata.len())
}

pub fn write_task_metadata(
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
    Ok(())
}

pub(crate) fn archive_worktree_session(repo: &Repository, session: &Session) -> Result<(), String> {
    observability::with_writable_db(repo, |conn| {
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
            Ok(()) => conn
                .execute_batch("commit")
                .map_err(|error| format!("commit archive transaction: {error}")),
            Err(error) => {
                let _ = conn.execute_batch("rollback");
                Err(error)
            }
        }
    })
}

pub(crate) fn clear_hidden_session_marker_with_conn(
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

pub(crate) fn unarchive_worktree_session(repo: &Repository, branch: &str) -> Result<(), String> {
    observability::with_writable_db(repo, |conn| {
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
            Ok(()) => conn
                .execute_batch("commit")
                .map_err(|error| format!("commit unarchive transaction: {error}")),
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
    let conn = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| format!("open {} read-only: {error}", path.display()))?;
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

pub(crate) fn hidden_session_exists(repo: &Repository, branch: &str) -> Result<bool, String> {
    let path = observability::db_path(repo);
    if !path.exists() {
        return Ok(false);
    }
    let conn = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| format!("open {} read-only: {error}", path.display()))?;
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

pub fn append_runtime_log(repo: &Repository, message: &str) -> Result<(), String> {
    crate::observability::append_runtime_message(repo, message)
}

pub fn save_agent_state(repo: &Repository, branch: &str, state: AgentState) -> Result<(), String> {
    observability::with_writable_db(repo, |conn| {
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
    use crate::agent::PromptMode;
    use crate::config::{Checks, EscapeKey, MergeMethod};

    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{SystemTime, UNIX_EPOCH};

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
        rusqlite::Connection::open(&db).unwrap();

        assert!(!hidden_session_exists(&repo, "feature").unwrap());

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn refresh_state_preserves_runtime_pr_columns_and_unseen_comments() {
        let config = test_config();
        let mut fresh = test_session("feature", "/repo/feature");
        let mut previous = test_session("feature", "/repo/feature");
        previous.agent_state = AgentState::Running;
        previous
            .wt_columns
            .insert("ci".to_string(), "passed".to_string());
        previous.unseen_comments = true;

        fresh.preserve_refresh_state_from(previous, &config);

        assert_eq!(fresh.agent_state, AgentState::Running);
        assert_eq!(
            fresh.wt_columns.get("ci").map(String::as_str),
            Some("passed")
        );
        assert!(fresh.unseen_comments);
    }

    #[test]
    fn refresh_state_clears_pr_state_for_non_task_branches() {
        let mut config = test_config();
        config.default_base = Some("main".to_string());
        let mut fresh = test_session("main", "/repo/main");
        let mut previous = test_session("main", "/repo/main");
        previous.pr.error = Some("stale".to_string());
        previous.unseen_comments = true;

        fresh.preserve_refresh_state_from(previous, &config);

        assert!(fresh.pr.error.is_none());
        assert!(!fresh.unseen_comments);
    }

    #[test]
    fn refresh_state_clears_pr_state_for_hidden_branches() {
        let config = test_config();
        let mut fresh = test_session("feature", "/repo/feature");
        fresh.hidden = true;
        let mut previous = test_session("feature", "/repo/feature");
        previous.pr.error = Some("stale".to_string());
        previous.unseen_comments = true;

        fresh.preserve_refresh_state_from(previous, &config);

        assert!(fresh.pr.error.is_none());
        assert!(!fresh.unseen_comments);
    }

    #[test]
    fn phase_1_repository_reorder_preserves_transient_facts_when_repo_index_changes() {
        let config = test_config();
        let mut previous = test_session("feature", "/repo/feature");
        previous.repo_index = 0;
        previous.agent_state = AgentState::Running;
        previous.opencode_status = Some(OpencodeStatus::offline(
            Some("http://127.0.0.1:41000".to_string()),
            Some("session-1".to_string()),
        ));
        previous.pr.error = Some("cached PR failure".to_string());
        previous.pr.details = Some(crate::github::PrDetails::default());
        previous
            .wt_columns
            .insert("ci".to_string(), "passed".to_string());
        previous.unseen_comments = true;

        let mut fresh = test_session("feature", "/repo/feature");
        fresh.repo_index = 1;
        assert_eq!(
            fresh.identity_key(),
            previous.identity_key(),
            "presentation order must not change Worktree Session identity"
        );
        fresh.preserve_refresh_state_from(previous, &config);

        assert_eq!(fresh.agent_state, AgentState::Running);
        assert!(fresh.opencode_status.is_some());
        assert_eq!(fresh.pr.error.as_deref(), Some("cached PR failure"));
        assert!(fresh.pr.details.is_some());
        assert_eq!(
            fresh.wt_columns.get("ci").map(String::as_str),
            Some("passed")
        );
        assert!(fresh.unseen_comments);
    }

    #[test]
    fn phase_1_same_path_changed_branch_does_not_inherit_agent_session_or_pr_cache_facts() {
        let mut previous = test_session("old-feature", "/repo/feature");
        previous.agent_state = AgentState::Running;
        previous.opencode_status = Some(OpencodeStatus::offline(
            Some("http://127.0.0.1:41000".to_string()),
            Some("old-session".to_string()),
        ));
        previous.pr.error = Some("old branch PR failure".to_string());
        previous.pr.details = Some(crate::github::PrDetails::default());
        previous
            .wt_columns
            .insert("old".to_string(), "branch".to_string());
        previous.unseen_comments = true;

        let fresh = test_session("new-feature", "/repo/feature");
        assert_ne!(
            fresh.identity_key(),
            previous.identity_key(),
            "branch continuity must be part of Worktree Session identity"
        );

        assert_eq!(fresh.agent_state, AgentState::Idle);
        assert!(fresh.opencode_status.is_none());
        assert!(fresh.pr.error.is_none());
        assert!(fresh.pr.details.is_none());
        assert!(fresh.wt_columns.is_empty());
        assert!(!fresh.unseen_comments);
    }

    #[test]
    fn recreated_worktree_at_same_path_and_branch_has_new_identity() {
        let mut previous = test_session("feature", "/repo/worktree");
        previous.incarnation = "old-git-link".to_string();
        let mut recreated = test_session("feature", "/repo/worktree");
        recreated.incarnation = "new-git-link".to_string();

        assert_ne!(previous.identity_key(), recreated.identity_key());
    }

    #[test]
    fn refresh_uses_repository_root_when_different_repositories_report_same_session_identity() {
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
            },
            WorktreeSessionRepository {
                repo_index: 1,
                repo: &repo_a,
                config: &config,
                label: "a",
                key: None,
            },
        ];

        refresh_worktree_sessions(
            &repositories,
            &BTreeMap::from([(0, repo_a.root.clone()), (1, repo_b.root.clone())]),
            &mut sessions,
        )
        .unwrap();

        assert_eq!(sessions[0].repo_label, "b");
        assert_eq!(sessions[0].agent_state, AgentState::NeedsInput);
        assert_eq!(sessions[1].repo_label, "a");
        assert_eq!(sessions[1].agent_state, AgentState::Running);
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
        }];

        assert!(
            refresh_worktree_sessions(
                &repositories,
                &BTreeMap::from([(0, repo.root.clone())]),
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
        Config {
            default_agent: "ask".to_string(),
            default_base: None,
            plan_dir: "plans".to_string(),
            review_packet_dir: ".agent/review".to_string(),
            worktree_command: "wt".to_string(),
            opencode_port_base: 41_000,
            opencode_port_span: 1_000,
            opencode_shutdown_owned_servers: false,
            opencode_plan_plugin: false,
            escape_key: EscapeKey::EscEsc,
            merge_method: MergeMethod::Squash,
            icon_style: crate::config::IconStyle::Unicode,
            icon_style_configured: false,
            auto: crate::config::AutoConfig::default(),
            layout: crate::config::LayoutConfig::default(),
            checks: Checks::default(),
            worktree_columns: Vec::new(),
            tools: BTreeMap::new(),
            agent_commands: BTreeMap::new(),
            agent_prompt_modes: BTreeMap::<String, PromptMode>::new(),
            prompt_templates: BTreeMap::new(),
            user_path: PathBuf::from("/tmp/prism-test-user-config.toml"),
            repo_config_path: PathBuf::from("/tmp/prism-test-repo-config.toml"),
        }
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }

    fn write_executable(path: &Path, text: &str) {
        fs::write(path, text).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }
}
