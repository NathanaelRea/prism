use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::agent::AgentState;
use crate::config::Config;
use crate::execution::{self, WorkflowIdentity as ExecutionIdentity, WorkflowKind};
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
    pub kind: WorkflowKind,
    pub run_id: String,
    pub display_id: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct DispatchSnapshot {
    pub state: Option<execution::DispatchState>,
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
    Draft,
    Queued,
    Running,
    Paused,
    Done,
    Failed,
    Aborted,
}

impl WorkflowLifecycle {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "draft" => Ok(Self::Draft),
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "paused" => Ok(Self::Paused),
            "done" => Ok(Self::Done),
            "failed" => Ok(Self::Failed),
            "aborted" => Ok(Self::Aborted),
            other => Err(format!("unknown workflow lifecycle: {other}")),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Draft => "draft",
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
            Some("queued") => Self::Queued,
            Some("starting") => Self::Starting,
            Some("running") => Self::Running,
            Some("waiting") => Self::Waiting,
            Some("done") => Self::Done,
            Some("failed") => Self::Failed,
            Some("aborted") => Self::Aborted,
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
    pub interruption_generation: i64,
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
        let observed = execution::now_ms();
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
        let mut repositories = Vec::new();
        for source in &self.sources {
            match inspect_repository(source, request, observed) {
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
                "plan run {} is owned by {}; control {} instead",
                target.identity.display_id, owner.display_id, owner.display_id
            ));
        }
        if request.action == ControlAction::Recover {
            if target.dispatch.state != Some(execution::DispatchState::RecoveryPending) {
                return Err("workflow does not require recovery".to_string());
            }
            let batch = self.recover_batch(&[RecoveryDecision {
                workflow: target.identity.clone(),
                interruption_generation: target.dispatch.interruption_generation,
                restart: true,
            }])?;
            return Ok(ControlReceipt {
                workflow: target.identity.clone(),
                state: "queued".to_string(),
                warnings: batch.warnings,
            });
        }
        let source = self
            .sources
            .iter()
            .find(|source| source.repo.root == target.identity.repository)
            .ok_or_else(|| "workflow repository is no longer available".to_string())?;
        let path = source.repo.prism_dir().join("prism.db");
        let (state, mut warnings) = apply_control_transaction(&path, target, request.action)?;
        if request.action == ControlAction::Resume {
            notify_daemon_after_commit(true, "control", &mut warnings);
        } else if request.action == ControlAction::Pause {
            notify_daemon_after_commit(false, "control", &mut warnings);
        }
        Ok(ControlReceipt {
            workflow: target.identity.clone(),
            state,
            warnings,
        })
    }

    pub fn recover_batch(
        &self,
        decisions: &[RecoveryDecision],
    ) -> Result<RecoveryBatchReceipt, String> {
        for decision in decisions {
            self.source_for_identity(&decision.workflow)?;
        }
        for decision in decisions.iter().filter(|decision| decision.restart) {
            let source = self.source_for_identity(&decision.workflow)?;
            let config = Config::load(&source.repo);
            if worker::legacy_worker_running(
                &source.repo,
                &config,
                &execution_identity(&decision.workflow)?,
            )? {
                return Err(format!(
                    "legacy {} worker for run {} is still active; try recovery after it exits",
                    decision.workflow.kind, decision.workflow.run_id
                ));
            }
        }

        let mut receipt = RecoveryBatchReceipt::default();
        for source in &self.sources {
            let repository_decisions = decisions
                .iter()
                .filter(|decision| paths_equal(&decision.workflow.repository, &source.repo.root))
                .map(|decision| {
                    Ok((
                        execution_identity(&decision.workflow)?,
                        decision.interruption_generation,
                        decision.restart,
                    ))
                })
                .collect::<Result<Vec<_>, String>>()?;
            if repository_decisions.is_empty() {
                continue;
            }
            let path = source.repo.prism_dir().join("prism.db");
            execution::persistence::WorkflowStore::open(&path)
                .map_err(|error| error.to_string())?
                .apply_recovery_decision(&repository_decisions, execution::now_ms())
                .map_err(|error| error.to_string())?;
            if repository_decisions.iter().any(|(_, _, restart)| *restart) {
                notify_daemon_after_commit(true, "recovery", &mut receipt.warnings);
            }
        }
        Ok(receipt)
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

fn notify_daemon_after_commit(ensure: bool, operation: &str, warnings: &mut Vec<String>) {
    if ensure && let Err(error) = worker::ensure_running() {
        warnings.push(format!(
            "{operation} committed, but daemon is unavailable: {error}"
        ));
        return;
    }
    if let Err(error) = worker::wake() {
        warnings.push(format!(
            "{operation} committed, but daemon wake failed: {error}"
        ));
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
    let mut workflows =
        read_projection(&reader, source, &mut warnings, "load_workflows", |reader| {
            load_workflows(reader, &source.repo.root, request.include_terminal)
        })
        .unwrap_or_default();
    workflows.sort_by(|left, right| {
        right
            .updated_unix_ms
            .cmp(&left.updated_unix_ms)
            .then_with(|| left.identity.kind.cmp(&right.identity.kind))
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
                || workflow.dispatch.state == Some(execution::DispatchState::RecoveryPending)
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
            None | Some(execution::DispatchState::Terminal)
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
    reader
        .hidden()
        .map(|branches| branches.into_iter().collect())
}

fn load_agent(
    reader: &workspace_persistence::WorkspaceReader,
    branch: &str,
) -> Result<Option<AgentStatus>, String> {
    reader.agent(branch)?.map_or(Ok(None), |row| {
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
    reader.pull_request(branch)?.map_or(Ok(None), |row| {
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

fn load_workflows(
    reader: &workspace_persistence::WorkspaceReader,
    repo_root: &Path,
    include_terminal: bool,
) -> Result<Vec<WorkflowSnapshot>, String> {
    let repo_root_display = repo_root.display().to_string();
    let rows = reader.workflows(&repo_root_display, include_terminal)?;
    let mut workflows = rows
        .into_iter()
        .map(|row| {
            let lifecycle = WorkflowLifecycle::parse(&row.lifecycle)?;
            let pause_requested = row.pause_requested != 0;
            let dispatch_state = row
                .dispatch_state
                .as_deref()
                .map(execution::DispatchState::parse)
                .transpose()?;
            Ok(WorkflowSnapshot {
                identity: WorkflowIdentity {
                    repository: absolute_path(repo_root),
                    kind: if row.kind == "auto" {
                        WorkflowKind::Auto
                    } else {
                        WorkflowKind::Plan
                    },
                    run_id: row.run_id,
                    display_id: String::new(),
                },
                owner: None,
                worktree: WorktreeIdentity {
                    path: absolute_path(Path::new(&row.worktree_path)),
                    display: String::new(),
                },
                lifecycle,
                pause_requested,
                dispatch: DispatchSnapshot {
                    state: dispatch_state,
                    daemon_instance_id: row.daemon_instance_id,
                    worker_id: row.worker_id,
                    lease_expires_unix_ms: row.lease_expires_unix_ms,
                    heartbeat_unix_ms: row.heartbeat_unix_ms,
                    interruption_generation: row.interruption_generation,
                    updated_unix_ms: row.dispatch_updated_unix_ms,
                },
                current_step: row.current_step.map(|label| StepSummary {
                    label,
                    state: StepState::parse(row.current_step_state),
                }),
                progress: Progress {
                    completed: row.completed.max(0) as usize,
                    total: row.total.max(0) as usize,
                },
                available_controls: controls_for(lifecycle, pause_requested, dispatch_state, true),
                updated_unix_ms: row.updated_unix_ms,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let owners = linked_plan_owners(reader, repo_root)?;
    let identities = workflows
        .iter()
        .map(|workflow| {
            (
                (workflow.identity.kind, workflow.identity.run_id.clone()),
                workflow.identity.clone(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    for workflow in &mut workflows {
        if workflow.identity.kind == WorkflowKind::Plan
            && let Some(owner) = owners.get(&workflow.identity.run_id)
        {
            workflow.owner = Some(
                identities
                    .get(&(WorkflowKind::Auto, owner.run_id.clone()))
                    .cloned()
                    .unwrap_or_else(|| owner.clone()),
            );
            workflow.available_controls = AvailableControls::default();
        }
    }
    Ok(workflows)
}

fn linked_plan_owners(
    reader: &workspace_persistence::WorkspaceReader,
    repo_root: &Path,
) -> Result<BTreeMap<String, WorkflowIdentity>, String> {
    let mut owners = BTreeMap::new();
    for row in reader.linked_plan_owners()? {
        owners
            .entry(row.plan_run_id)
            .or_insert_with(|| WorkflowIdentity {
                repository: absolute_path(repo_root),
                kind: WorkflowKind::Auto,
                display_id: short_id(WorkflowKind::Auto, &row.auto_run_id),
                run_id: row.auto_run_id,
            });
    }
    Ok(owners)
}

fn controls_for(
    lifecycle: WorkflowLifecycle,
    pause_requested: bool,
    dispatch: Option<execution::DispatchState>,
    ownerless: bool,
) -> AvailableControls {
    let terminal = lifecycle.terminal();
    AvailableControls {
        pause: ownerless
            && !terminal
            && !pause_requested
            && !matches!(
                dispatch,
                Some(execution::DispatchState::RecoveryPending | execution::DispatchState::Paused)
            ),
        resume: ownerless
            && !terminal
            && (pause_requested
                || lifecycle == WorkflowLifecycle::Paused
                || dispatch == Some(execution::DispatchState::Paused))
            && dispatch != Some(execution::DispatchState::RecoveryPending),
        stop: ownerless && !terminal,
        recover: ownerless && dispatch == Some(execution::DispatchState::RecoveryPending),
    }
}

fn assign_display_ids(repositories: &mut [RepositorySnapshot]) {
    let mut counts = BTreeMap::<String, usize>::new();
    for workflow in repositories.iter().flat_map(|repo| &repo.workflows) {
        let prefix = short_id(workflow.identity.kind, &workflow.identity.run_id);
        *counts.entry(prefix).or_default() += 1;
    }
    for repository in repositories {
        for workflow in &mut repository.workflows {
            let short = short_id(workflow.identity.kind, &workflow.identity.run_id);
            workflow.identity.display_id = if counts[&short] == 1 {
                short
            } else {
                format!(
                    "{}:{}",
                    kind_prefix(workflow.identity.kind),
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
                    (workflow.identity.kind, workflow.identity.run_id.clone()),
                    workflow.identity.display_id.clone(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        for workflow in &mut repository.workflows {
            if let Some(owner) = &mut workflow.owner
                && let Some(display_id) = display_ids.get(&(owner.kind, owner.run_id.clone()))
            {
                owner.display_id.clone_from(display_id);
            }
        }
    }
}

fn apply_control_transaction(
    path: &Path,
    workflow: &WorkflowSnapshot,
    action: ControlAction,
) -> Result<(String, Vec<String>), String> {
    if workflow.dispatch.state == Some(execution::DispatchState::RecoveryPending) {
        return Err("workflow was interrupted; use recover instead".to_string());
    }
    let now = execution::now_ms();
    let mut warnings = Vec::new();
    let kind = if workflow.identity.kind == WorkflowKind::Auto {
        workspace_persistence::ControlKind::Auto
    } else {
        workspace_persistence::ControlKind::Plan
    };
    let action = match action {
        ControlAction::Pause => workspace_persistence::ControlAction::Pause,
        ControlAction::Resume => workspace_persistence::ControlAction::Resume,
        ControlAction::Stop => workspace_persistence::ControlAction::Stop,
        ControlAction::Recover => unreachable!(),
    };
    let output = workspace_persistence::apply_control(
        path,
        &workspace_persistence::ControlInput {
            kind,
            run_id: &workflow.identity.run_id,
            lifecycle: workflow.lifecycle.label(),
            pause_requested: workflow.pause_requested,
            updated_unix_ms: workflow.updated_unix_ms,
            dispatch_state: workflow.dispatch.state.map(execution::DispatchState::label),
            interruption_generation: workflow.dispatch.interruption_generation,
            action,
            now,
        },
    )?;
    if action == workspace_persistence::ControlAction::Stop {
        cancel_recorded_work(
            RecordedCancellation {
                processes: output
                    .processes
                    .into_iter()
                    .filter_map(|(pid, identity)| {
                        Some((
                            u32::try_from(pid).ok()?,
                            identity.and_then(|value| u64::try_from(value).ok()),
                        ))
                    })
                    .collect(),
                sessions: output.sessions,
            },
            &mut warnings,
        );
    }
    Ok((output.state, warnings))
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
    } else if workflow.dispatch.state == Some(execution::DispatchState::RecoveryPending)
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

struct RecordedCancellation {
    processes: Vec<(u32, Option<u64>)>,
    sessions: Vec<crate::harness::SessionRef>,
}

fn cancel_recorded_work(cancellation: RecordedCancellation, warnings: &mut Vec<String>) {
    for session in cancellation.sessions {
        if let Err(error) = crate::harness::cancel_native_session(&session) {
            warnings.push(format!(
                "workflow stopped, but native session cancellation failed: {error}"
            ));
        }
    }
    for (pid, identity) in cancellation.processes {
        let recorded = crate::process::RecordedProcess::from_stored(pid, identity);
        match crate::process::terminate_recorded_process(
            recorded,
            std::time::Duration::from_secs(1),
        ) {
            Ok(crate::process::TerminationOutcome::Terminated)
            | Ok(crate::process::TerminationOutcome::AlreadyExited)
            | Ok(crate::process::TerminationOutcome::IdentityReused) => {}
            Ok(crate::process::TerminationOutcome::Unverifiable) => warnings.push(format!(
                "workflow stopped, but external process {pid} has no reusable process identity"
            )),
            Err(error) => warnings.push(format!(
                "workflow stopped, but external process {pid} cancellation failed: {error}"
            )),
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

fn execution_identity(identity: &WorkflowIdentity) -> Result<ExecutionIdentity, String> {
    Ok(ExecutionIdentity::new(identity.kind, &identity.run_id))
}

fn workflow_selector_matches(selector: &str, identity: &WorkflowIdentity) -> bool {
    selector == identity.display_id
        || selector
            .strip_prefix("auto:")
            .is_some_and(|id| identity.kind == WorkflowKind::Auto && id == identity.run_id)
        || selector
            .strip_prefix("plan:")
            .is_some_and(|id| identity.kind == WorkflowKind::Plan && id == identity.run_id)
        || selector.strip_prefix("a:").is_some_and(|id| {
            identity.kind == WorkflowKind::Auto && id.len() == 8 && identity.run_id.starts_with(id)
        })
        || selector.strip_prefix("p:").is_some_and(|id| {
            identity.kind == WorkflowKind::Plan && id.len() == 8 && identity.run_id.starts_with(id)
        })
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

fn short_id(kind: WorkflowKind, run_id: &str) -> String {
    format!(
        "{}:{}",
        kind_prefix(kind),
        run_id.chars().take(8).collect::<String>()
    )
}

fn kind_prefix(kind: WorkflowKind) -> &'static str {
    if kind == WorkflowKind::Auto { "a" } else { "p" }
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
    use crate::persistence::database::TestDatabase;
    use crate::sqlx_test_params as params;
    use std::sync::atomic::{AtomicU64, Ordering};

    static DATABASE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

    fn connection() -> TestDatabase {
        let path = std::env::temp_dir().join(format!(
            "prism-workspace-interface-{}-{}.db",
            std::process::id(),
            DATABASE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        TestDatabase::open(&path).unwrap()
    }

    fn reader(conn: &TestDatabase) -> workspace_persistence::WorkspaceReader {
        workspace_persistence::WorkspaceReader::open(conn.path()).unwrap()
    }

    fn apply_test_control(
        conn: &mut TestDatabase,
        workflow: &WorkflowSnapshot,
        action: ControlAction,
    ) -> Result<(String, Vec<String>), String> {
        apply_control_transaction(conn.path(), workflow, action)
    }

    fn workflow(status: &str, paused: bool, dispatch: &str) -> WorkflowSnapshot {
        let lifecycle = WorkflowLifecycle::parse(status).unwrap();
        let dispatch = match dispatch {
            "queued" => execution::DispatchState::Queued,
            "claimed" => execution::DispatchState::Claimed,
            "recovery_pending" => execution::DispatchState::RecoveryPending,
            "paused" => execution::DispatchState::Paused,
            "terminal" => execution::DispatchState::Terminal,
            other => panic!("unexpected dispatch state: {other}"),
        };
        WorkflowSnapshot {
            identity: WorkflowIdentity {
                repository: PathBuf::from("/repo"),
                kind: WorkflowKind::Plan,
                run_id: "plan-1".to_string(),
                display_id: "p:plan-1".to_string(),
            },
            owner: None,
            worktree: WorktreeIdentity {
                path: PathBuf::from("/repo"),
                display: "repo".to_string(),
            },
            lifecycle,
            pause_requested: paused,
            dispatch: DispatchSnapshot {
                state: Some(dispatch),
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

    fn insert_run(conn: &TestDatabase, status: &str, paused: bool, dispatch: &str) {
        conn.execute(
            "insert into plan_run (
               id, repo_root, scope_path, plan_path, plan_display, step_name,
               start_step, total_steps, mode, status, pause_requested, selected_step,
               created_unix_ms, updated_unix_ms
             ) values (
               'plan-1', '/repo', '/repo', '/repo/plan.md', 'plan.md', 'phase',
               1, 1, 'sequential', ?1, ?2, 1, 10, 20
             )",
            params![status, paused],
        )
        .unwrap();
        conn.execute(
            "insert into workflow_execution (
               workflow_kind, run_id, dispatch_state, fencing_token, requeue_requested,
               interruption_generation, created_unix_ms, updated_unix_ms
             ) values ('plan', 'plan-1', ?1, 1, 0, 0, 10, 20)",
            params![dispatch],
        )
        .unwrap();
    }

    #[test]
    fn cached_pull_request_states_use_mergeability_not_review_decision() {
        assert_eq!(
            pull_request_state("OPEN", false, false),
            PullRequestState::Open
        );
        assert_eq!(
            pull_request_state("OPEN", false, true),
            PullRequestState::Draft
        );
        assert_eq!(
            pull_request_state("CLOSED", true, false),
            PullRequestState::Merged
        );
        assert_eq!(mergeability_state("DIRTY"), MergeabilityState::Dirty);
        assert_eq!(ci_state("passed"), CiState::Success);
    }

    #[test]
    fn persisted_agent_labels_project_as_stable_typed_states() {
        let conn = connection();
        conn.execute(
            "insert into agent_state values ('feature', 'needs input', 42)",
            [],
        )
        .unwrap();

        let agent = load_agent(&reader(&conn), "feature").unwrap().unwrap();

        assert_eq!(agent.state, Some(AgentState::NeedsInput));
        assert_eq!(agent.updated_unix_ms, Some(42));
        assert_eq!(serde_json::to_value(agent).unwrap()["state"], "needs_input");
    }

    #[test]
    fn cached_pull_request_preserves_mergeability_staleness_and_error_provenance() {
        let conn = connection();
        conn.execute(
            "insert into pr_cache (
            branch, number, provider, canonical_host, project_path, native_cr_id,
            display_number, source_provider, source_canonical_host, source_project_path,
            target_provider, target_canonical_host, target_project_path,
            title, url, state, review_decision, head_ref, base_ref,
            head_sha, updated_at, merge_state_status, check_status, merged, draft,
            last_refreshed, refreshed_unix_ms, observation_error
          ) values ('feature', 42, 'github', 'github.com', 'org/repo', '42', 42,
            'github', 'github.com', 'org/repo', 'github', 'github.com', 'org/repo',
            'PR', 'https://example.test/42', 'OPEN',
            'APPROVED', 'feature', 'main', 'abc', '', 'DIRTY', 'passed', 0, 0, '', 10,
            'gh unavailable')",
            [],
        )
        .unwrap();

        let pull_request = load_pr(&reader(&conn), "feature", 100_000)
            .unwrap()
            .unwrap();

        assert_eq!(pull_request.state, PullRequestState::Open);
        assert_eq!(pull_request.mergeability, Some(MergeabilityState::Dirty));
        assert_eq!(pull_request.ci, Some(CiState::Success));
        assert_eq!(pull_request.age_ms, 90_000);
        assert!(pull_request.stale);
        assert_eq!(pull_request.error.as_deref(), Some("gh unavailable"));
        assert_eq!(pull_request.provenance, ObservationProvenance::SqliteCache);
        let json = serde_json::to_value(&pull_request).unwrap();
        assert_eq!(json["state"], "open");
        assert_eq!(json["mergeability"], "dirty");
        assert_eq!(json["ci"], "success");
        assert_eq!(json["provenance"], "sqlite_cache");
    }

    #[test]
    fn recent_terminal_filter_keeps_attention_workflows_and_is_bounded() {
        let mut workflows = (0..12)
            .map(|index| {
                let mut workflow = workflow("done", false, "terminal");
                workflow.identity.run_id = format!("done-{index:02}");
                workflow.updated_unix_ms = 100 - index;
                workflow
            })
            .collect::<Vec<_>>();
        workflows.push(workflow("failed", false, "terminal"));
        workflows.push(workflow("running", false, "claimed"));

        retain_recent_terminal_workflows(&mut workflows, 10);

        assert_eq!(workflows.len(), 12);
        assert!(
            workflows
                .iter()
                .any(|workflow| workflow.lifecycle == WorkflowLifecycle::Failed)
        );
        assert!(
            workflows
                .iter()
                .any(|workflow| workflow.lifecycle == WorkflowLifecycle::Running)
        );
    }

    #[test]
    fn active_resume_does_not_request_a_second_execution() {
        let mut conn = connection();
        insert_run(&conn, "running", true, "claimed");
        conn.execute(
            "insert into plan_step_run (run_id, step, prompt, status, execution_process_id)
             values ('plan-1', 1, 'phase', 'running', ?1)",
            params![i64::from(std::process::id())],
        )
        .unwrap();

        let (state, _) = apply_test_control(
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
    fn claimed_resume_does_not_reconcile_a_starting_step_without_a_pid() {
        let mut conn = connection();
        insert_run(&conn, "running", true, "claimed");
        conn.execute(
            "insert into plan_step_run (run_id, step, prompt, status)
             values ('plan-1', 1, 'phase', 'starting')",
            [],
        )
        .unwrap();

        let (state, _) = apply_test_control(
            &mut conn,
            &workflow("running", true, "claimed"),
            ControlAction::Resume,
        )
        .unwrap();

        assert_eq!(state, "running");
        let persisted: (String, String, i64) = conn
            .query_row(
                "select p.status, e.dispatch_state, e.requeue_requested
                 from plan_run p join workflow_execution e on e.run_id = p.id",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(persisted, ("running".to_string(), "claimed".to_string(), 0));
        let step: String = conn
            .query_row("select status from plan_step_run", [], |row| row.get(0))
            .unwrap();
        assert_eq!(step, "starting");
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

        let error = apply_test_control(
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
        let mut conn = connection();
        insert_run(&conn, "running", false, "claimed");
        let mut command = std::process::Command::new("sleep");
        command.arg("30");
        let mut child = crate::process::SupervisedChild::spawn(&mut command, None, None).unwrap();
        let pid = child.id();
        let recorded = crate::process::record_process(pid).unwrap();
        let start = recorded.identity.map(|identity| identity.stored_value());
        let reaper = std::thread::spawn(move || child.wait().unwrap());
        conn.execute(
            "insert into plan_step_run (
               run_id, step, prompt, status, execution_process_id,
               execution_process_start_time_ticks
             ) values ('plan-1', 1, 'phase', 'running', ?1, ?2)",
            params![pid, start.map(|value| i64::try_from(value).unwrap())],
        )
        .unwrap();

        let (state, warnings) = apply_test_control(
            &mut conn,
            &workflow("running", false, "claimed"),
            ControlAction::Stop,
        )
        .unwrap();

        assert_eq!(state, "aborted");
        assert!(warnings.is_empty(), "{warnings:?}");
        let process: (Option<i64>, Option<i64>) = conn
            .query_row(
                "select execution_process_id, execution_process_start_time_ticks
                 from plan_step_run where run_id = 'plan-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(process, (None, None));
        reaper.join().unwrap();
        assert_eq!(
            crate::process::observe_process(recorded).unwrap(),
            crate::process::ProcessObservation::Missing
        );
    }

    #[test]
    fn dispatch_only_pause_advertises_and_accepts_resume() {
        let controls = controls_for(
            WorkflowLifecycle::Running,
            false,
            Some(execution::DispatchState::Paused),
            true,
        );
        assert!(controls.resume);
        assert!(!controls.pause);

        let mut conn = connection();
        insert_run(&conn, "running", false, "paused");
        let (state, _) = apply_test_control(
            &mut conn,
            &workflow("running", false, "paused"),
            ControlAction::Resume,
        )
        .unwrap();
        assert_eq!(state, "queued");
    }

    #[test]
    fn linked_plan_owner_is_available_when_owner_is_not_projected() {
        let conn = connection();
        conn.execute_batch(
            "insert into auto_run (
               id, repo_root, worktree_path, branch, mode, variant, prompt_summary,
               initial_prompt, status, created_unix_ms, updated_unix_ms
             ) values ('auto-owner-12345678', '/repo', '/repo/wt', 'feature',
               'full', 'default', '', '', 'running', 1, 1);
             insert into auto_step_run (run_id, sequence, step_key, status, attempt, plan_run_id)
             values ('auto-owner-12345678', 1, 'run_plan', 'running', 1, 'plan-child');",
        )
        .unwrap();

        let owners = linked_plan_owners(&reader(&conn), Path::new("/repo")).unwrap();
        let owner = owners.get("plan-child").unwrap();
        assert_eq!(owner.kind, WorkflowKind::Auto);
        assert_eq!(owner.run_id, "auto-owner-12345678");
        assert_eq!(owner.display_id, "a:auto-own");
    }

    #[test]
    fn archived_auto_link_does_not_make_a_plan_uncontrollable() {
        let conn = connection();
        conn.execute_batch(
            "insert into auto_run (
               id, repo_root, worktree_path, branch, mode, variant, prompt_summary,
               initial_prompt, status, created_unix_ms, updated_unix_ms, archived_unix_ms
             ) values ('archived-owner', '/repo', '/repo/wt', 'feature',
               'full', 'default', '', '', 'done', 1, 1, 42);
             insert into auto_step_run (run_id, sequence, step_key, status, attempt, plan_run_id)
             values ('archived-owner', 1, 'run_plan', 'done', 1, 'active-plan');",
        )
        .unwrap();

        let owners = linked_plan_owners(&reader(&conn), Path::new("/repo")).unwrap();

        assert!(!owners.contains_key("active-plan"));
    }
}
