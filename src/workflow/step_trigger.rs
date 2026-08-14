//! Step Trigger lifecycle seam and bounded external process adapter.

use std::collections::VecDeque;
use std::future::Future;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::Command;

pub const TRIGGER_PROTOCOL_VERSION: u32 = 1;

pub type TriggerFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, TriggerError>> + Send + 'a>>;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TriggerSubject {
    pub repository: PathBuf,
    pub worktree: PathBuf,
    pub change_request: Option<String>,
    /// Launch-time head hint retained for history. Trigger cycles observe the association's
    /// provider-current exact head so Agent pushes can advance the run.
    pub change_request_head: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TriggerContext {
    pub run_id: String,
    pub step_key: String,
    pub attempt_id: String,
    pub cycle: u64,
    pub cycle_started_unix_ms: i64,
    pub subject: TriggerSubject,
    pub cancellation_requested: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum TriggerDecision {
    Run {
        summary: String,
    },
    Satisfied {
        summary: String,
    },
    Wait {
        summary: String,
        wake_at_unix_ms: i64,
    },
    Fail {
        reason: String,
    },
}

impl TriggerDecision {
    pub fn summary(&self) -> &str {
        match self {
            Self::Run { summary } | Self::Satisfied { summary } | Self::Wait { summary, .. } => {
                summary
            }
            Self::Fail { reason } => reason,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PreparedState(pub serde_json::Value);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentOutcomeStatus {
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentOutcome {
    pub status: AgentOutcomeStatus,
    #[serde(default)]
    pub process_id: Option<u32>,
    pub session_id: String,
    pub final_text: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum PostStepResult {
    Success {
        summary: String,
    },
    Wait {
        summary: String,
        wake_at_unix_ms: i64,
    },
    Fail {
        reason: String,
    },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TriggerRecoveryPolicy {
    pub prepare_repeatable: bool,
    pub finalize_repeatable: bool,
}

impl TriggerRecoveryPolicy {
    pub const REPEATABLE: Self = Self {
        prepare_repeatable: true,
        finalize_repeatable: true,
    };
    pub const UNCERTAIN: Self = Self {
        prepare_repeatable: false,
        finalize_repeatable: false,
    };
}

/// A Trigger observes whether one Agent Step should run and optionally performs work immediately
/// before and after that Agent. Trigger implementations never receive Workflow prompt text.
pub trait StepTrigger: Send + Sync + 'static {
    fn should_run_step<'a>(
        &'a self,
        context: &'a TriggerContext,
    ) -> TriggerFuture<'a, TriggerDecision>;

    fn pre_step_run<'a>(
        &'a self,
        _context: &'a TriggerContext,
    ) -> TriggerFuture<'a, PreparedState> {
        Box::pin(async { Ok(PreparedState::default()) })
    }

    fn post_step_run<'a>(
        &'a self,
        _context: &'a TriggerContext,
        _prepared: &'a PreparedState,
        _outcome: &'a AgentOutcome,
    ) -> TriggerFuture<'a, PostStepResult> {
        Box::pin(async {
            Ok(PostStepResult::Success {
                summary: "finalized".into(),
            })
        })
    }

    fn recovery_policy(&self) -> TriggerRecoveryPolicy {
        TriggerRecoveryPolicy::REPEATABLE
    }
}

#[derive(Clone, Default)]
pub struct TriggerRegistry {
    // Schedulers retain a clone of this registry. Shared interior mutability lets the Worker add
    // pinned external Triggers when a new immutable Workflow snapshot is launched.
    triggers: Arc<std::sync::RwLock<std::collections::BTreeMap<String, Arc<dyn StepTrigger>>>>,
}

impl TriggerRegistry {
    pub fn insert(
        &self,
        name: impl Into<String>,
        trigger: impl StepTrigger,
    ) -> Result<(), TriggerError> {
        self.insert_shared(name, Arc::new(trigger))
    }

    pub fn insert_shared(
        &self,
        name: impl Into<String>,
        trigger: Arc<dyn StepTrigger>,
    ) -> Result<(), TriggerError> {
        let name = name.into();
        let mut triggers = self
            .triggers
            .write()
            .map_err(|_| TriggerError::Protocol("Trigger registry lock is poisoned".into()))?;
        if let Some(existing) = triggers.get(&name) {
            if Arc::ptr_eq(existing, &trigger) {
                return Ok(());
            }
            return Err(TriggerError::Protocol(format!(
                "trigger '{name}' is already registered"
            )));
        }
        triggers.insert(name, trigger);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn StepTrigger>> {
        self.triggers.read().ok()?.get(name).cloned()
    }
}

#[derive(Clone, Debug)]
pub struct ExternalTriggerLimits {
    pub timeout: Duration,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
}

impl Default for ExternalTriggerLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            stdout_bytes: 64 * 1024,
            stderr_bytes: 16 * 1024,
        }
    }
}

