//! Fresh-session Agent execution for prompt-first Workflow Steps.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::process::{CancellationToken, Command, LiveProcessCompletion, Outcome, ProcessInput};

use super::step_trigger::{AgentOutcome, AgentOutcomeStatus};

static SESSION_SEQUENCE: AtomicU64 = AtomicU64::new(1);

struct InvocationCleanup(crate::harness::Invocation);

impl std::ops::Deref for InvocationCleanup {
    type Target = crate::harness::Invocation;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for InvocationCleanup {
    fn drop(&mut self) {
        self.0.cleanup();
    }
}

pub type AgentFuture<'a> =
    Pin<Box<dyn Future<Output = Result<AgentOutcome, AgentExecutionError>> + Send + 'a>>;

#[derive(Clone, Debug, Default)]
pub struct AgentCancellation(CancellationToken);

impl AgentCancellation {
    pub fn cancel(&self) {
        self.0.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }

    pub(crate) fn token(&self) -> CancellationToken {
        self.0.clone()
    }
}

#[derive(Clone, Debug)]
pub struct AgentRequest {
    pub run_id: String,
    pub step_key: String,
    pub attempt_id: String,
    pub repository: PathBuf,
    pub worktree: PathBuf,
    pub harness: Option<String>,
    pub model: Option<String>,
    pub variant: Option<String>,
    /// One authored turn. Only the initial turn may have selected predecessor context appended.
    pub prompt: String,
    /// Existing native session for an authored follow-up turn. `None` starts a fresh session.
    pub resume_session_id: Option<String>,
    /// Require the fresh turn to report a native session that can accept a follow-up.
    pub require_resumable_session: bool,
    pub cancellation: AgentCancellation,
}

pub trait AgentExecutor: Send + Sync + 'static {
    fn execute<'a>(&'a self, request: AgentRequest) -> AgentFuture<'a>;
}

#[derive(Clone, Debug)]
pub struct HarnessAgentExecutor {
    pub timeout: Duration,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
}

impl Default for HarnessAgentExecutor {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(60 * 60),
            stdout_bytes: 4 * 1024 * 1024,
            stderr_bytes: 1024 * 1024,
        }
    }
}

impl AgentExecutor for HarnessAgentExecutor {
    fn execute<'a>(&'a self, request: AgentRequest) -> AgentFuture<'a> {
        Box::pin(async move {
            if request.cancellation.is_cancelled() {
                return Err(AgentExecutionError::Cancelled);
            }
            let repository = crate::repo::Repository {
                root: request.repository.clone(),
            };
            let config = crate::config::Config::load(&repository);
            if !config.config_errors.is_empty() {
                return Err(AgentExecutionError::Configuration(
                    config.config_errors.join("; "),
                ));
            }
            let harness_id = request
                .harness
                .as_deref()
                .unwrap_or(&config.default_harness);
            let harness_config = config
                .harness_config(harness_id)
                .map_err(AgentExecutionError::Configuration)?;
            let harness = crate::harness::Harness::new(harness_id, &harness_config);
            let selection = crate::harness::AgentSelection {
                model: request.model.as_deref(),
                variant: request.variant.as_deref(),
            };
            let invocation = if let Some(session_id) = request.resume_session_id.as_deref() {
                harness.headless_resume_with_model(
                    &request.prompt,
                    &request.worktree,
                    session_id,
                    selection,
                )
            } else {
                harness.headless_with_model(
                    &request.prompt,
                    &request.worktree,
                    &format!("{} · {}", request.run_id, request.step_key),
                    None,
                    selection,
                    false,
                )
            }
            .map_err(AgentExecutionError::Configuration)?;
            execute_invocation(
                invocation,
                request,
                harness_config.adapter,
                self.timeout,
                self.stdout_bytes,
                self.stderr_bytes,
            )
            .await
        })
    }
}

