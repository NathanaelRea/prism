use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::agent::AgentState;
use crate::config::Config;
use crate::git::{self, GitStatus};
use crate::lifecycle;
use crate::persistence::workspace as workspace_persistence;
use crate::repo::Repository;
use crate::{worker, workspace};

const RECENT_TERMINAL_WORKFLOWS: usize = 10;

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
    pub workflow_details: Vec<crate::WorkflowProjection>,
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
    pub state: Option<AgentState>,
    pub updated_unix_ms: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CachedPullRequest {
    pub number: i64,
    pub title: String,
    pub url: String,
    pub state: PullRequestState,
    pub mergeability: Option<MergeabilityState>,
    pub ci: Option<CiState>,
    pub observed_unix_ms: i64,
    pub age_ms: i64,
    pub stale: bool,
    pub error: Option<String>,
    pub provenance: ObservationProvenance,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestState {
    Open,
    Draft,
    Closed,
    Merged,
    Unknown,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MergeabilityState {
    Clean,
    Dirty,
    Blocked,
    Behind,
    Unstable,
    HasHooks,
    Unknown,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CiState {
    Pending,
    Success,
    Failed,
    Mixed,
    Unknown,
}

impl CiState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Mixed => "mixed",
            Self::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for CiState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

impl std::ops::Deref for CiState {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.label()
    }
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ObservationProvenance {
    SqliteCache,
}

#[derive(Clone, Debug, Serialize)]
pub struct WorkflowSnapshot {
    pub identity: WorkflowIdentity,
    pub owner: Option<WorkflowIdentity>,
    pub worktree: WorktreeIdentity,
    pub lifecycle: WorkflowLifecycle,
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
    pub run_id: String,
    pub display_id: String,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DispatchState {
    Queued,
    Claimed,
    RecoveryPending,
    Paused,
    Terminal,
}

impl DispatchState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Claimed => "claimed",
            Self::RecoveryPending => "recovery_pending",
            Self::Paused => "paused",
            Self::Terminal => "terminal",
        }
    }
}

impl std::ops::Deref for DispatchState {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.label()
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct DispatchSnapshot {
    pub state: Option<DispatchState>,
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
    pub state: StepState,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowLifecycle {
    Queued,
    Running,
    Paused,
    Done,
    Failed,
    Aborted,
}

impl WorkflowLifecycle {
    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Aborted => "aborted",
        }
    }

    pub fn as_str(self) -> &'static str {
        self.label()
    }

    pub(crate) fn terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Aborted)
    }
}

impl PartialEq<&str> for WorkflowLifecycle {
    fn eq(&self, other: &&str) -> bool {
        self.label() == *other
    }
}

impl std::fmt::Display for WorkflowLifecycle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

impl std::ops::Deref for WorkflowLifecycle {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.label()
    }
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StepState {
    Queued,
    Starting,
    Running,
    Waiting,
    Done,
    Failed,
    Aborted,
    Skipped,
    Unknown,
}