#[derive(Clone)]
pub struct ExternalTrigger {
    executable: PathBuf,
    limits: ExternalTriggerLimits,
    recovery_policy: TriggerRecoveryPolicy,
}

impl ExternalTrigger {
    pub fn new(executable: impl Into<PathBuf>, limits: ExternalTriggerLimits) -> Self {
        Self {
            executable: executable.into(),
            limits,
            recovery_policy: TriggerRecoveryPolicy::UNCERTAIN,
        }
    }

    pub fn with_recovery_policy(mut self, recovery_policy: TriggerRecoveryPolicy) -> Self {
        self.recovery_policy = recovery_policy;
        self
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    async fn invoke(
        &self,
        request: TriggerPhaseRequest,
    ) -> Result<TriggerPhaseResponse, TriggerError> {
        let metadata = std::fs::metadata(&self.executable).map_err(TriggerError::Io)?;
        if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
            return Err(TriggerError::Protocol(format!(
                "trigger {} is not executable",
                self.executable.display()
            )));
        }
        let mut command = Command::new(&self.executable);
        command.as_std_mut().process_group(0);
        let mut child = command
            .kill_on_drop(true)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(TriggerError::Io)?;
        let process_id = child.id();
        let mut process_group = process_id.map(ProcessGroupGuard::new);
        let body = serde_json::to_vec(&request)
            .map_err(|error| TriggerError::Protocol(error.to_string()))?;
        let mut stdin = child.stdin.take().ok_or_else(|| {
            TriggerError::Protocol("external trigger stdin was not captured".into())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            TriggerError::Protocol("external trigger stdout was not captured".into())
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            TriggerError::Protocol("external trigger stderr was not captured".into())
        })?;
        let stdout_limit = self.limits.stdout_bytes;
        let stderr_limit = self.limits.stderr_bytes;
        let stdout_task = tokio::spawn(read_bounded(stdout, stdout_limit));
        let stderr_task = tokio::spawn(read_bounded(stderr, stderr_limit));
        let mut stdin_task = tokio::spawn(async move {
            stdin.write_all(&body).await.map_err(TriggerError::Io)?;
            stdin.write_all(b"\n").await.map_err(TriggerError::Io)
        });
        let status = match tokio::time::timeout(self.limits.timeout, async {
            let status = child.wait().await.map_err(TriggerError::Io)?;
            (&mut stdin_task).await.map_err(|error| {
                TriggerError::Protocol(format!("join trigger stdin: {error}"))
            })??;
            Ok::<_, TriggerError>(status)
        })
        .await
        {
            Ok(status) => status?,
            Err(_) => {
                if let Some(process_id) = process_id {
                    let _ = crate::system::process::send_process_group_signal(
                        process_id,
                        libc::SIGKILL,
                    );
                }
                let _ = child.kill().await;
                let _ = child.wait().await;
                stdin_task.abort();
                let _ = stdin_task.await;
                if let Some(process_group) = &mut process_group {
                    process_group.disarm();
                }
                stdout_task.abort();
                stderr_task.abort();
                let _ = stdout_task.await;
                let _ = stderr_task.await;
                let stderr = String::new();
                return Err(TriggerError::Timeout {
                    executable: self.executable.clone(),
                    diagnostic: stderr,
                });
            }
        };
        if let Some(process_id) = process_id {
            let _ = crate::system::process::send_process_group_signal(process_id, libc::SIGKILL);
        }
        let drain_timeout = self.limits.timeout.min(Duration::from_secs(5));
        let (stdout, stderr) = match tokio::time::timeout(drain_timeout, async {
            let stdout = stdout_task.await.map_err(|error| {
                TriggerError::Protocol(format!("join trigger stdout: {error}"))
            })??;
            let stderr = stderr_task.await.map_err(|error| {
                TriggerError::Protocol(format!("join trigger stderr: {error}"))
            })??;
            Ok::<_, TriggerError>((stdout, stderr))
        })
        .await
        {
            Ok(output) => output?,
            Err(_) => {
                return Err(TriggerError::Timeout {
                    executable: self.executable.clone(),
                    diagnostic: "Trigger output streams did not close".into(),
                });
            }
        };
        if let Some(process_group) = &mut process_group {
            process_group.disarm();
        }
        let diagnostic = String::from_utf8_lossy(&stderr.bytes).into_owned();
        if !status.success() {
            return Err(TriggerError::Process {
                executable: self.executable.clone(),
                status: status.to_string(),
                diagnostic,
            });
        }
        if stdout.truncated {
            return Err(TriggerError::Protocol(format!(
                "trigger {} exceeded its {} byte stdout limit",
                self.executable.display(),
                self.limits.stdout_bytes
            )));
        }
        let response: TriggerPhaseResponse =
            serde_json::from_slice(&stdout.bytes).map_err(|error| {
                TriggerError::Protocol(format!(
                    "invalid response from trigger {}: {error}; stderr: {}",
                    self.executable.display(),
                    single_line(&diagnostic)
                ))
            })?;
        let version = match &response {
            TriggerPhaseResponse::Decision {
                protocol_version, ..
            }
            | TriggerPhaseResponse::Prepared {
                protocol_version, ..
            }
            | TriggerPhaseResponse::Completed {
                protocol_version, ..
            } => *protocol_version,
        };
        if version != TRIGGER_PROTOCOL_VERSION {
            return Err(TriggerError::Protocol(format!(
                "trigger {} returned protocol version {version}; expected {TRIGGER_PROTOCOL_VERSION}",
                self.executable.display()
            )));
        }
        Ok(response)
    }
}