async fn execute_invocation(
    invocation: crate::harness::Invocation,
    request: AgentRequest,
    adapter: String,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
) -> Result<AgentOutcome, AgentExecutionError> {
    let invocation = InvocationCleanup(invocation);
    let (program, args) = invocation
        .argv
        .split_first()
        .ok_or_else(|| AgentExecutionError::Configuration("harness invocation is empty".into()))?;
    let command = Command::new(program)
        .args(args)
        .current_dir(&request.worktree)
        .envs(&invocation.environment);
    let input = invocation
        .stdin
        .as_deref()
        .map_or(ProcessInput::Null, |input| {
            ProcessInput::Bytes(input.as_bytes())
        });
    let cancellation = request.cancellation.token();
    let mut process = crate::process::spawn_streaming_configured(
        command,
        crate::process::ProcessPolicy::WorkflowStep,
        timeout,
        input,
        Some(cancellation),
        crate::process::ProcessDescriptor::new("workflow.agent.execute"),
        crate::harness::WORKFLOW_OUTPUT_CHANNEL_CAPACITY,
        crate::harness::MAX_OUTPUT_LINE_BYTES,
        stdout_limit,
        stderr_limit,
    )
    .await
    .map_err(|error| {
        if request.cancellation.is_cancelled() {
            AgentExecutionError::Cancelled
        } else {
            AgentExecutionError::Protocol(format!("execute Agent subprocess: {error}"))
        }
    })?;
    let process_id = Some(process.pid());
    // Lines are a bounded live-display side channel only. Protocol parsing and
    // byte limits consume the independent raw captures so unterminated output,
    // non-UTF-8 bytes, CRLF, and large lines remain byte-exact.
    while process.next_line().await.is_some() {}
    let completion = process.wait().await;
    let (stdout, stderr) = process.captured_output();
    if request.cancellation.is_cancelled() {
        return Err(AgentExecutionError::Cancelled);
    }
    match completion {
        LiveProcessCompletion::Canceled => return Err(AgentExecutionError::Cancelled),
        LiveProcessCompletion::Exited(Outcome::TimedOut | Outcome::InactivityTimedOut) => {
            return Err(AgentExecutionError::Timeout(timeout));
        }
        LiveProcessCompletion::Failed(error) => {
            return Err(AgentExecutionError::Protocol(format!(
                "execute Agent subprocess: {error}"
            )));
        }
        LiveProcessCompletion::Exited(Outcome::Exited(0)) => {}
        LiveProcessCompletion::Exited(outcome) => {
            return Err(AgentExecutionError::Process {
                status: outcome.name().to_string(),
                diagnostic: single_line(&String::from_utf8_lossy(&stderr.bytes)),
            });
        }
    }
    if stdout.total_bytes > stdout_limit as u64 {
        return Err(AgentExecutionError::OutputLimit {
            stream: "stdout",
            limit: stdout_limit,
        });
    }
    if stderr.total_bytes > stderr_limit as u64 {
        return Err(AgentExecutionError::OutputLimit {
            stream: "stderr",
            limit: stderr_limit,
        });
    }
    let (native_session, final_text) = extract_agent_result(&adapter, &stdout.bytes);
    let final_text = final_text.ok_or_else(|| {
        AgentExecutionError::Protocol("successful Agent produced no final message".into())
    })?;
    if request.require_resumable_session
        && request.resume_session_id.is_none()
        && native_session.is_none()
    {
        return Err(AgentExecutionError::Protocol(format!(
            "{adapter} did not report the native Agent Session required for follow-ups"
        )));
    }
    if let (Some(expected), Some(observed)) = (
        request.resume_session_id.as_deref(),
        native_session.as_deref(),
    ) && expected != observed
    {
        return Err(AgentExecutionError::Protocol(format!(
            "follow-up resumed Agent Session {expected}, but {adapter} reported {observed}"
        )));
    }
    let resumed_session = request.resume_session_id.clone();
    Ok(AgentOutcome {
        status: AgentOutcomeStatus::Succeeded,
        process_id,
        session_id: native_session.or(resumed_session).unwrap_or_else(|| {
            format!(
                "{}:{}:{}",
                adapter,
                request.attempt_id,
                SESSION_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            )
        }),
        final_text,
    })
}

