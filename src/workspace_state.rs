use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;

use crate::config::Config;
use crate::execution::{self, WorkflowIdentity as ExecutionIdentity, WorkflowKind};
use crate::git::{self, GitStatus};
use crate::lifecycle;
use crate::repo::Repository;
use crate::{storage, worker, workspace};

#[derive(Clone, Debug)]
pub struct WorkspaceContext {
    pub repo: Option<PathBuf>,
    pub cwd: PathBuf,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct InspectRequest {
    pub include_hidden: bool,
    pub include_terminal: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct WorkspaceSnapshot {
    pub schema_version: u32,
    pub observed_unix_ms: i64,
    pub daemon: worker::DaemonHealth,
    pub repositories: Vec<RepositorySnapshot>,
    pub totals: WorkspaceTotals,
    pub warnings: Vec<Diagnostic>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RepositorySnapshot {
    pub root: PathBuf,
    pub label: String,
    pub shortcut: Option<char>,
    pub worktrees: Vec<WorktreeSnapshot>,
    pub workflows: Vec<WorkflowSnapshot>,
    pub totals: RepositoryTotals,
}

#[derive(Clone, Debug, Serialize)]
pub struct WorktreeSnapshot {
    pub identity: WorktreeIdentity,
    pub branch: BranchState,
    pub git: GitStatus,
    pub hidden: bool,
    pub agent: AgentStatus,
    pub pull_request: Option<CachedPullRequest>,
    pub workflows: Vec<WorkflowIdentity>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct WorktreeIdentity {
    pub path: PathBuf,
    pub display: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "state", content = "name", rename_all = "snake_case")]
pub enum BranchState {
    Named(String),
    Detached,
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct AgentStatus {
    pub state: Option<String>,
    pub updated_unix_ms: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CachedPullRequest {
    pub number: i64,
    pub title: String,
    pub url: String,
    pub state: String,
    pub mergeability: Option<String>,
    pub ci: Option<String>,
    pub observed_unix_ms: i64,
    pub age_ms: i64,
    pub provenance: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub struct WorkflowSnapshot {
    pub identity: WorkflowIdentity,
    pub owner: Option<WorkflowIdentity>,
    pub worktree: WorktreeIdentity,
    pub lifecycle: String,
    pub pause_requested: bool,
    pub dispatch: DispatchSnapshot,
    pub current_step: Option<StepSummary>,
    pub progress: Progress,
    pub available_controls: AvailableControls,
    pub updated_unix_ms: i64,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct WorkflowIdentity {
    pub repository: PathBuf,
    pub kind: String,
    pub run_id: String,
    pub display_id: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct DispatchSnapshot {
    pub state: Option<String>,
    pub daemon_instance_id: Option<String>,
    pub worker_id: Option<String>,
    pub lease_expires_unix_ms: Option<i64>,
    pub heartbeat_unix_ms: Option<i64>,
    pub interruption_generation: i64,
    pub updated_unix_ms: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct StepSummary {
    pub label: String,
    pub state: String,
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct Progress {
    pub completed: usize,
    pub total: usize,
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct AvailableControls {
    pub pause: bool,
    pub resume: bool,
    pub stop: bool,
    pub recover: bool,
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct WorkspaceTotals {
    pub repositories: usize,
    pub worktrees: usize,
    pub workflows: usize,
    pub attention: usize,
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct RepositoryTotals {
    pub worktrees: usize,
    pub workflows: usize,
    pub attention: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct Diagnostic {
    pub scope: String,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlAction {
    Pause,
    Resume,
    Stop,
    Recover,
}

#[derive(Clone, Debug)]
pub struct ControlRequest {
    pub action: ControlAction,
    pub selector: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ControlReceipt {
    pub workflow: WorkflowIdentity,
    pub state: String,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug)]
struct RepoSource {
    repo: Repository,
    shortcut: Option<char>,
}

pub struct WorkspaceState {
    context: WorkspaceContext,
    sources: Vec<RepoSource>,
    discovery_diagnostics: Vec<Diagnostic>,
}

impl WorkspaceState {
    pub fn open(context: WorkspaceContext) -> Result<Self, String> {
        let mut sources = Vec::new();
        let mut diagnostics = Vec::new();
        if let Some(path) = context.repo.as_deref() {
            sources.push(RepoSource {
                repo: Repository::discover(Some(path))?,
                shortcut: None,
            });
        } else {
            for entry in workspace::load_entries()? {
                match Repository::discover(Some(&entry.root)) {
                    Ok(repo) => sources.push(RepoSource {
                        repo,
                        shortcut: entry.key,
                    }),
                    Err(error) => diagnostics.push(Diagnostic {
                        scope: entry.root.display().to_string(),
                        message: error,
                    }),
                }
            }
            if sources.is_empty()
                && let Ok(repo) = Repository::discover(Some(&context.cwd))
            {
                sources.push(RepoSource {
                    repo,
                    shortcut: None,
                });
            }
        }
        if sources.is_empty() {
            return Err("no repository can be inspected".to_string());
        }
        Ok(Self {
            context,
            sources,
            discovery_diagnostics: diagnostics,
        })
    }

    pub fn inspect(&self, request: InspectRequest) -> Result<WorkspaceSnapshot, String> {
        let observed = execution::now_ms();
        let mut warnings = self.discovery_diagnostics.clone();
        let daemon = match worker::probe_health() {
            Ok(health) => health,
            Err(error) => {
                warnings.push(Diagnostic {
                    scope: "daemon".to_string(),
                    message: error,
                });
                worker::DaemonHealth::stopped()
            }
        };
        let mut repositories = Vec::new();
        for source in &self.sources {
            match inspect_repository(source, request, observed) {
                Ok((repository, mut repository_warnings)) => {
                    repositories.push(repository);
                    warnings.append(&mut repository_warnings);
                }
                Err(error) => warnings.push(Diagnostic {
                    scope: source.repo.root.display().to_string(),
                    message: error,
                }),
            }
        }
        if repositories.is_empty() {
            return Err("no repository can be inspected".to_string());
        }
        assign_display_ids(&mut repositories);
        let totals = WorkspaceTotals {
            repositories: repositories.len(),
            worktrees: repositories.iter().map(|repo| repo.worktrees.len()).sum(),
            workflows: repositories.iter().map(|repo| repo.workflows.len()).sum(),
            attention: repositories.iter().map(|repo| repo.totals.attention).sum(),
        };
        Ok(WorkspaceSnapshot {
            schema_version: 1,
            observed_unix_ms: observed,
            daemon,
            repositories,
            totals,
            warnings,
        })
    }

    pub fn resolve_subject(
        &self,
        snapshot: &WorkspaceSnapshot,
        selector: Option<&str>,
    ) -> Result<Subject, String> {
        let explicit_selector = selector.is_some();
        let selector = selector
            .map(str::to_string)
            .unwrap_or_else(|| self.context.cwd.display().to_string());
        let mut matches = Vec::new();
        for (repo_index, repo) in snapshot.repositories.iter().enumerate() {
            if selector == repo.root.display().to_string()
                || selector.strip_prefix("repo:") == Some(repo.label.as_str())
            {
                matches.push(Subject::Repository(repo_index));
            }
            for (worktree_index, worktree) in repo.worktrees.iter().enumerate() {
                let branch = branch_label(&worktree.branch);
                if selector == worktree.identity.path.display().to_string()
                    || selector.strip_prefix("wt:") == Some(branch)
                    || (!explicit_selector
                        && path_contains(&worktree.identity.path, Path::new(&selector)))
                {
                    matches.push(Subject::Worktree(repo_index, worktree_index));
                }
            }
            for (workflow_index, workflow) in repo.workflows.iter().enumerate() {
                if workflow_selector_matches(&selector, &workflow.identity) {
                    matches.push(Subject::Workflow(repo_index, workflow_index));
                }
            }
        }
        matches.dedup();
        if matches
            .iter()
            .any(|subject| matches!(subject, Subject::Worktree(_, _)))
        {
            matches.retain(|subject| !matches!(subject, Subject::Repository(_)));
        }
        match matches.len() {
            0 => Err(format!("selector not found: {selector}")),
            1 => Ok(matches.remove(0)),
            _ => Err(format!(
                "ambiguous selector {selector}; matches {}",
                subject_candidates(snapshot, &matches).join(", ")
            )),
        }
    }

    pub fn control(&self, request: ControlRequest) -> Result<ControlReceipt, String> {
        let snapshot = self.inspect(InspectRequest {
            include_hidden: true,
            include_terminal: true,
        })?;
        let target =
            self.resolve_workflow(&snapshot, request.selector.as_deref(), request.action)?;
        if let Some(owner) = &target.owner {
            return Err(format!(
                "plan run {} is owned by {}; control {} instead",
                target.identity.display_id, owner.display_id, owner.display_id
            ));
        }
        let source = self
            .sources
            .iter()
            .find(|source| source.repo.root == target.identity.repository)
            .ok_or_else(|| "workflow repository is no longer available".to_string())?;
        let path = source.repo.prism_dir().join("prism.db");
        let mut conn = storage::open_writable(&path).map_err(|error| error.to_string())?;
        let (state, mut warnings) = if request.action == ControlAction::Recover {
            if target.dispatch.state.as_deref() != Some("recovery_pending") {
                return Err("workflow does not require recovery".to_string());
            }
            execution::apply_recovery_decision(
                &mut conn,
                &[(
                    execution_identity(&target.identity)?,
                    target.dispatch.interruption_generation,
                    true,
                )],
            )?;
            ("queued".to_string(), Vec::new())
        } else {
            apply_control_transaction(&mut conn, target, request.action)?
        };
        if matches!(
            request.action,
            ControlAction::Resume | ControlAction::Recover
        ) {
            if let Err(error) = worker::ensure_running().and_then(|_| worker::wake()) {
                warnings.push(format!(
                    "control committed, but daemon notification failed: {error}"
                ));
            }
        } else if request.action == ControlAction::Pause
            && let Err(error) = worker::wake()
        {
            warnings.push(format!(
                "control committed, but daemon wake failed: {error}"
            ));
        }
        Ok(ControlReceipt {
            workflow: target.identity.clone(),
            state,
            warnings,
        })
    }

    fn resolve_workflow<'a>(
        &self,
        snapshot: &'a WorkspaceSnapshot,
        selector: Option<&str>,
        action: ControlAction,
    ) -> Result<&'a WorkflowSnapshot, String> {
        if let Some(selector) = selector {
            return match self.resolve_subject(snapshot, Some(selector))? {
                Subject::Workflow(repo, workflow) => validate_explicit_control(
                    &snapshot.repositories[repo].workflows[workflow],
                    action,
                ),
                Subject::Repository(repo) => {
                    eligible_owner(&snapshot.repositories[repo].workflows, action)
                }
                Subject::Worktree(repo, worktree) => {
                    let path = &snapshot.repositories[repo].worktrees[worktree]
                        .identity
                        .path;
                    let candidates = snapshot.repositories[repo]
                        .workflows
                        .iter()
                        .filter(|workflow| &workflow.worktree.path == path)
                        .collect::<Vec<_>>();
                    eligible_refs(&candidates, action)
                }
            };
        }
        match self.resolve_subject(snapshot, None)? {
            Subject::Workflow(repo, workflow) => {
                Ok(&snapshot.repositories[repo].workflows[workflow])
            }
            Subject::Repository(repo) => {
                eligible_owner(&snapshot.repositories[repo].workflows, action)
            }
            Subject::Worktree(repo, worktree) => {
                let path = &snapshot.repositories[repo].worktrees[worktree]
                    .identity
                    .path;
                let candidates = snapshot.repositories[repo]
                    .workflows
                    .iter()
                    .filter(|workflow| &workflow.worktree.path == path)
                    .collect::<Vec<_>>();
                eligible_refs(&candidates, action)
            }
        }
    }
}

pub(crate) fn control_repository_workflow(
    repo: &Repository,
    action: ControlAction,
    kind: &str,
    run_id: &str,
) -> Result<ControlReceipt, String> {
    WorkspaceState::open(WorkspaceContext {
        repo: Some(repo.root.clone()),
        cwd: repo.root.clone(),
    })?
    .control(ControlRequest {
        action,
        selector: Some(format!("{kind}:{run_id}")),
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Subject {
    Repository(usize),
    Worktree(usize, usize),
    Workflow(usize, usize),
}

fn inspect_repository(
    source: &RepoSource,
    request: InspectRequest,
    observed: i64,
) -> Result<(RepositorySnapshot, Vec<Diagnostic>), String> {
    let config = Config::load(&source.repo);
    let inventory = lifecycle::list_worktrees(&source.repo, &config)?;
    let db_path = source.repo.prism_dir().join("prism.db");
    let mut warnings = Vec::new();
    let conn = if db_path.exists() {
        match storage::open_readonly(&db_path) {
            Ok(conn) => Some(conn),
            Err(error) => {
                warnings.push(repository_diagnostic(source, error.to_string()));
                None
            }
        }
    } else {
        None
    };
    let hidden = read_projection(&conn, source, &mut warnings, load_hidden).unwrap_or_default();
    let mut workflows = read_projection(&conn, source, &mut warnings, |conn| {
        load_workflows(conn, &source.repo.root, request.include_terminal)
    })
    .unwrap_or_default();
    let mut worktrees = Vec::new();
    for entry in inventory {
        let is_hidden = hidden.contains(&entry.branch);
        if is_hidden && !request.include_hidden {
            continue;
        }
        let identity = WorktreeIdentity {
            display: if entry.branch == "(detached)" {
                entry
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("worktree")
            } else {
                &entry.branch
            }
            .to_string(),
            path: absolute_path(&entry.path),
        };
        let branch = if entry.branch == "(detached)" {
            BranchState::Detached
        } else {
            BranchState::Named(entry.branch.clone())
        };
        let agent = read_projection(&conn, source, &mut warnings, |conn| {
            load_agent(conn, &entry.branch)
        })
        .flatten()
        .unwrap_or_default();
        let pull_request = read_projection(&conn, source, &mut warnings, |conn| {
            load_pr(conn, &entry.branch, observed)
        })
        .flatten();
        let associated = workflows
            .iter()
            .filter(|workflow| paths_equal(&workflow.worktree.path, &identity.path))
            .map(|workflow| workflow.identity.clone())
            .collect();
        worktrees.push(WorktreeSnapshot {
            git: git::inspect_status(&identity.path, &config),
            identity,
            branch,
            hidden: is_hidden,
            agent,
            pull_request,
            workflows: associated,
        });
    }
    worktrees.sort_by(|left, right| left.identity.path.cmp(&right.identity.path));
    workflows.sort_by(|left, right| {
        right
            .updated_unix_ms
            .cmp(&left.updated_unix_ms)
            .then_with(|| left.identity.kind.cmp(&right.identity.kind))
            .then_with(|| left.identity.run_id.cmp(&right.identity.run_id))
    });
    let attention = workflows
        .iter()
        .filter(|workflow| {
            matches!(workflow.lifecycle.as_str(), "failed")
                || workflow.dispatch.state.as_deref() == Some("recovery_pending")
                || workflow.pause_requested
        })
        .count();
    Ok((
        RepositorySnapshot {
            root: absolute_path(&source.repo.root),
            label: workspace::label_for_root(&source.repo.root),
            shortcut: source.shortcut,
            totals: RepositoryTotals {
                worktrees: worktrees.len(),
                workflows: workflows.len(),
                attention,
            },
            worktrees,
            workflows,
        },
        warnings,
    ))
}

fn read_projection<T>(
    conn: &Option<Connection>,
    source: &RepoSource,
    warnings: &mut Vec<Diagnostic>,
    read: impl FnOnce(&Connection) -> Result<T, String>,
) -> Option<T> {
    let conn = conn.as_ref()?;
    match read(conn) {
        Ok(value) => Some(value),
        Err(error) => {
            warnings.push(repository_diagnostic(source, error));
            None
        }
    }
}

fn repository_diagnostic(source: &RepoSource, message: String) -> Diagnostic {
    Diagnostic {
        scope: source.repo.root.display().to_string(),
        message,
    }
}

fn load_hidden(conn: &Connection) -> Result<BTreeSet<String>, String> {
    if !table_exists(conn, "hidden_session")? {
        return Ok(BTreeSet::new());
    }
    let mut statement = conn
        .prepare("select branch from hidden_session")
        .map_err(|error| error.to_string())?;
    statement
        .query_map([], |row| row.get(0))
        .map_err(|error| error.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|error| error.to_string())
}

fn load_agent(conn: &Connection, branch: &str) -> Result<Option<AgentStatus>, String> {
    if !table_exists(conn, "agent_state")? {
        return Ok(None);
    }
    conn.query_row(
        "select state, updated_unix_ms from agent_state where branch = ?1",
        [branch],
        |row| {
            Ok(AgentStatus {
                state: row.get(0)?,
                updated_unix_ms: row.get(1)?,
            })
        },
    )
    .optional()
    .map_err(|error| error.to_string())
}

fn load_pr(
    conn: &Connection,
    branch: &str,
    observed: i64,
) -> Result<Option<CachedPullRequest>, String> {
    if !table_exists(conn, "pr_cache")? {
        return Ok(None);
    }
    conn.query_row(
        "select number, title, url, state, review_decision, check_status, refreshed_unix_ms from pr_cache where branch = ?1",
        [branch],
        |row| {
            let refreshed = row.get::<_, i64>(6)?.saturating_mul(1_000);
            Ok(CachedPullRequest {
                number: row.get(0)?, title: row.get(1)?, url: row.get(2)?, state: row.get(3)?,
                mergeability: row.get::<_, Option<String>>(4)?.filter(|value| !value.is_empty()),
                ci: row.get::<_, Option<String>>(5)?.filter(|value| !value.is_empty()),
                observed_unix_ms: refreshed, age_ms: observed.saturating_sub(refreshed), provenance: "sqlite_cache",
            })
        },
    ).optional().map_err(|error| error.to_string())
}

fn load_workflows(
    conn: &Connection,
    repo_root: &Path,
    include_terminal: bool,
) -> Result<Vec<WorkflowSnapshot>, String> {
    if !table_exists(conn, "workflow_execution")? {
        return Ok(Vec::new());
    }
    let terminal_filter = if include_terminal {
        ""
    } else {
        "and (r.status in ('queued','running','paused','failed') or e.dispatch_state in ('queued','claimed','recovery_pending','paused'))"
    };
    let query = format!(
        "select 'auto', r.id, r.worktree_path, r.status, r.pause_requested, r.updated_unix_ms,
                e.dispatch_state, e.daemon_instance_id, e.worker_id, e.lease_expires_unix_ms,
                e.heartbeat_unix_ms, e.interruption_generation, e.updated_unix_ms,
                (select s.step_key from auto_step_run s where s.run_id = r.id order by s.sequence desc limit 1),
                (select s.status from auto_step_run s where s.run_id = r.id order by s.sequence desc limit 1),
                (select count(*) from auto_step_run s where s.run_id = r.id and s.status = 'done'),
                (select count(*) from auto_step_run s where s.run_id = r.id)
         from auto_run r left join workflow_execution e on e.workflow_kind = 'auto' and e.run_id = r.id
         where r.repo_root = ?1 and r.archived_unix_ms is null {terminal_filter}
         union all
         select 'plan', r.id, r.scope_path, r.status, r.pause_requested, r.updated_unix_ms,
                e.dispatch_state, e.daemon_instance_id, e.worker_id, e.lease_expires_unix_ms,
                e.heartbeat_unix_ms, e.interruption_generation, e.updated_unix_ms,
                (select r.step_name || ' ' || s.step || '/' || r.total_steps from plan_step_run s where s.run_id = r.id order by case s.status when 'running' then 0 when 'starting' then 1 when 'queued' then 2 else 3 end, s.step limit 1),
                (select s.status from plan_step_run s where s.run_id = r.id order by case s.status when 'running' then 0 when 'starting' then 1 when 'queued' then 2 else 3 end, s.step limit 1),
                (select count(*) from plan_step_run s where s.run_id = r.id and s.status in ('done','skipped')),
                r.total_steps
         from plan_run r left join workflow_execution e on e.workflow_kind = 'plan' and e.run_id = r.id
         where r.repo_root = ?1 and r.archived_unix_ms is null {terminal_filter}"
    );
    let mut statement = conn
        .prepare(&query)
        .map_err(|error| format!("prepare workflow projection: {error}"))?;
    let rows = statement
        .query_map([repo_root.display().to_string()], |row| {
            let kind: String = row.get(0)?;
            let lifecycle: String = row.get(3)?;
            let pause_requested = row.get::<_, i64>(4)? != 0;
            let dispatch_state: Option<String> = row.get(6)?;
            let ownerless = true;
            Ok(WorkflowSnapshot {
                identity: WorkflowIdentity {
                    repository: absolute_path(repo_root),
                    kind: kind.clone(),
                    run_id: row.get(1)?,
                    display_id: String::new(),
                },
                owner: None,
                worktree: WorktreeIdentity {
                    path: absolute_path(Path::new(&row.get::<_, String>(2)?)),
                    display: String::new(),
                },
                lifecycle: lifecycle.clone(),
                pause_requested,
                dispatch: DispatchSnapshot {
                    state: dispatch_state.clone(),
                    daemon_instance_id: row.get(7)?,
                    worker_id: row.get(8)?,
                    lease_expires_unix_ms: row.get(9)?,
                    heartbeat_unix_ms: row.get(10)?,
                    interruption_generation: row.get::<_, Option<i64>>(11)?.unwrap_or(0),
                    updated_unix_ms: row.get(12)?,
                },
                current_step: row.get::<_, Option<String>>(13)?.map(|label| StepSummary {
                    label,
                    state: row
                        .get::<_, Option<String>>(14)
                        .ok()
                        .flatten()
                        .unwrap_or_else(|| "unknown".to_string()),
                }),
                progress: Progress {
                    completed: row.get::<_, i64>(15)?.max(0) as usize,
                    total: row.get::<_, i64>(16)?.max(0) as usize,
                },
                available_controls: controls_for(
                    &lifecycle,
                    pause_requested,
                    dispatch_state.as_deref(),
                    ownerless,
                ),
                updated_unix_ms: row.get(5)?,
            })
        })
        .map_err(|error| format!("query workflow projection: {error}"))?;
    let mut workflows = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read workflow projection: {error}"))?;
    drop(statement);
    let owners = linked_plan_owners(conn)?;
    let identities = workflows
        .iter()
        .map(|workflow| {
            (
                (
                    workflow.identity.kind.clone(),
                    workflow.identity.run_id.clone(),
                ),
                workflow.identity.clone(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    for workflow in &mut workflows {
        if workflow.identity.kind == "plan"
            && let Some(auto_id) = owners.get(&workflow.identity.run_id)
            && let Some(owner) = identities.get(&("auto".to_string(), auto_id.clone()))
        {
            workflow.owner = Some(owner.clone());
            workflow.available_controls = AvailableControls::default();
        }
    }
    Ok(workflows)
}

fn linked_plan_owners(conn: &Connection) -> Result<BTreeMap<String, String>, String> {
    let mut statement = conn.prepare("select distinct plan_run_id, run_id from auto_step_run where plan_run_id is not null order by plan_run_id, run_id").map_err(|error| format!("prepare linked plan ownership: {error}"))?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| format!("query linked plan ownership: {error}"))?;
    let mut owners = BTreeMap::new();
    for row in rows {
        let (plan, auto) = row.map_err(|error| format!("read linked plan ownership: {error}"))?;
        owners.entry(plan).or_insert(auto);
    }
    Ok(owners)
}

fn controls_for(
    lifecycle: &str,
    pause_requested: bool,
    dispatch: Option<&str>,
    ownerless: bool,
) -> AvailableControls {
    let terminal = matches!(lifecycle, "done" | "failed" | "aborted");
    AvailableControls {
        pause: ownerless && !terminal && !pause_requested && dispatch != Some("recovery_pending"),
        resume: ownerless
            && !terminal
            && (pause_requested || lifecycle == "paused")
            && dispatch != Some("recovery_pending"),
        stop: ownerless && !terminal,
        recover: ownerless && dispatch == Some("recovery_pending"),
    }
}

fn assign_display_ids(repositories: &mut [RepositorySnapshot]) {
    let mut counts = BTreeMap::<String, usize>::new();
    for workflow in repositories.iter().flat_map(|repo| &repo.workflows) {
        let prefix = short_id(&workflow.identity.kind, &workflow.identity.run_id);
        *counts.entry(prefix).or_default() += 1;
    }
    for repository in repositories {
        for workflow in &mut repository.workflows {
            let short = short_id(&workflow.identity.kind, &workflow.identity.run_id);
            workflow.identity.display_id = if counts[&short] == 1 {
                short
            } else {
                format!(
                    "{}:{}",
                    kind_prefix(&workflow.identity.kind),
                    workflow.identity.run_id
                )
            };
            workflow.worktree.display = workflow
                .worktree
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("worktree")
                .to_string();
        }
        for worktree in &mut repository.worktrees {
            for identity in &mut worktree.workflows {
                if let Some(workflow) = repository.workflows.iter().find(|workflow| {
                    workflow.identity.kind == identity.kind
                        && workflow.identity.run_id == identity.run_id
                }) {
                    identity
                        .display_id
                        .clone_from(&workflow.identity.display_id);
                }
            }
        }
        let display_ids = repository
            .workflows
            .iter()
            .map(|workflow| {
                (
                    (
                        workflow.identity.kind.clone(),
                        workflow.identity.run_id.clone(),
                    ),
                    workflow.identity.display_id.clone(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        for workflow in &mut repository.workflows {
            if let Some(owner) = &mut workflow.owner
                && let Some(display_id) =
                    display_ids.get(&(owner.kind.clone(), owner.run_id.clone()))
            {
                owner.display_id.clone_from(display_id);
            }
        }
    }
}

fn apply_control_transaction(
    conn: &mut Connection,
    workflow: &WorkflowSnapshot,
    action: ControlAction,
) -> Result<(String, Vec<String>), String> {
    if workflow.dispatch.state.as_deref() == Some("recovery_pending") {
        return Err("workflow was interrupted; use recover instead".to_string());
    }
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| format!("begin workflow control: {error}"))?;
    let now = execution::now_ms();
    validate_control_snapshot(&tx, workflow)?;
    let (run_table, step_table, active_states) = if workflow.identity.kind == "auto" {
        (
            "auto_run",
            "auto_step_run",
            "('starting','running','waiting')",
        )
    } else {
        ("plan_run", "plan_step_run", "('starting','running')")
    };
    let active: i64 = tx
        .query_row(
            &format!(
                "select count(*) from {step_table} where run_id = ?1 and status in {active_states}"
            ),
            [&workflow.identity.run_id],
            |row| row.get(0),
        )
        .map_err(|error| format!("inspect active workflow steps: {error}"))?;
    let mut warnings = Vec::new();
    let state = match action {
        ControlAction::Pause => {
            if workflow.pause_requested || workflow.lifecycle == "paused" {
                return Err("workflow is already paused".to_string());
            }
            let state = if active > 0 {
                workflow.lifecycle.as_str()
            } else {
                "paused"
            };
            let changed = tx.execute(&format!("update {run_table} set pause_requested = 1, status = ?1, updated_unix_ms = ?2 where id = ?3 and status not in ('done','aborted')"), params![state, now, workflow.identity.run_id]).map_err(|error| format!("pause workflow: {error}"))?;
            if changed != 1 {
                return Err("workflow cannot be paused from its current state".to_string());
            }
            if active == 0 {
                set_dispatch(&tx, workflow, "paused", now)?;
            }
            if workflow.identity.kind == "auto" {
                tx.execute("update plan_run set pause_requested = 1, status = case when exists(select 1 from plan_step_run s where s.run_id = plan_run.id and s.status in ('starting','running')) then status else 'paused' end, updated_unix_ms = ?1 where id in (select plan_run_id from auto_step_run where run_id = ?2 and plan_run_id is not null and status in ('queued','starting','running','waiting')) and status not in ('done','failed','aborted')", params![now, workflow.identity.run_id]).map_err(|error| format!("pause linked plan: {error}"))?;
            }
            if active > 0 {
                "pause_requested".to_string()
            } else {
                "paused".to_string()
            }
        }
        ControlAction::Resume => {
            if !workflow.pause_requested && workflow.lifecycle != "paused" {
                return Err("workflow is not paused".to_string());
            }
            let live_execution: i64 = tx
                .query_row(
                    &format!(
                        "select count(*) from {step_table}
                         where run_id = ?1 and status in ('starting','running')"
                    ),
                    [&workflow.identity.run_id],
                    |row| row.get(0),
                )
                .map_err(|error| format!("inspect live workflow steps: {error}"))?;
            let resumed_state = if live_execution > 0 {
                "running"
            } else {
                "queued"
            };
            let changed = tx.execute(&format!("update {run_table} set pause_requested = 0, status = ?1, updated_unix_ms = ?2 where id = ?3 and (pause_requested = 1 or status = 'paused')"), params![resumed_state, now, workflow.identity.run_id]).map_err(|error| format!("resume workflow: {error}"))?;
            if changed != 1 {
                return Err("workflow changed while applying resume".to_string());
            }
            if workflow.identity.kind == "auto" {
                tx.execute(
                    "update plan_run set pause_requested = 0,
                       status = case when exists(
                         select 1 from plan_step_run s where s.run_id = plan_run.id
                           and s.status in ('starting','running')
                       ) then 'running' else 'queued' end,
                       updated_unix_ms = ?1
                     where id in (select plan_run_id from auto_step_run
                       where run_id = ?2 and plan_run_id is not null
                         and status in ('queued','starting','running','waiting'))
                       and status = 'paused'",
                    params![now, workflow.identity.run_id],
                )
                .map_err(|error| format!("resume linked plan: {error}"))?;
            }
            if live_execution == 0 {
                enqueue_dispatch(&tx, workflow, now)?;
            }
            resumed_state.to_string()
        }
        ControlAction::Stop => {
            let cancellation = recorded_cancellation(&tx, workflow)?;
            tx.execute(&format!("update {step_table} set status = 'aborted', finished_unix_ms = ?1, error = coalesce(error, 'aborted') where run_id = ?2 and status in ('queued','starting','running','waiting')"), params![now, workflow.identity.run_id]).map_err(|error| format!("abort workflow steps: {error}"))?;
            let changed = tx.execute(&format!("update {run_table} set pause_requested = 0, status = 'aborted', updated_unix_ms = ?1 where id = ?2 and status not in ('done','aborted')"), params![now, workflow.identity.run_id]).map_err(|error| format!("stop workflow: {error}"))?;
            if changed != 1 {
                return Err("workflow cannot be stopped from its current state".to_string());
            }
            if workflow.identity.kind == "auto" {
                tx.execute(
                    "update plan_step_run set status = 'aborted', finished_unix_ms = ?1,
                       error = coalesce(error, 'aborted')
                     where run_id in (select plan_run_id from auto_step_run
                       where run_id = ?2 and plan_run_id is not null)
                       and status in ('queued','starting','running')",
                    params![now, workflow.identity.run_id],
                )
                .map_err(|error| format!("abort linked plan steps: {error}"))?;
                tx.execute(
                    "update plan_run set pause_requested = 0, status = 'aborted',
                       updated_unix_ms = ?1
                     where id in (select plan_run_id from auto_step_run
                       where run_id = ?2 and plan_run_id is not null)
                       and status not in ('done','failed','aborted')",
                    params![now, workflow.identity.run_id],
                )
                .map_err(|error| format!("abort linked plan: {error}"))?;
            }
            set_dispatch(&tx, workflow, "terminal", now)?;
            tx.commit()
                .map_err(|error| format!("commit workflow control: {error}"))?;
            cancel_recorded_work(cancellation, &mut warnings);
            return Ok(("aborted".to_string(), warnings));
        }
        ControlAction::Recover => unreachable!(),
    };
    tx.commit()
        .map_err(|error| format!("commit workflow control: {error}"))?;
    Ok((state, warnings))
}

fn set_dispatch(
    conn: &Connection,
    workflow: &WorkflowSnapshot,
    state: &str,
    now: i64,
) -> Result<(), String> {
    conn.execute("update workflow_execution set dispatch_state = ?1, worker_id = null, daemon_instance_id = null, lease_expires_unix_ms = null, executor_pid = null, executor_process_identity = null, requeue_requested = 0, fencing_token = fencing_token + 1, updated_unix_ms = ?2 where workflow_kind = ?3 and run_id = ?4", params![state, now, workflow.identity.kind, workflow.identity.run_id]).map_err(|error| format!("update workflow dispatch: {error}"))?;
    Ok(())
}

fn enqueue_dispatch(
    conn: &Connection,
    workflow: &WorkflowSnapshot,
    now: i64,
) -> Result<(), String> {
    conn.execute("insert into workflow_execution (workflow_kind, run_id, dispatch_state, fencing_token, interruption_generation, created_unix_ms, updated_unix_ms) values (?1, ?2, 'queued', 0, 0, ?3, ?3) on conflict(workflow_kind, run_id) do update set dispatch_state = case when workflow_execution.dispatch_state = 'claimed' then 'claimed' else 'queued' end, requeue_requested = case when workflow_execution.dispatch_state = 'claimed' then 1 else 0 end, worker_id = case when workflow_execution.dispatch_state = 'claimed' then worker_id else null end, daemon_instance_id = case when workflow_execution.dispatch_state = 'claimed' then daemon_instance_id else null end, updated_unix_ms = excluded.updated_unix_ms", params![workflow.identity.kind, workflow.identity.run_id, now]).map_err(|error| format!("queue workflow: {error}"))?;
    Ok(())
}

fn eligible_owner(
    workflows: &[WorkflowSnapshot],
    action: ControlAction,
) -> Result<&WorkflowSnapshot, String> {
    eligible_refs(&workflows.iter().collect::<Vec<_>>(), action)
}

fn validate_explicit_control(
    workflow: &WorkflowSnapshot,
    action: ControlAction,
) -> Result<&WorkflowSnapshot, String> {
    let available = match action {
        ControlAction::Pause => workflow.available_controls.pause,
        ControlAction::Resume => workflow.available_controls.resume,
        ControlAction::Stop => workflow.available_controls.stop,
        ControlAction::Recover => workflow.available_controls.recover,
    };
    if available {
        Ok(workflow)
    } else if workflow.dispatch.state.as_deref() == Some("recovery_pending")
        && action != ControlAction::Recover
    {
        Err("workflow was interrupted; use recover instead".to_string())
    } else {
        Err(format!(
            "{} is not available for workflow {}",
            control_action_label(action),
            workflow.identity.display_id
        ))
    }
}

fn validate_control_snapshot(conn: &Connection, workflow: &WorkflowSnapshot) -> Result<(), String> {
    let run_table = if workflow.identity.kind == "auto" {
        "auto_run"
    } else {
        "plan_run"
    };
    let current = conn
        .query_row(
            &format!(
                "select r.status, r.pause_requested, r.updated_unix_ms,
                        e.dispatch_state, coalesce(e.interruption_generation, 0)
                 from {run_table} r left join workflow_execution e
                   on e.workflow_kind = ?1 and e.run_id = r.id
                 where r.id = ?2"
            ),
            params![workflow.identity.kind, workflow.identity.run_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)? != 0,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()
        .map_err(|error| format!("validate workflow control: {error}"))?;
    let expected = (
        workflow.lifecycle.clone(),
        workflow.pause_requested,
        workflow.updated_unix_ms,
        workflow.dispatch.state.clone(),
        workflow.dispatch.interruption_generation,
    );
    if current.as_ref() != Some(&expected) {
        return Err("workflow changed while applying control; inspect it again".to_string());
    }
    Ok(())
}

struct RecordedCancellation {
    processes: Vec<(u32, Option<u64>)>,
    sessions: Vec<crate::harness::SessionRef>,
}

fn recorded_cancellation(
    conn: &Connection,
    workflow: &WorkflowSnapshot,
) -> Result<RecordedCancellation, String> {
    let query = if workflow.identity.kind == "auto" {
        "select execution_process_id, execution_process_start_time_ticks
         from auto_step_run where run_id = ?1 and execution_process_id is not null
         union
         select execution_process_id, execution_process_start_time_ticks
         from plan_step_run where run_id in (
           select plan_run_id from auto_step_run where run_id = ?1 and plan_run_id is not null
         ) and execution_process_id is not null"
    } else {
        "select execution_process_id, execution_process_start_time_ticks
         from plan_step_run where run_id = ?1 and execution_process_id is not null"
    };
    let mut statement = conn
        .prepare(query)
        .map_err(|error| format!("prepare workflow process cancellation: {error}"))?;
    let rows = statement
        .query_map([&workflow.identity.run_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?))
        })
        .map_err(|error| format!("query workflow process cancellation: {error}"))?;
    let mut processes = Vec::new();
    for row in rows {
        let (pid, start) = row.map_err(|error| format!("read workflow process: {error}"))?;
        if let Ok(pid) = u32::try_from(pid) {
            processes.push((pid, start.and_then(|value| u64::try_from(value).ok())));
        }
    }
    let query = if workflow.identity.kind == "auto" {
        "select session_adapter_id, session_endpoint, session_id
         from auto_step_run where run_id = ?1 and session_id is not null
         union
         select session_adapter_id, session_endpoint, session_id
         from plan_step_run where run_id in (
           select plan_run_id from auto_step_run where run_id = ?1 and plan_run_id is not null
         ) and session_id is not null"
    } else {
        "select session_adapter_id, session_endpoint, session_id
         from plan_step_run where run_id = ?1 and session_id is not null"
    };
    let mut statement = conn
        .prepare(query)
        .map_err(|error| format!("prepare workflow session cancellation: {error}"))?;
    let rows = statement
        .query_map([&workflow.identity.run_id], |row| {
            Ok(crate::harness::SessionRef {
                adapter_id: row.get(0)?,
                endpoint: row.get(1)?,
                id: row.get(2)?,
            })
        })
        .map_err(|error| format!("query workflow session cancellation: {error}"))?;
    let sessions = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read workflow session: {error}"))?;
    Ok(RecordedCancellation {
        processes,
        sessions,
    })
}

fn cancel_recorded_work(cancellation: RecordedCancellation, warnings: &mut Vec<String>) {
    for session in cancellation.sessions {
        if let Err(error) = crate::harness::cancel_native_session(&session) {
            warnings.push(format!(
                "workflow stopped, but native session cancellation failed: {error}"
            ));
        }
    }
    for (pid, start) in cancellation.processes {
        if let Err(error) = crate::harness::terminate_process(pid, start) {
            warnings.push(format!(
                "workflow stopped, but external process {pid} cancellation failed: {error}"
            ));
        }
    }
}

fn control_action_label(action: ControlAction) -> &'static str {
    match action {
        ControlAction::Pause => "pause",
        ControlAction::Resume => "resume",
        ControlAction::Stop => "stop",
        ControlAction::Recover => "recover",
    }
}

fn eligible_refs<'a>(
    workflows: &[&'a WorkflowSnapshot],
    action: ControlAction,
) -> Result<&'a WorkflowSnapshot, String> {
    let candidates = workflows
        .iter()
        .copied()
        .filter(|workflow| match action {
            ControlAction::Pause => workflow.available_controls.pause,
            ControlAction::Resume => workflow.available_controls.resume,
            ControlAction::Stop => workflow.available_controls.stop,
            ControlAction::Recover => workflow.available_controls.recover,
        })
        .collect::<Vec<_>>();
    match candidates.as_slice() {
        [workflow] => Ok(*workflow),
        [] => Err("no eligible owner workflow found".to_string()),
        _ => Err(format!(
            "ambiguous workflow target; matches {}",
            candidates
                .iter()
                .map(|workflow| workflow.identity.display_id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn execution_identity(identity: &WorkflowIdentity) -> Result<ExecutionIdentity, String> {
    let kind = match identity.kind.as_str() {
        "auto" => WorkflowKind::Auto,
        "plan" => WorkflowKind::Plan,
        other => return Err(format!("unknown workflow kind: {other}")),
    };
    Ok(ExecutionIdentity::new(kind, &identity.run_id))
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool, String> {
    conn.query_row(
        "select exists(select 1 from sqlite_master where type = 'table' and name = ?1)",
        [table],
        |row| row.get::<_, i64>(0),
    )
    .map(|value| value != 0)
    .map_err(|error| error.to_string())
}

fn workflow_selector_matches(selector: &str, identity: &WorkflowIdentity) -> bool {
    selector == identity.display_id
        || selector
            .strip_prefix("auto:")
            .is_some_and(|id| identity.kind == "auto" && id == identity.run_id)
        || selector
            .strip_prefix("plan:")
            .is_some_and(|id| identity.kind == "plan" && id == identity.run_id)
        || selector.strip_prefix("a:").is_some_and(|id| {
            identity.kind == "auto" && id.len() == 8 && identity.run_id.starts_with(id)
        })
        || selector.strip_prefix("p:").is_some_and(|id| {
            identity.kind == "plan" && id.len() == 8 && identity.run_id.starts_with(id)
        })
}

fn subject_candidates(snapshot: &WorkspaceSnapshot, subjects: &[Subject]) -> Vec<String> {
    subjects
        .iter()
        .map(|subject| match *subject {
            Subject::Repository(repo) => format!("repo:{}", snapshot.repositories[repo].label),
            Subject::Worktree(repo, worktree) => format!(
                "wt:{}",
                branch_label(&snapshot.repositories[repo].worktrees[worktree].branch)
            ),
            Subject::Workflow(repo, workflow) => snapshot.repositories[repo].workflows[workflow]
                .identity
                .display_id
                .clone(),
        })
        .collect()
}

fn short_id(kind: &str, run_id: &str) -> String {
    format!(
        "{}:{}",
        kind_prefix(kind),
        run_id.chars().take(8).collect::<String>()
    )
}

fn kind_prefix(kind: &str) -> &str {
    if kind == "auto" { "a" } else { "p" }
}
fn branch_label(branch: &BranchState) -> &str {
    match branch {
        BranchState::Named(name) => name,
        BranchState::Detached => "(detached)",
    }
}
fn absolute_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir().unwrap_or_default().join(path)
        }
    })
}
fn paths_equal(left: &Path, right: &Path) -> bool {
    absolute_path(left) == absolute_path(right)
}
fn path_contains(root: &Path, selected: &Path) -> bool {
    selected.is_absolute() && absolute_path(selected).starts_with(absolute_path(root))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "create table plan_run (
               id text primary key, status text not null, pause_requested integer not null,
               updated_unix_ms integer not null
             );
             create table plan_step_run (
               run_id text not null, step integer not null, status text not null,
               finished_unix_ms integer, error text, execution_process_id integer,
               execution_process_start_time_ticks integer, session_adapter_id text,
               session_endpoint text, session_id text
             );
             create table workflow_execution (
               workflow_kind text not null, run_id text not null, dispatch_state text not null,
               worker_id text, daemon_instance_id text, lease_expires_unix_ms integer,
               heartbeat_unix_ms integer, fencing_token integer not null,
               executor_pid integer, executor_process_identity text,
               requeue_requested integer not null, interruption_generation integer not null,
               created_unix_ms integer not null, updated_unix_ms integer not null,
               primary key (workflow_kind, run_id)
             );",
        )
        .unwrap();
        conn
    }

    fn workflow(status: &str, paused: bool, dispatch: &str) -> WorkflowSnapshot {
        WorkflowSnapshot {
            identity: WorkflowIdentity {
                repository: PathBuf::from("/repo"),
                kind: "plan".to_string(),
                run_id: "plan-1".to_string(),
                display_id: "p:plan-1".to_string(),
            },
            owner: None,
            worktree: WorktreeIdentity {
                path: PathBuf::from("/repo"),
                display: "repo".to_string(),
            },
            lifecycle: status.to_string(),
            pause_requested: paused,
            dispatch: DispatchSnapshot {
                state: Some(dispatch.to_string()),
                daemon_instance_id: None,
                worker_id: None,
                lease_expires_unix_ms: None,
                heartbeat_unix_ms: None,
                interruption_generation: 0,
                updated_unix_ms: Some(20),
            },
            current_step: None,
            progress: Progress::default(),
            available_controls: AvailableControls::default(),
            updated_unix_ms: 20,
        }
    }

    fn insert_run(conn: &Connection, status: &str, paused: bool, dispatch: &str) {
        conn.execute(
            "insert into plan_run values ('plan-1', ?1, ?2, 20)",
            params![status, paused],
        )
        .unwrap();
        conn.execute(
            "insert into workflow_execution (
               workflow_kind, run_id, dispatch_state, fencing_token, requeue_requested,
               interruption_generation, created_unix_ms, updated_unix_ms
             ) values ('plan', 'plan-1', ?1, 1, 0, 0, 10, 20)",
            [dispatch],
        )
        .unwrap();
    }

    #[test]
    fn active_resume_does_not_request_a_second_execution() {
        let mut conn = connection();
        insert_run(&conn, "running", true, "claimed");
        conn.execute(
            "insert into plan_step_run (run_id, step, status) values ('plan-1', 1, 'running')",
            [],
        )
        .unwrap();

        let (state, _) = apply_control_transaction(
            &mut conn,
            &workflow("running", true, "claimed"),
            ControlAction::Resume,
        )
        .unwrap();

        assert_eq!(state, "running");
        let dispatch: (String, i64) = conn
            .query_row(
                "select dispatch_state, requeue_requested from workflow_execution",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(dispatch, ("claimed".to_string(), 0));
    }

    #[test]
    fn stale_control_cannot_overwrite_recovery_pending() {
        let mut conn = connection();
        insert_run(&conn, "queued", false, "recovery_pending");
        conn.execute(
            "update workflow_execution set interruption_generation = 1, updated_unix_ms = 21",
            [],
        )
        .unwrap();

        let error = apply_control_transaction(
            &mut conn,
            &workflow("queued", false, "queued"),
            ControlAction::Pause,
        )
        .unwrap_err();

        assert!(error.contains("changed while applying control"));
        let state: String = conn
            .query_row("select dispatch_state from workflow_execution", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(state, "recovery_pending");
    }

    #[test]
    fn stop_commits_then_cancels_a_recorded_process() {
        use std::os::unix::process::CommandExt;

        let mut conn = connection();
        insert_run(&conn, "running", false, "claimed");
        let mut command = std::process::Command::new("sleep");
        command.arg("30");
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
        let mut child = command.spawn().unwrap();
        let pid = child.id();
        let start = crate::harness::process_start_time_ticks(pid).unwrap();
        conn.execute(
            "insert into plan_step_run (
               run_id, step, status, execution_process_id,
               execution_process_start_time_ticks
             ) values ('plan-1', 1, 'running', ?1, ?2)",
            params![pid, i64::try_from(start).unwrap()],
        )
        .unwrap();

        let (state, warnings) = apply_control_transaction(
            &mut conn,
            &workflow("running", false, "claimed"),
            ControlAction::Stop,
        )
        .unwrap();

        assert_eq!(state, "aborted");
        assert!(warnings.is_empty(), "{warnings:?}");
        let _ = child.wait();
        assert_ne!(crate::harness::process_start_time_ticks(pid), Some(start));
    }
}