impl StepTrigger for ExternalTrigger {
    fn should_run_step<'a>(
        &'a self,
        context: &'a TriggerContext,
    ) -> TriggerFuture<'a, TriggerDecision> {
        Box::pin(async move {
            match self
                .invoke(TriggerPhaseRequest::check(context.clone()))
                .await?
            {
                TriggerPhaseResponse::Decision { decision, .. } => Ok(decision),
                response => Err(unexpected_response("check", &response)),
            }
        })
    }

    fn pre_step_run<'a>(&'a self, context: &'a TriggerContext) -> TriggerFuture<'a, PreparedState> {
        Box::pin(async move {
            match self
                .invoke(TriggerPhaseRequest::prepare(context.clone()))
                .await?
            {
                TriggerPhaseResponse::Prepared { prepared_state, .. } => Ok(prepared_state),
                response => Err(unexpected_response("prepare", &response)),
            }
        })
    }

    fn post_step_run<'a>(
        &'a self,
        context: &'a TriggerContext,
        prepared: &'a PreparedState,
        outcome: &'a AgentOutcome,
    ) -> TriggerFuture<'a, PostStepResult> {
        Box::pin(async move {
            match self
                .invoke(TriggerPhaseRequest::finalize(
                    context.clone(),
                    prepared.clone(),
                    outcome.clone(),
                ))
                .await?
            {
                TriggerPhaseResponse::Completed { completion, .. } => Ok(completion),
                response => Err(unexpected_response("finalize", &response)),
            }
        })
    }

    fn recovery_policy(&self) -> TriggerRecoveryPolicy {
        self.recovery_policy
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TriggerPhaseEnvelope<T> {
    pub protocol_version: u32,
    #[serde(flatten)]
    pub body: T,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum TriggerPhaseBody {
    #[serde(rename = "should_run_step")]
    Check { context: TriggerContext },
    #[serde(rename = "pre_step_run")]
    Prepare { context: TriggerContext },
    #[serde(rename = "post_step_run")]
    Finalize {
        context: TriggerContext,
        prepared_state: PreparedState,
        agent_outcome: AgentOutcome,
    },
}

pub type TriggerPhaseRequest = TriggerPhaseEnvelope<TriggerPhaseBody>;

impl TriggerPhaseRequest {
    pub fn check(context: TriggerContext) -> Self {
        Self {
            protocol_version: TRIGGER_PROTOCOL_VERSION,
            body: TriggerPhaseBody::Check { context },
        }
    }

    pub fn prepare(context: TriggerContext) -> Self {
        Self {
            protocol_version: TRIGGER_PROTOCOL_VERSION,
            body: TriggerPhaseBody::Prepare { context },
        }
    }

    pub fn finalize(
        context: TriggerContext,
        prepared_state: PreparedState,
        agent_outcome: AgentOutcome,
    ) -> Self {
        Self {
            protocol_version: TRIGGER_PROTOCOL_VERSION,
            body: TriggerPhaseBody::Finalize {
                context,
                prepared_state,
                agent_outcome,
            },
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "response", rename_all = "snake_case")]
pub enum TriggerPhaseResponse {
    Decision {
        protocol_version: u32,
        decision: TriggerDecision,
    },
    Prepared {
        protocol_version: u32,
        prepared_state: PreparedState,
    },
    Completed {
        protocol_version: u32,
        completion: PostStepResult,
    },
}

fn unexpected_response(phase: &str, response: &TriggerPhaseResponse) -> TriggerError {
    TriggerError::Protocol(format!(
        "trigger returned {} response during {phase}",
        match response {
            TriggerPhaseResponse::Decision { .. } => "decision",
            TriggerPhaseResponse::Prepared { .. } => "prepared",
            TriggerPhaseResponse::Completed { .. } => "completed",
        }
    ))
}

struct ProcessGroupGuard {
    process_id: u32,
    armed: bool,
}

impl ProcessGroupGuard {
    fn new(process_id: u32) -> Self {
        Self {
            process_id,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ =
                crate::system::process::send_process_group_signal(self.process_id, libc::SIGKILL);
        }
    }
}

struct BoundedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

async fn read_bounded(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
) -> Result<BoundedOutput, TriggerError> {
    let mut output = Vec::with_capacity(limit.min(8192));
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;
    loop {
        let count = reader.read(&mut buffer).await.map_err(TriggerError::Io)?;
        if count == 0 {
            return Ok(BoundedOutput {
                bytes: output,
                truncated,
            });
        }
        let remaining = limit.saturating_sub(output.len());
        output.extend_from_slice(&buffer[..count.min(remaining)]);
        truncated |= count > remaining;
    }
}

#[derive(Clone, Debug)]
pub struct TriggerExecutableSnapshot {
    pub digest: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct TriggerSnapshotStore {
    root: PathBuf,
}

impl TriggerSnapshotStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn retain(&self, executable: &Path) -> Result<TriggerExecutableSnapshot, TriggerError> {
        let bytes = std::fs::read(executable).map_err(TriggerError::Io)?;
        self.retain_bytes(executable, &bytes)
    }

    pub fn retain_bytes(
        &self,
        executable: &Path,
        bytes: &[u8],
    ) -> Result<TriggerExecutableSnapshot, TriggerError> {
        if !bytes.starts_with(b"#!") {
            return Err(TriggerError::Protocol(format!(
                "trigger {} does not begin with a shebang",
                executable.display()
            )));
        }
        let digest = format!("sha256:{:x}", Sha256::digest(bytes));
        let path = self.root.join(digest.trim_start_matches("sha256:"));
        if path.exists() {
            let actual = std::fs::read(&path).map_err(TriggerError::Io)?;
            if actual != bytes {
                return Err(TriggerError::Protocol(format!(
                    "trigger snapshot {} has conflicting bytes",
                    path.display()
                )));
            }
        } else {
            std::fs::create_dir_all(&self.root).map_err(TriggerError::Io)?;
            static TEMPORARY_SEQUENCE: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(1);
            let temporary = self.root.join(format!(
                ".{}-{}-{}.tmp",
                digest.trim_start_matches("sha256:"),
                std::process::id(),
                TEMPORARY_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::write(&temporary, bytes).map_err(TriggerError::Io)?;
            std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o700))
                .map_err(TriggerError::Io)?;
            std::fs::File::open(&temporary)
                .and_then(|file| file.sync_all())
                .map_err(TriggerError::Io)?;
            match std::fs::rename(&temporary, &path) {
                Ok(()) => {}
                Err(_) if path.is_file() => {
                    let _ = std::fs::remove_file(temporary);
                }
                Err(error) => return Err(TriggerError::Io(error)),
            }
            crate::durability::sync_directory(
                &self.root,
                crate::durability::DurabilityIntent::Standard,
            )
            .map_err(TriggerError::Io)?;
        }
        Ok(TriggerExecutableSnapshot { digest, path })
    }
}

/// Replace mutable external Trigger paths in a run snapshot with retained executable copies.
/// The source digest must still match, so a concurrent edit fails instead of changing the run.
pub fn pin_workflow_triggers(
    workflow: &mut crate::workflow::source::CompiledWorkflow,
    store: &TriggerSnapshotStore,
) -> Result<Vec<TriggerExecutableSnapshot>, TriggerError> {
    let mut snapshots = Vec::new();
    for step in &mut workflow.steps {
        let Some(trigger) = &mut step.trigger else {
            continue;
        };
        let Some(executable) = &trigger.executable else {
            continue;
        };
        let snapshot = if let Some(bytes) = trigger.captured_bytes.take() {
            store.retain_bytes(executable, &bytes)?
        } else {
            store.retain(executable)?
        };
        if snapshot.digest != trigger.digest {
            return Err(TriggerError::Protocol(format!(
                "Trigger '{}' changed while the Workflow snapshot was being retained",
                trigger.name
            )));
        }
        trigger.executable = Some(snapshot.path.clone());
        snapshots.push(snapshot);
    }
    workflow.digest.clear();
    let canonical =
        serde_json::to_vec(workflow).map_err(|error| TriggerError::Protocol(error.to_string()))?;
    workflow.digest = format!("sha256:{:x}", Sha256::digest(canonical));
    Ok(snapshots)
}

/// Deterministic Trigger fixture. Decisions and hook results are consumed in insertion order.
#[derive(Clone, Default)]
pub struct ScriptedTrigger {
    decisions: Arc<Mutex<VecDeque<Result<TriggerDecision, TriggerError>>>>,
    preparations: Arc<Mutex<VecDeque<Result<PreparedState, TriggerError>>>>,
    completions: Arc<Mutex<VecDeque<Result<PostStepResult, TriggerError>>>>,
    recovery: TriggerRecoveryPolicy,
}

impl ScriptedTrigger {
    pub fn new(decisions: impl IntoIterator<Item = TriggerDecision>) -> Self {
        Self {
            decisions: Arc::new(Mutex::new(
                decisions.into_iter().map(Ok).collect::<VecDeque<_>>(),
            )),
            preparations: Arc::new(Mutex::new(VecDeque::new())),
            completions: Arc::new(Mutex::new(VecDeque::new())),
            recovery: TriggerRecoveryPolicy::REPEATABLE,
        }
    }

    pub fn push_preparation(&self, result: Result<PreparedState, TriggerError>) {
        self.preparations.lock().unwrap().push_back(result);
    }

    pub fn push_completion(&self, result: Result<PostStepResult, TriggerError>) {
        self.completions.lock().unwrap().push_back(result);
    }

    pub fn push_decision(&self, decision: TriggerDecision) {
        self.decisions.lock().unwrap().push_back(Ok(decision));
    }

    pub fn with_recovery_policy(mut self, policy: TriggerRecoveryPolicy) -> Self {
        self.recovery = policy;
        self
    }
}

impl StepTrigger for ScriptedTrigger {
    fn should_run_step<'a>(
        &'a self,
        _context: &'a TriggerContext,
    ) -> TriggerFuture<'a, TriggerDecision> {
        let result = self
            .decisions
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err(TriggerError::Fixture("no scripted decision remains".into())));
        Box::pin(async move { result })
    }

    fn pre_step_run<'a>(
        &'a self,
        _context: &'a TriggerContext,
    ) -> TriggerFuture<'a, PreparedState> {
        let result = self
            .preparations
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(PreparedState::default()));
        Box::pin(async move { result })
    }

    fn post_step_run<'a>(
        &'a self,
        _context: &'a TriggerContext,
        _prepared: &'a PreparedState,
        _outcome: &'a AgentOutcome,
    ) -> TriggerFuture<'a, PostStepResult> {
        let result = self
            .completions
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| {
                Ok(PostStepResult::Success {
                    summary: "scripted completion".into(),
                })
            });
        Box::pin(async move { result })
    }

    fn recovery_policy(&self) -> TriggerRecoveryPolicy {
        self.recovery
    }
}