fn extract_agent_result(adapter: &str, stdout: &[u8]) -> (Option<String>, Option<String>) {
    let text = String::from_utf8_lossy(stdout);
    if adapter == "generic" {
        return (None, non_empty(text.trim()));
    }
    let mut session = None;
    let mut final_text = None;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        session =
            session.or_else(|| find_string(&value, &["session_id", "sessionID", "thread_id"]));
        if let Some(candidate) = agent_text(&value) {
            final_text = non_empty(candidate.trim());
        }
    }
    if final_text.is_none() {
        final_text = non_empty(text.trim());
    }
    (session, final_text)
}

fn agent_text(value: &serde_json::Value) -> Option<String> {
    for pointer in [
        "/result",
        "/text",
        "/message/text",
        "/message/content",
        "/item/text",
        "/content/0/text",
        "/message/content/0/text",
    ] {
        if let Some(text) = value.pointer(pointer).and_then(serde_json::Value::as_str) {
            return Some(text.to_string());
        }
    }
    None
}

fn find_string(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    match value {
        serde_json::Value::Object(object) => {
            for key in keys {
                if let Some(value) = object.get(*key).and_then(serde_json::Value::as_str) {
                    return Some(value.to_string());
                }
            }
            object.values().find_map(|value| find_string(value, keys))
        }
        serde_json::Value::Array(values) => {
            values.iter().find_map(|value| find_string(value, keys))
        }
        _ => None,
    }
}

fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_string())
}

pub fn prompt_with_context(authored_prompt: &str, context: &[(String, String)]) -> String {
    if context.is_empty() {
        return authored_prompt.to_string();
    }
    let mut prompt = authored_prompt.to_string();
    for (label, final_text) in context {
        prompt.push_str("\n\n--- Context from ");
        prompt.push_str(label);
        prompt.push_str(" ---\n");
        prompt.push_str(final_text);
    }
    prompt
}

#[derive(Debug)]
pub enum AgentExecutionError {
    Configuration(String),
    Protocol(String),
    Io(std::io::Error),
    Process { status: String, diagnostic: String },
    Timeout(Duration),
    OutputLimit { stream: &'static str, limit: usize },
    Cancelled,
}

impl std::fmt::Display for AgentExecutionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Configuration(error) | Self::Protocol(error) => formatter.write_str(error),
            Self::Io(error) => error.fmt(formatter),
            Self::Process { status, diagnostic } => {
                write!(formatter, "Agent exited with {status}: {diagnostic}")
            }
            Self::Timeout(timeout) => write!(formatter, "Agent exceeded {timeout:?}"),
            Self::OutputLimit { stream, limit } => {
                write!(formatter, "Agent {stream} exceeded {limit} bytes")
            }
            Self::Cancelled => formatter.write_str("Agent cancelled"),
        }
    }
}

impl std::error::Error for AgentExecutionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

fn single_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[derive(Clone, Default)]
pub struct RecordingAgentExecutor {
    requests: Arc<std::sync::Mutex<Vec<AgentRequest>>>,
    outcomes: Arc<std::sync::Mutex<std::collections::VecDeque<Result<AgentOutcome, String>>>>,
}

impl RecordingAgentExecutor {
    pub fn push_outcome(&self, outcome: AgentOutcome) {
        self.outcomes.lock().unwrap().push_back(Ok(outcome));
    }

    pub fn push_failure(&self, error: impl Into<String>) {
        self.outcomes.lock().unwrap().push_back(Err(error.into()));
    }