impl StepState {
    fn parse(value: Option<String>) -> Self {
        match value.as_deref() {
            Some("queued" | "runnable") => Self::Queued,
            Some("starting") => Self::Starting,
            Some("running" | "claimed") => Self::Running,
            Some("waiting") => Self::Waiting,
            Some("done" | "succeeded") => Self::Done,
            Some("failed") => Self::Failed,
            Some("aborted" | "cancelled") => Self::Aborted,
            Some("skipped") => Self::Skipped,
            _ => Self::Unknown,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Aborted => "aborted",
            Self::Skipped => "skipped",
            Self::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for StepState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
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
    pub code: DiagnosticCode,
    pub severity: DiagnosticSeverity,
    pub scope: String,
    pub operation: Option<&'static str>,
    pub provenance: Option<ObservationProvenance>,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticCode {
    #[serde(rename = "repository_discovery_failed")]
    RepositoryDiscovery,
    #[serde(rename = "repository_inspection_failed")]
    RepositoryInspection,
    #[serde(rename = "projection_read_failed")]
    ProjectionRead,
    #[serde(rename = "daemon_probe_failed")]
    DaemonProbe,
    #[serde(rename = "cached_observation_failed")]
    CachedObservation,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Warning,
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
pub struct RecoveryDecision {
    pub workflow: WorkflowIdentity,
    pub restart: bool,
}

#[derive(Clone, Debug, Default)]
pub struct RecoveryBatchReceipt {
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
                        code: DiagnosticCode::RepositoryDiscovery,
                        severity: DiagnosticSeverity::Warning,
                        scope: entry.root.display().to_string(),
                        operation: Some("discover_repository"),
                        provenance: None,
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
        let observed = current_unix_ms();
        let mut warnings = self.discovery_diagnostics.clone();
        let daemon = match worker::probe_health() {
            Ok(health) => health,
            Err(error) => {
                warnings.push(Diagnostic {
                    code: DiagnosticCode::DaemonProbe,
                    severity: DiagnosticSeverity::Warning,
                    scope: "daemon".to_string(),
                    operation: Some("probe_daemon_health"),
                    provenance: None,
                    message: error,
                });
                worker::DaemonHealth::stopped()
            }
        };
        let ledger_runs = match list_ledger_workflows(&self.sources) {
            Ok(runs) => runs,
            Err(error) => {
                warnings.push(Diagnostic {
                    code: DiagnosticCode::ProjectionRead,
                    severity: DiagnosticSeverity::Warning,
                    scope: "workflow ledger".to_string(),
                    operation: Some("list_workflows"),
                    provenance: None,
                    message: error,
                });
                Vec::new()
            }
        };
        let mut repositories = Vec::new();
        for source in &self.sources {
            match inspect_repository(source, request, observed, &ledger_runs) {
                Ok((repository, mut repository_warnings)) => {
                    repositories.push(repository);
                    warnings.append(&mut repository_warnings);
                }
                Err(error) => warnings.push(Diagnostic {
                    code: DiagnosticCode::RepositoryInspection,
                    severity: DiagnosticSeverity::Warning,
                    scope: source.repo.root.display().to_string(),
                    operation: Some("inspect_repository"),
                    provenance: None,
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
        warnings.sort_by(|left, right| {
            left.scope
                .cmp(&right.scope)
                .then_with(|| left.operation.cmp(&right.operation))
                .then_with(|| left.message.cmp(&right.message))
        });
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
                "child run {} is owned by {}; control {} instead",
                target.identity.display_id, owner.display_id, owner.display_id
            ));
        }
        if request.action == ControlAction::Recover
            && target.dispatch.state != Some(DispatchState::RecoveryPending)
        {
            return Err("workflow does not require recovery".to_string());
        }
        let command = match request.action {
            ControlAction::Pause => crate::WorkflowCommand::Pause,
            ControlAction::Resume => crate::WorkflowCommand::Resume,
            ControlAction::Stop => crate::WorkflowCommand::Cancel,
            ControlAction::Recover => crate::WorkflowCommand::Retry,
        };
        worker::command_workflow(&target.identity.run_id, command)?;
        Ok(ControlReceipt {
            workflow: target.identity.clone(),
            state: match request.action {
                ControlAction::Pause => "paused",
                ControlAction::Resume | ControlAction::Recover => "runnable",
                ControlAction::Stop => "cancelled",
            }
            .to_string(),
            warnings: Vec::new(),
        })
    }

    pub fn recover_batch(
        &self,
        decisions: &[RecoveryDecision],
    ) -> Result<RecoveryBatchReceipt, String> {
        for decision in decisions {
            self.source_for_identity(&decision.workflow)?;
            if decision.restart {
                worker::command_workflow(&decision.workflow.run_id, crate::WorkflowCommand::Retry)?;
            } else {
                worker::command_workflow(
                    &decision.workflow.run_id,
                    crate::WorkflowCommand::Cancel,
                )?;
            }
        }
        Ok(RecoveryBatchReceipt::default())
    }

    fn source_for_identity(&self, identity: &WorkflowIdentity) -> Result<&RepoSource, String> {
        self.sources
            .iter()
            .find(|source| paths_equal(&source.repo.root, &identity.repository))
            .ok_or_else(|| {
                format!(
                    "workflow repository is no longer available: {}",
                    identity.repository.display()
                )
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

fn list_ledger_workflows(sources: &[RepoSource]) -> Result<Vec<crate::WorkflowProjection>, String> {
    let repositories = sources
        .iter()
        .map(|source| source.repo.root.display().to_string())
        .collect::<Vec<_>>();
    let from_worker = repositories
        .iter()
        .try_fold(Vec::new(), |mut runs, repository| {
            runs.extend(worker::list_workflows(Some(Path::new(repository)), 256)?);
            Ok::<_, String>(runs)
        });
    match from_worker {
        Ok(runs) => Ok(runs),
        Err(_) if !crate::util::prism_config_dir().join("workflow.db").exists() => Ok(Vec::new()),
        Err(socket_error) => crate::async_runtime::block_on(async {
            let operations = crate::WorkflowOperations::open_default().await?;
            let mut runs = Vec::new();
            for repository in repositories {
                runs.extend(operations.list(Some(&repository), 256).await?);
            }
            Ok::<_, crate::WorkflowOperationError>(runs)
        })
        .map_err(|error| format!("access workflow ledger runtime: {error}"))?
        .map_err(|error| format!("{socket_error}; direct workflow ledger read failed: {error}")),
    }
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
    ledger_runs: &[crate::WorkflowProjection],
) -> Result<(RepositorySnapshot, Vec<Diagnostic>), String> {
    let config = Config::load(&source.repo);
    let inventory = lifecycle::list_worktrees(&source.repo, &config)?;
    let db_path = source.repo.prism_dir().join("prism.db");
    let mut warnings = Vec::new();
    let reader = if db_path.exists() {
        match workspace_persistence::WorkspaceReader::open(&db_path) {
            Ok(conn) => Some(conn),
            Err(error) => {
                warnings.push(repository_diagnostic(
                    source,
                    "open_readonly_database",
                    error.to_string(),
                ));
                None
            }
        }
    } else {
        None
    };
    let hidden = read_projection(&reader, source, &mut warnings, "load_hidden", load_hidden)
        .unwrap_or_default();
    let mut workflows = load_ledger_workflows(ledger_runs, &source.repo.root)?;
    let workflow_details = ledger_runs
        .iter()
        .filter(|run| {
            run.repository
                .as_deref()
                .is_some_and(|repository| paths_equal(Path::new(repository), &source.repo.root))
        })
        .cloned()
        .collect::<Vec<_>>();
    if !request.include_terminal {
        workflows.retain(|workflow| !workflow.lifecycle.terminal());
    }
    workflows.sort_by(|left, right| {
        right
            .updated_unix_ms
            .cmp(&left.updated_unix_ms)
            .then_with(|| left.identity.run_id.cmp(&right.identity.run_id))
    });
    if request.include_terminal {
        retain_recent_terminal_workflows(&mut workflows, RECENT_TERMINAL_WORKFLOWS);
    }
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
        let agent = read_projection(&reader, source, &mut warnings, "load_agent", |reader| {
            load_agent(reader, &entry.branch)
        })
        .flatten()
        .unwrap_or_default();
        let pull_request = read_projection(
            &reader,
            source,
            &mut warnings,
            "load_pull_request",
            |reader| load_pr(reader, &entry.branch, observed),
        )
        .flatten();
        if let Some(error) = pull_request
            .as_ref()
            .and_then(|pull_request| pull_request.error.as_ref())
        {
            warnings.push(Diagnostic {
                code: DiagnosticCode::CachedObservation,
                severity: DiagnosticSeverity::Warning,
                scope: identity.path.display().to_string(),
                operation: Some("refresh_pull_request"),
                provenance: Some(ObservationProvenance::SqliteCache),
                message: error.clone(),
            });
        }
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
    let workflow_attention = workflows
        .iter()
        .filter(|workflow| {
            workflow.lifecycle == WorkflowLifecycle::Failed
                || workflow.dispatch.state == Some(DispatchState::RecoveryPending)
                || workflow.pause_requested
        })
        .count();
    let worktree_attention = worktrees
        .iter()
        .filter(|worktree| {
            matches!(
                worktree.agent.state,
                Some(AgentState::NeedsInput | AgentState::NeedsRestart | AgentState::ExitedError)
            ) || worktree.git.conflicts > 0
                || worktree.git.error.is_some()
                || worktree.pull_request.as_ref().is_some_and(|pull_request| {
                    pull_request.error.is_some()
                        || matches!(pull_request.ci, Some(CiState::Failed | CiState::Mixed))
                })
        })
        .count();
    let attention = workflow_attention + worktree_attention;
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
            workflow_details,
        },
        warnings,
    ))
}

fn retain_recent_terminal_workflows(workflows: &mut Vec<WorkflowSnapshot>, limit: usize) {
    let mut terminal = 0;
    workflows.retain(|workflow| {
        let terminal_workflow = matches!(
            workflow.lifecycle,
            WorkflowLifecycle::Done | WorkflowLifecycle::Aborted
        ) && matches!(
            workflow.dispatch.state,
            None | Some(DispatchState::Terminal)
        );
        !terminal_workflow || {
            terminal += 1;
            terminal <= limit
        }
    });
}

fn read_projection<T>(
    reader: &Option<workspace_persistence::WorkspaceReader>,
    source: &RepoSource,
    warnings: &mut Vec<Diagnostic>,
    operation: &'static str,
    read: impl FnOnce(&workspace_persistence::WorkspaceReader) -> Result<T, String>,
) -> Option<T> {
    let reader = reader.as_ref()?;
    match read(reader) {
        Ok(value) => Some(value),
        Err(error) => {
            warnings.push(repository_diagnostic(source, operation, error));
            None
        }
    }
}

fn repository_diagnostic(
    source: &RepoSource,
    operation: &'static str,
    message: String,
) -> Diagnostic {
    Diagnostic {
        code: DiagnosticCode::ProjectionRead,
        severity: DiagnosticSeverity::Warning,
        scope: source.repo.root.display().to_string(),
        operation: Some(operation),
        provenance: Some(ObservationProvenance::SqliteCache),
        message,
    }
}

fn load_hidden(
    reader: &workspace_persistence::WorkspaceReader,
) -> Result<BTreeSet<String>, String> {
    await_cache(reader.hidden()).map(|branches| branches.into_iter().collect())
}

fn load_agent(
    reader: &workspace_persistence::WorkspaceReader,
    branch: &str,
) -> Result<Option<AgentStatus>, String> {
    await_cache(reader.agent(branch))?.map_or(Ok(None), |row| {
        let state = AgentState::parse(&row.state)
            .ok_or_else(|| format!("unknown agent state: {}", row.state))?;
        Ok(Some(AgentStatus {
            state: Some(state),
            updated_unix_ms: Some(row.updated_unix_ms),
        }))
    })
}

fn load_pr(
    reader: &workspace_persistence::WorkspaceReader,
    branch: &str,
    observed: i64,
) -> Result<Option<CachedPullRequest>, String> {
    await_cache(reader.pull_request(branch))?.map_or(Ok(None), |row| {
        let refreshed = row.refreshed_unix_ms.saturating_mul(1_000);
        let age_ms = observed.saturating_sub(refreshed).max(0);
        let error = row.observation_error;
        let state = pull_request_state(&row.state, row.merged != 0, row.draft != 0);
        Ok(Some(CachedPullRequest {
            number: row.number,
            title: row.title,
            url: row.url,
            state,
            mergeability: row
                .merge_state_status
                .filter(|value| !value.is_empty())
                .map(|value| mergeability_state(&value)),
            ci: row
                .check_status
                .filter(|value| !value.is_empty())
                .map(|value| ci_state(&value)),
            observed_unix_ms: refreshed,
            age_ms,
            stale: error.is_some()
                || age_ms
                    > i64::try_from(crate::remote::PR_SUMMARY_POLL_INTERVAL.as_millis() * 2)
                        .unwrap_or(i64::MAX),
            error,
            provenance: ObservationProvenance::SqliteCache,
        }))
    })
}

fn await_cache<T>(
    future: impl std::future::Future<Output = Result<T, String>>,
) -> Result<T, String> {
    crate::async_runtime::block_on(future)
        .map_err(|error| format!("access repository cache runtime: {error}"))?
}

fn pull_request_state(value: &str, merged: bool, draft: bool) -> PullRequestState {
    if merged {
        PullRequestState::Merged
    } else if draft {
        PullRequestState::Draft
    } else {
        match value.to_ascii_lowercase().as_str() {
            "open" => PullRequestState::Open,
            "closed" => PullRequestState::Closed,
            "merged" => PullRequestState::Merged,
            _ => PullRequestState::Unknown,
        }
    }
}

fn mergeability_state(value: &str) -> MergeabilityState {
    match value.to_ascii_lowercase().as_str() {
        "clean" => MergeabilityState::Clean,
        "dirty" => MergeabilityState::Dirty,
        "blocked" => MergeabilityState::Blocked,
        "behind" => MergeabilityState::Behind,
        "unstable" => MergeabilityState::Unstable,
        "has_hooks" => MergeabilityState::HasHooks,
        _ => MergeabilityState::Unknown,
    }
}

fn ci_state(value: &str) -> CiState {
    match value.to_ascii_lowercase().as_str() {
        "running" | "pending" => CiState::Pending,
        "passed" | "success" => CiState::Success,
        "failed" | "failure" => CiState::Failed,
        "mixed" => CiState::Mixed,
        _ => CiState::Unknown,
    }
}

fn load_ledger_workflows(
    runs: &[crate::WorkflowProjection],
    repo_root: &Path,
) -> Result<Vec<WorkflowSnapshot>, String> {
    runs.iter()
        .filter(|run| {
            run.repository
                .as_deref()
                .is_some_and(|repository| paths_equal(Path::new(repository), repo_root))
        })
        .map(|run| {
            let lifecycle = ledger_lifecycle(&run.status)?;
            let dispatch_state = match run.status.as_str() {
                "waiting" | "runnable" => Some(DispatchState::Queued),
                "running" => Some(DispatchState::Claimed),
                "paused" => Some(DispatchState::Paused),
                "recovery_required" => Some(DispatchState::RecoveryPending),
                "succeeded" | "failed" | "cancelled" => Some(DispatchState::Terminal),
                _ => None,
            };
            let current_step = run
                .steps
                .iter()
                .find(|step| matches!(step.status.as_str(), "claimed" | "runnable" | "waiting"))
                .or_else(|| run.steps.last());
            let worktree = run
                .steps
                .iter()
                .find_map(|step| {
                    serde_json::from_str::<serde_json::Value>(&step.input_json)
                        .ok()?
                        .get("cwd")?
                        .as_str()
                        .map(PathBuf::from)
                })
                .unwrap_or_else(|| repo_root.to_path_buf());
            let worker_id = run
                .attempts
                .iter()
                .find(|attempt| attempt.status == "claimed")
                .map(|attempt| attempt.worker_id.clone());
            let completed = run
                .steps
                .iter()
                .filter(|step| step.status == "succeeded")
                .count();
            Ok(WorkflowSnapshot {
                identity: WorkflowIdentity {
                    repository: absolute_path(repo_root),
                    run_id: run.id.clone(),
                    display_id: String::new(),
                },
                owner: run.parent_run_id.as_ref().map(|run_id| WorkflowIdentity {
                    repository: absolute_path(repo_root),
                    run_id: run_id.clone(),
                    display_id: run_id.clone(),
                }),
                worktree: WorktreeIdentity {
                    path: absolute_path(&worktree),
                    display: String::new(),
                },
                lifecycle,
                pause_requested: false,
                dispatch: DispatchSnapshot {
                    state: dispatch_state,
                    daemon_instance_id: None,
                    worker_id,
                    lease_expires_unix_ms: None,
                    heartbeat_unix_ms: None,
                    interruption_generation: 0,
                    updated_unix_ms: Some(run.updated_unix_ms),
                },
                current_step: current_step.map(|step| StepSummary {
                    label: step.key.clone(),
                    state: StepState::parse(Some(step.status.clone())),
                }),
                progress: Progress {
                    completed,
                    total: run.steps.len(),
                },
                available_controls: controls_for(lifecycle, false, dispatch_state, true),
                updated_unix_ms: run.updated_unix_ms,
            })
        })
        .collect()
}

fn ledger_lifecycle(status: &str) -> Result<WorkflowLifecycle, String> {
    match status {
        "waiting" | "runnable" => Ok(WorkflowLifecycle::Queued),
        "running" => Ok(WorkflowLifecycle::Running),
        "paused" => Ok(WorkflowLifecycle::Paused),
        "succeeded" => Ok(WorkflowLifecycle::Done),
        "failed" | "recovery_required" => Ok(WorkflowLifecycle::Failed),
        "cancelled" => Ok(WorkflowLifecycle::Aborted),
        other => Err(format!("unknown global workflow lifecycle: {other}")),
    }
}

fn controls_for(
    lifecycle: WorkflowLifecycle,
    pause_requested: bool,
    dispatch: Option<DispatchState>,
    ownerless: bool,
) -> AvailableControls {
    let terminal = lifecycle.terminal();
    AvailableControls {
        pause: ownerless
            && !terminal
            && !pause_requested
            && !matches!(
                dispatch,
                Some(DispatchState::RecoveryPending | DispatchState::Paused)
            ),
        resume: ownerless
            && !terminal
            && (pause_requested
                || lifecycle == WorkflowLifecycle::Paused
                || dispatch == Some(DispatchState::Paused))
            && dispatch != Some(DispatchState::RecoveryPending),
        stop: ownerless && !terminal,
        recover: ownerless && dispatch == Some(DispatchState::RecoveryPending),
    }
}

fn assign_display_ids(repositories: &mut [RepositorySnapshot]) {
    let mut counts = BTreeMap::<String, usize>::new();
    for workflow in repositories.iter().flat_map(|repo| &repo.workflows) {
        let prefix = short_id(&workflow.identity.run_id);
        *counts.entry(prefix).or_default() += 1;
    }
    for repository in repositories {
        for workflow in &mut repository.workflows {
            let short = short_id(&workflow.identity.run_id);
            workflow.identity.display_id = if counts[&short] == 1 {
                short
            } else {
                workflow.identity.run_id.clone()
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
                if let Some(workflow) = repository
                    .workflows
                    .iter()
                    .find(|workflow| workflow.identity.run_id == identity.run_id)
                {
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
                    workflow.identity.run_id.clone(),
                    workflow.identity.display_id.clone(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        for workflow in &mut repository.workflows {
            if let Some(owner) = &mut workflow.owner
                && let Some(display_id) = display_ids.get(&owner.run_id)
            {
                owner.display_id.clone_from(display_id);
            }
        }
    }
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
    } else if workflow.dispatch.state == Some(DispatchState::RecoveryPending)
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
        _ => Err(format!("ambiguous workflow target; matches {}", {
            let mut display_ids = candidates
                .iter()
                .map(|workflow| workflow.identity.display_id.as_str())
                .collect::<Vec<_>>();
            display_ids.sort_unstable();
            display_ids.join(", ")
        })),
    }
}

fn workflow_selector_matches(selector: &str, identity: &WorkflowIdentity) -> bool {
    selector == identity.display_id
        || selector == identity.run_id
        || selector
            .strip_prefix("w:")
            .is_some_and(|id| id.len() == 8 && identity.run_id.starts_with(id))
}

fn subject_candidates(snapshot: &WorkspaceSnapshot, subjects: &[Subject]) -> Vec<String> {
    let mut candidates = subjects
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
        .collect::<Vec<_>>();
    candidates.sort();
    candidates
}

fn short_id(run_id: &str) -> String {
    format!("w:{}", run_id.chars().take(8).collect::<String>())
}

fn current_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
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