#[derive(Debug)]
pub enum TriggerError {
    Io(std::io::Error),
    Timeout {
        executable: PathBuf,
        diagnostic: String,
    },
    Process {
        executable: PathBuf,
        status: String,
        diagnostic: String,
    },
    Protocol(String),
    Fixture(String),
}

impl std::fmt::Display for TriggerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => error.fmt(formatter),
            Self::Protocol(error) | Self::Fixture(error) => formatter.write_str(error),
            Self::Timeout {
                executable,
                diagnostic,
            } => write!(
                formatter,
                "trigger {} timed out: {}",
                executable.display(),
                single_line(diagnostic)
            ),
            Self::Process {
                executable,
                status,
                diagnostic,
            } => write!(
                formatter,
                "trigger {} exited with {status}: {}",
                executable.display(),
                single_line(diagnostic)
            ),
        }
    }
}

impl std::error::Error for TriggerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for TriggerError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

fn single_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> TriggerContext {
        TriggerContext {
            run_id: "run".into(),
            step_key: "review".into(),
            attempt_id: "attempt".into(),
            cycle: 2,
            cycle_started_unix_ms: 1,
            subject: TriggerSubject {
                repository: "/repo".into(),
                worktree: "/repo/worktree".into(),
                change_request: Some("github:example/repo:42".into()),
                change_request_head: Some("abc".into()),
            },
            cancellation_requested: false,
        }
    }

    #[tokio::test]
    async fn scripted_trigger_covers_all_decisions_and_hooks() {
        let trigger = ScriptedTrigger::new([
            TriggerDecision::Run {
                summary: "threads found".into(),
            },
            TriggerDecision::Satisfied {
                summary: "clean".into(),
            },
            TriggerDecision::Wait {
                summary: "checks pending".into(),
                wake_at_unix_ms: 20,
            },
            TriggerDecision::Fail {
                reason: "unsupported".into(),
            },
        ]);
        trigger.push_preparation(Ok(PreparedState(serde_json::json!({"threads":[1]}))));
        trigger.push_completion(Ok(PostStepResult::Success {
            summary: "resolved".into(),
        }));
        assert!(matches!(
            trigger.should_run_step(&context()).await.unwrap(),
            TriggerDecision::Run { .. }
        ));
        assert_eq!(
            trigger.pre_step_run(&context()).await.unwrap().0,
            serde_json::json!({"threads":[1]})
        );
        let outcome = AgentOutcome {
            status: AgentOutcomeStatus::Succeeded,
            process_id: Some(42),
            session_id: "session".into(),
            final_text: "done".into(),
        };
        assert!(matches!(
            trigger
                .post_step_run(&context(), &PreparedState::default(), &outcome)
                .await
                .unwrap(),
            PostStepResult::Success { .. }
        ));
        assert!(matches!(
            trigger.should_run_step(&context()).await.unwrap(),
            TriggerDecision::Satisfied { .. }
        ));
        assert!(matches!(
            trigger.should_run_step(&context()).await.unwrap(),
            TriggerDecision::Wait { .. }
        ));
        assert!(matches!(
            trigger.should_run_step(&context()).await.unwrap(),
            TriggerDecision::Fail { .. }
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn external_trigger_timeout_covers_blocked_stdin_delivery() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = std::env::temp_dir().join(format!(
            "prism-blocked-trigger-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let executable = root.join("trigger");
        std::fs::write(&executable, "#!/bin/sh\nsleep 10\n").unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions).unwrap();
        let trigger = ExternalTrigger::new(
            &executable,
            ExternalTriggerLimits {
                timeout: Duration::from_millis(50),
                ..ExternalTriggerLimits::default()
            },
        );
        let mut request = TriggerPhaseRequest::check(context());
        if let TriggerPhaseBody::Check { context } = &mut request.body {
            context.subject.change_request = Some("x".repeat(1024 * 1024));
        }
        let error = tokio::time::timeout(Duration::from_secs(2), trigger.invoke(request))
            .await
            .expect("Trigger timeout supervision must not hang")
            .unwrap_err();
        assert!(matches!(error, TriggerError::Timeout { .. }));
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn external_trigger_uses_bounded_versioned_json_lifecycle() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = std::env::temp_dir().join(format!(
            "prism-external-trigger-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let executable = root.join("trigger");
        std::fs::write(
            &executable,
            r#"#!/bin/sh
IFS= read -r request
case "$request" in
  *should_run_step*) printf '%s\n' '{"response":"decision","protocol_version":1,"decision":{"decision":"run","summary":"work found"}}' ;;
  *pre_step_run*) printf '%s\n' '{"response":"prepared","protocol_version":1,"prepared_state":{"captured":["T1"]}}' ;;
  *post_step_run*) printf '%s\n' '{"response":"completed","protocol_version":1,"completion":{"result":"success","summary":"done"}}' ;;
  *) exit 4 ;;
esac
"#,
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions).unwrap();
        let trigger = ExternalTrigger::new(executable, ExternalTriggerLimits::default());

        assert!(matches!(
            trigger.should_run_step(&context()).await.unwrap(),
            TriggerDecision::Run { .. }
        ));
        let prepared = trigger.pre_step_run(&context()).await.unwrap();
        assert_eq!(prepared.0["captured"][0], "T1");
        assert!(matches!(
            trigger
                .post_step_run(
                    &context(),
                    &prepared,
                    &AgentOutcome {
                        status: AgentOutcomeStatus::Succeeded,
                        process_id: Some(42),
                        session_id: "fresh".into(),
                        final_text: "plain final".into(),
                    },
                )
                .await
                .unwrap(),
            PostStepResult::Success { .. }
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn snapshots_pin_executable_bytes() {
        let root = std::env::temp_dir().join(format!(
            "prism-trigger-snapshot-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let executable = root.join("trigger");
        std::fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        let snapshot = TriggerSnapshotStore::new(root.join("snapshots"))
            .retain(&executable)
            .unwrap();
        std::fs::write(&executable, "#!/bin/sh\nexit 1\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(snapshot.path).unwrap(),
            "#!/bin/sh\nexit 0\n"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