    pub fn requests(&self) -> Vec<AgentRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl AgentExecutor for RecordingAgentExecutor {
    fn execute<'a>(&'a self, request: AgentRequest) -> AgentFuture<'a> {
        self.requests.lock().unwrap().push(request);
        let result = self
            .outcomes
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err("no recorded Agent outcome remains".into()));
        Box::pin(async move { result.map_err(AgentExecutionError::Protocol) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authored_prompt_is_unchanged_without_context() {
        let prompt = "exact $HOME\nno json";
        assert_eq!(prompt_with_context(prompt, &[]), prompt);
    }

    #[test]
    fn context_is_plain_labeled_text() {
        assert_eq!(
            prompt_with_context(
                "Implement",
                &[
                    ("review-a".into(), "finding one".into()),
                    ("review-b".into(), "finding two".into())
                ]
            ),
            "Implement\n\n--- Context from review-a ---\nfinding one\n\n--- Context from review-b ---\nfinding two"
        );
    }

    #[tokio::test]
    async fn followup_requires_and_reuses_a_native_session_identity() {
        fn invocation() -> crate::harness::Invocation {
            crate::harness::Invocation {
                argv: if cfg!(windows) {
                    vec![
                        "powershell.exe".into(),
                        "-NoLogo".into(),
                        "-NoProfile".into(),
                        "-NonInteractive".into(),
                        "-Command".into(),
                        "Write-Output '{\"result\":\"done\"}'".into(),
                    ]
                } else {
                    vec![
                        "/bin/sh".into(),
                        "-c".into(),
                        "printf '%s\\n' '{\"result\":\"done\"}'".into(),
                    ]
                },
                environment: std::collections::BTreeMap::new(),
                stdin: None,
                prompt_file: None,
                structured_events: true,
                attach: false,
            }
        }

        let request = AgentRequest {
            run_id: "run".into(),
            step_key: "step".into(),
            attempt_id: "attempt".into(),
            repository: "/repo".into(),
            worktree: "/tmp".into(),
            harness: Some("pi".into()),
            model: None,
            variant: None,
            prompt: "follow up".into(),
            resume_session_id: Some("native-session".into()),
            require_resumable_session: false,
            cancellation: AgentCancellation::default(),
        };
        let outcome = execute_invocation(
            invocation(),
            request.clone(),
            "pi".into(),
            Duration::from_secs(5),
            1024,
            1024,
        )
        .await
        .unwrap();
        assert_eq!(outcome.session_id, "native-session");

        let error = execute_invocation(
            invocation(),
            AgentRequest {
                resume_session_id: None,
                require_resumable_session: true,
                ..request
            },
            "pi".into(),
            Duration::from_secs(5),
            1024,
            1024,
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("required for follow-ups"),
            "unexpected error: {error}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_agent_future_removes_temporary_prompt_artifact() {
        let root = std::env::temp_dir().join(format!(
            "prism-agent-cleanup-{}-{}",
            std::process::id(),
            crate::util::timestamp_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let prompt_file = root.join("prompt.txt");
        std::fs::write(&prompt_file, "temporary prompt").unwrap();
        let invocation = crate::harness::Invocation {
            argv: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
            environment: std::collections::BTreeMap::new(),
            stdin: None,
            prompt_file: Some(prompt_file.clone()),
            structured_events: false,
            attach: false,
        };
        let request = AgentRequest {
            run_id: "run".into(),
            step_key: "step".into(),
            attempt_id: "attempt".into(),
            repository: root.clone(),
            worktree: root.clone(),
            harness: Some("generic".into()),
            model: None,
            variant: None,
            prompt: "prompt".into(),
            resume_session_id: None,
            require_resumable_session: false,
            cancellation: AgentCancellation::default(),
        };
        let task = tokio::spawn(execute_invocation(
            invocation,
            request,
            "generic".into(),
            Duration::from_secs(60),
            1024,
            1024,
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;
        task.abort();
        let _ = task.await;

        assert!(!prompt_file.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn external_cancellation_wins_over_process_completion_shape() {
        let cancellation = AgentCancellation::default();
        let invocation = crate::harness::Invocation {
            argv: vec!["/bin/sh".into(), "-c".into(), "exec sleep 30".into()],
            environment: std::collections::BTreeMap::new(),
            stdin: None,
            prompt_file: None,
            structured_events: false,
            attach: false,
        };
        let request = AgentRequest {
            run_id: "run".into(),
            step_key: "step".into(),
            attempt_id: "attempt".into(),
            repository: "/tmp".into(),
            worktree: "/tmp".into(),
            harness: Some("generic".into()),
            model: None,
            variant: None,
            prompt: "prompt".into(),
            resume_session_id: None,
            require_resumable_session: false,
            cancellation: cancellation.clone(),
        };
        let task = tokio::spawn(execute_invocation(
            invocation,
            request,
            "generic".into(),
            Duration::from_secs(60),
            1024,
            1024,
        ));
        tokio::task::yield_now().await;
        cancellation.cancel();
        assert!(matches!(
            task.await.unwrap(),
            Err(AgentExecutionError::Cancelled)
        ));
    }

    #[tokio::test]
    async fn timeout_covers_blocked_stdin_delivery() {
        let invocation = crate::harness::Invocation {
            argv: if cfg!(windows) {
                vec![
                    "powershell.exe".into(),
                    "-NoLogo".into(),
                    "-NoProfile".into(),
                    "-NonInteractive".into(),
                    "-Command".into(),
                    "Start-Sleep -Seconds 10".into(),
                ]
            } else {
                vec!["/bin/sh".into(), "-c".into(), "sleep 10".into()]
            },
            environment: std::collections::BTreeMap::new(),
            stdin: Some("x".repeat(1024 * 1024)),
            prompt_file: None,
            structured_events: true,
            attach: false,
        };
        let error = tokio::time::timeout(
            Duration::from_secs(2),
            execute_invocation(
                invocation,
                AgentRequest {
                    run_id: "run".into(),
                    step_key: "step".into(),
                    attempt_id: "attempt".into(),
                    repository: "/tmp".into(),
                    worktree: "/tmp".into(),
                    harness: Some("pi".into()),
                    model: None,
                    variant: None,
                    prompt: "prompt".into(),
                    resume_session_id: None,
                    require_resumable_session: false,
                    cancellation: AgentCancellation::default(),
                },
                "pi".into(),
                Duration::from_millis(50),
                1024,
                1024,
            ),
        )
        .await
        .expect("Agent timeout supervision must not hang")
        .unwrap_err();
        assert!(matches!(error, AgentExecutionError::Timeout(_)));
    }

    #[tokio::test]
    async fn successful_leader_cleans_up_descendants_before_output_drain() {
        let invocation = crate::harness::Invocation {
            argv: if cfg!(windows) {
                vec![
                    "powershell.exe".into(),
                    "-NoLogo".into(),
                    "-NoProfile".into(),
                    "-NonInteractive".into(),
                    "-Command".into(),
                    concat!(
                        "$null = Start-Process -FilePath ping.exe ",
                        "-ArgumentList '-n','31','127.0.0.1' -NoNewWindow; ",
                        "Write-Output '{\"result\":\"done\"}'"
                    )
                    .into(),
                ]
            } else {
                vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "sleep 30 & printf '%s\\n' '{\"result\":\"done\"}'".into(),
                ]
            },
            environment: std::collections::BTreeMap::new(),
            stdin: None,
            prompt_file: None,
            structured_events: true,
            attach: false,
        };
        let outcome = tokio::time::timeout(
            Duration::from_secs(2),
            execute_invocation(
                invocation,
                AgentRequest {
                    run_id: "run".into(),
                    step_key: "step".into(),
                    attempt_id: "attempt".into(),
                    repository: "/tmp".into(),
                    worktree: "/tmp".into(),
                    harness: Some("pi".into()),
                    model: None,
                    variant: None,
                    prompt: "prompt".into(),
                    resume_session_id: None,
                    require_resumable_session: false,
                    cancellation: AgentCancellation::default(),
                },
                "pi".into(),
                Duration::from_secs(5),
                1024,
                1024,
            ),
        )
        .await
        .expect("descendant cleanup must close inherited pipes")
        .unwrap();
        assert_eq!(outcome.final_text, "done");
    }

    #[test]
    fn extracts_structured_final_text_without_requiring_application_json() {
        let output = br#"{"type":"thread.started","thread_id":"thread-1"}
{"type":"item.completed","item":{"type":"agent_message","text":"plain final answer"}}
"#;
        assert_eq!(
            extract_agent_result("codex", output),
            (Some("thread-1".into()), Some("plain final answer".into()))
        );
    }
}
