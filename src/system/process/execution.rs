//! Async one-shot ProcessKit execution and Prism result mapping.

use std::error::Error;
use std::fmt;
use std::time::{Duration, Instant};

use processkit::{Command, Error as ProcessKitError, Outcome, OutputBufferPolicy, Stdin};

use super::capture::{BoundedCapture, CapturedBytes};
use super::policy::{ProcessDescriptor, ProcessPolicy, infer_descriptor};
use super::telemetry::ProcessTelemetry;
use super::{CancellationToken, current_cancellation};
use crate::observability::{self, LogLevel};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessCompletion {
    Exited,
    Signaled,
    DeadlineExceeded,
}

impl ProcessCompletion {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Exited => "exited",
            Self::Signaled => "signaled",
            Self::DeadlineExceeded => "deadline_exceeded",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TerminationStage {
    #[default]
    None,
    Managed,
}

impl TerminationStage {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Managed => "managed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessStatus {
    code: Option<i32>,
    signal: Option<i32>,
}

impl ProcessStatus {
    pub const fn code(self) -> Option<i32> {
        self.code
    }

    pub const fn signal(self) -> Option<i32> {
        self.signal
    }

    pub const fn success(self) -> bool {
        matches!(self.code, Some(0))
    }
}

#[cfg(test)]
impl ProcessOutput {
    pub(crate) fn successful_for_test(
        stdout: impl Into<Vec<u8>>,
        stderr: impl Into<Vec<u8>>,
    ) -> Self {
        let stdout = stdout.into();
        let stderr = stderr.into();
        Self {
            status: ProcessStatus {
                code: Some(0),
                signal: None,
            },
            completion: ProcessCompletion::Exited,
            termination_stage: TerminationStage::None,
            stdout_total_bytes: stdout.len() as u64,
            stdout_truncated: false,
            stderr_total_bytes: stderr.len() as u64,
            stderr_truncated: false,
            stdout,
            stderr,
            elapsed: Duration::ZERO,
            deadline: Duration::ZERO,
            child_pid: 0,
        }
    }
}

impl fmt::Display for ProcessStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.code, self.signal) {
            (Some(code), _) => write!(formatter, "exit status: {code}"),
            (_, Some(signal)) => write!(formatter, "signal: {signal}"),
            _ => formatter.write_str("managed termination"),
        }
    }
}

#[derive(Debug)]
pub struct ProcessOutput {
    pub status: ProcessStatus,
    pub completion: ProcessCompletion,
    pub termination_stage: TerminationStage,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_total_bytes: u64,
    pub stdout_truncated: bool,
    pub stderr_total_bytes: u64,
    pub stderr_truncated: bool,
    pub elapsed: Duration,
    pub deadline: Duration,
    pub child_pid: u32,
}

#[derive(Clone, Copy, Debug)]
pub enum ProcessInput<'a> {
    Null,
    Bytes(&'a [u8]),
}

#[allow(dead_code)]
#[derive(Debug)]
pub struct ProcessExecutionError {
    source: Option<ProcessKitError>,
    message: Option<&'static str>,
    pub stdout: CapturedBytes,
    pub stderr: CapturedBytes,
    pub elapsed: Duration,
    pub child_pid: Option<u32>,
}

impl ProcessExecutionError {
    pub fn kind(&self) -> processkit::ErrorKind {
        self.source
            .as_ref()
            .map_or(processkit::ErrorKind::Other, ProcessKitError::kind)
    }
}

impl fmt::Display for ProcessExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(source) = self.source.as_ref() {
            source.fmt(formatter)
        } else {
            formatter.write_str(self.message.unwrap_or("subprocess capture was incomplete"))
        }
    }
}

impl Error for ProcessExecutionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_ref()
            .map(|source| source as &(dyn Error + 'static))
    }
}

#[cfg(all(test, unix))]
pub async fn execute(
    command: Command,
    policy: ProcessPolicy,
    input: ProcessInput<'_>,
    cancellation: Option<CancellationToken>,
) -> Result<ProcessOutput, ProcessExecutionError> {
    let descriptor = infer_descriptor(&command);
    execute_named(command, policy, input, cancellation, descriptor).await
}

pub async fn execute_named(
    command: Command,
    policy: ProcessPolicy,
    input: ProcessInput<'_>,
    cancellation: Option<CancellationToken>,
    descriptor: ProcessDescriptor,
) -> Result<ProcessOutput, ProcessExecutionError> {
    execute_named_with_level(
        command,
        policy,
        input,
        cancellation,
        descriptor,
        LogLevel::Error,
    )
    .await
}

async fn execute_named_with_level(
    command: Command,
    policy: ProcessPolicy,
    input: ProcessInput<'_>,
    cancellation: Option<CancellationToken>,
    descriptor: ProcessDescriptor,
    failure_level: LogLevel,
) -> Result<ProcessOutput, ProcessExecutionError> {
    let settings = policy.settings();
    execute_configured(
        command,
        policy,
        input,
        cancellation,
        descriptor,
        failure_level,
        settings.deadline,
        settings.termination_grace,
        BoundedCapture::tail(settings.capture_bytes),
        BoundedCapture::tail(settings.capture_bytes),
        settings.capture_bytes,
    )
    .await
}

/// Execute one command while retaining independent bounded byte prefixes.
///
/// This is the workflow protocol adapter: ProcessKit still owns spawning, async
/// stdin, draining, timeout/cancellation escalation, tree containment, and reaping.
#[allow(clippy::too_many_arguments)]
pub async fn execute_prefix_bounded(
    command: Command,
    policy: ProcessPolicy,
    timeout: Duration,
    stdout_bytes: usize,
    stderr_bytes: usize,
    input: ProcessInput<'_>,
    cancellation: Option<CancellationToken>,
    descriptor: ProcessDescriptor,
) -> Result<ProcessOutput, ProcessExecutionError> {
    execute_configured(
        command,
        policy,
        input,
        cancellation,
        descriptor,
        LogLevel::Error,
        timeout,
        policy.settings().termination_grace,
        BoundedCapture::prefix(stdout_bytes),
        BoundedCapture::prefix(stderr_bytes),
        stdout_bytes.max(stderr_bytes),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn execute_configured(
    command: Command,
    policy: ProcessPolicy,
    input: ProcessInput<'_>,
    cancellation: Option<CancellationToken>,
    descriptor: ProcessDescriptor,
    failure_level: LogLevel,
    deadline: Duration,
    termination_grace: Duration,
    stdout_capture: BoundedCapture,
    stderr_capture: BoundedCapture,
    pump_buffer_bytes: usize,
) -> Result<ProcessOutput, ProcessExecutionError> {
    let mut configured = command
        .clone()
        .timeout(deadline)
        .timeout_grace(termination_grace)
        .cancel_grace(termination_grace)
        .output_buffer(OutputBufferPolicy::bounded(0).with_max_bytes(pump_buffer_bytes))
        .stdout_raw_tee(stdout_capture.clone())
        .stderr_raw_tee(stderr_capture.clone());
    configured = match input {
        // Preserve a caller-configured ProcessKit stdin source (for example a
        // picker candidate list). A fresh Command's default remains closed.
        ProcessInput::Null => configured,
        ProcessInput::Bytes(bytes) => configured.stdin(Stdin::from_bytes(bytes.to_vec())),
    };
    if let Some(token) = cancellation.or_else(current_cancellation) {
        configured = configured.cancel_on(token);
    }

    let started = Instant::now();
    let mut telemetry = ProcessTelemetry::begin(&command, policy, descriptor, deadline);
    let process = match configured.start().await {
        Ok(process) => process,
        Err(source) => {
            let stdout = stdout_capture.snapshot();
            let stderr = stderr_capture.snapshot();
            telemetry.finish_error(
                &source,
                started.elapsed(),
                None,
                stdout.total_bytes,
                stderr.total_bytes,
                stdout.truncated,
                stderr.truncated,
            );
            return Err(ProcessExecutionError {
                source: Some(source),
                message: None,
                stdout,
                stderr,
                elapsed: started.elapsed(),
                child_pid: None,
            });
        }
    };
    let child_pid = process.pid();
    let result = match process.output_string().await {
        Ok(result) => result,
        Err(source) => {
            let stdout = stdout_capture.snapshot();
            let stderr = stderr_capture.snapshot();
            telemetry.finish_error(
                &source,
                started.elapsed(),
                child_pid,
                stdout.total_bytes,
                stderr.total_bytes,
                stdout.truncated,
                stderr.truncated,
            );
            return Err(ProcessExecutionError {
                source: Some(source),
                message: None,
                stdout,
                stderr,
                elapsed: started.elapsed(),
                child_pid,
            });
        }
    };
    let stdout = stdout_capture.snapshot();
    let stderr = stderr_capture.snapshot();
    if !stdout.complete || !stderr.complete {
        telemetry.finish_capture_error(
            started.elapsed(),
            child_pid,
            stdout.total_bytes,
            stderr.total_bytes,
            stdout.truncated,
            stderr.truncated,
        );
        return Err(ProcessExecutionError {
            source: None,
            message: Some("subprocess output pipes did not fully drain before completion"),
            stdout,
            stderr,
            elapsed: started.elapsed(),
            child_pid,
        });
    }
    let completion = match result.outcome() {
        Outcome::Exited(_) => ProcessCompletion::Exited,
        Outcome::Signalled(_) => ProcessCompletion::Signaled,
        Outcome::TimedOut | Outcome::InactivityTimedOut => ProcessCompletion::DeadlineExceeded,
        _ => ProcessCompletion::Signaled,
    };
    let output = ProcessOutput {
        status: ProcessStatus {
            code: result.code(),
            signal: result.signal(),
        },
        completion,
        termination_stage: if matches!(completion, ProcessCompletion::DeadlineExceeded) {
            TerminationStage::Managed
        } else {
            TerminationStage::None
        },
        stdout: stdout.bytes,
        stderr: stderr.bytes,
        stdout_total_bytes: stdout.total_bytes,
        stdout_truncated: stdout.truncated,
        stderr_total_bytes: stderr.total_bytes,
        stderr_truncated: stderr.truncated,
        elapsed: result.duration(),
        deadline,
        child_pid: child_pid.unwrap_or_default(),
    };
    telemetry.finish_output(&output, failure_level);
    Ok(output)
}

#[cfg(all(test, unix))]
pub async fn run_output(command: Command, policy: ProcessPolicy) -> Result<ProcessOutput, String> {
    let descriptor = infer_descriptor(&command);
    run_output_named(command, policy, descriptor).await
}

pub async fn run_output_named(
    command: Command,
    policy: ProcessPolicy,
    descriptor: ProcessDescriptor,
) -> Result<ProcessOutput, String> {
    run_output_with_failure_level(command, policy, LogLevel::Error, descriptor).await
}

pub async fn run_output_allow_failure(
    command: Command,
    policy: ProcessPolicy,
) -> Result<ProcessOutput, String> {
    let descriptor = infer_descriptor(&command);
    run_output_allow_failure_named(command, policy, descriptor).await
}

pub async fn run_output_allow_failure_named(
    command: Command,
    policy: ProcessPolicy,
    descriptor: ProcessDescriptor,
) -> Result<ProcessOutput, String> {
    run_output_with_failure_level(command, policy, LogLevel::Debug, descriptor).await
}

async fn run_output_with_failure_level(
    command: Command,
    policy: ProcessPolicy,
    failure_level: LogLevel,
    descriptor: ProcessDescriptor,
) -> Result<ProcessOutput, String> {
    let command_display = command_display(&command);
    let output = execute_named_with_level(
        command,
        policy,
        ProcessInput::Null,
        None,
        descriptor,
        failure_level,
    )
    .await
    .map_err(|error| execution_error_message(&command_display, &error))?;
    match output.completion {
        ProcessCompletion::DeadlineExceeded => Err(format!(
            "{command_display}: subprocess timed out after {} ms",
            output.deadline.as_millis()
        )),
        ProcessCompletion::Exited | ProcessCompletion::Signaled => Ok(output),
    }
}

fn execution_error_message(command_display: &str, error: &ProcessExecutionError) -> String {
    if error.kind() == processkit::ErrorKind::Cancelled {
        format!("{command_display}: subprocess canceled")
    } else {
        format!("{command_display}: {error}")
    }
}

pub fn is_cancellation_error(message: &str) -> bool {
    let normalized = message.to_ascii_lowercase();
    normalized.contains("subprocess canceled") || normalized.contains("subprocess cancelled")
}

pub async fn run_capture(command: Command, policy: ProcessPolicy) -> Result<String, String> {
    let descriptor = infer_descriptor(&command);
    run_capture_named(command, policy, descriptor).await
}

pub async fn run_capture_named(
    command: Command,
    policy: ProcessPolicy,
    descriptor: ProcessDescriptor,
) -> Result<String, String> {
    let command_display = command_display(&command);
    let output = run_output_named(command, policy, descriptor).await?;
    if output.status.success() && !output.stdout_truncated {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else if output.status.success() {
        Err(format!(
            "{command_display}: stdout was truncated after capturing a bounded tail of {} total bytes",
            output.stdout_total_bytes
        ))
    } else {
        Err(format!(
            "{command_display}: {}",
            super::process_failure_message(&output)
        ))
    }
}

pub async fn run_status(command: Command, policy: ProcessPolicy) -> Result<(), String> {
    let descriptor = infer_descriptor(&command);
    run_status_named(command, policy, descriptor).await
}

pub async fn run_status_named(
    command: Command,
    policy: ProcessPolicy,
    descriptor: ProcessDescriptor,
) -> Result<(), String> {
    let command_display = command_display(&command);
    let output = run_output_named(command, policy, descriptor).await?;
    output.status.success().then_some(()).ok_or_else(|| {
        format!(
            "{command_display}: {}",
            super::process_failure_message(&output)
        )
    })
}

pub async fn run_status_with_stdin_named(
    command: Command,
    stdin: &str,
    policy: ProcessPolicy,
    descriptor: ProcessDescriptor,
) -> Result<(), String> {
    let command_display = command_display(&command);
    let output = execute_named(
        command,
        policy,
        ProcessInput::Bytes(stdin.as_bytes()),
        None,
        descriptor,
    )
    .await
    .map_err(|error| format!("{command_display}: {error}"))?;
    if output.completion == ProcessCompletion::DeadlineExceeded {
        return Err(format!(
            "{command_display}: subprocess timed out after {} ms",
            output.deadline.as_millis()
        ));
    }
    output.status.success().then_some(()).ok_or_else(|| {
        format!(
            "{command_display}: {}",
            super::process_failure_message(&output)
        )
    })
}

fn command_display(command: &Command) -> String {
    observability::sanitize_command_text(&command.command_line())
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::ffi::OsStringExt;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::process::{ProcessObservation, RecordedProcess, observe_process};

    fn shell(script: &str) -> Command {
        Command::new("sh").args(["-c", script])
    }

    #[tokio::test]
    async fn direct_command_preserves_cwd_env_and_env_clear() {
        let temp = std::env::temp_dir().join(format!(
            "prism-process-cwd-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&temp).unwrap();
        let output = execute(
            shell("printf '%s|%s|%s' \"$PWD\" \"$ONLY\" \"${HOME-unset}\"")
                .env_clear()
                .env("ONLY", "present")
                .current_dir(&temp),
            ProcessPolicy::Metadata,
            ProcessInput::Null,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            output.stdout,
            format!("{}|present|unset", temp.display()).as_bytes()
        );
        std::fs::remove_dir_all(temp).unwrap();
    }

    #[tokio::test]
    async fn non_utf8_argv_and_raw_streams_are_byte_exact_and_distinct() {
        let argument = std::ffi::OsString::from_vec(vec![b'a', 0x80, b'z']);
        let output = execute(
            Command::new("sh")
                .args(["-c", "printf '%s' \"$1\"; printf '\\201err' >&2", "fixture"])
                .arg(argument),
            ProcessPolicy::Metadata,
            ProcessInput::Null,
            None,
        )
        .await
        .unwrap();
        assert_eq!(output.stdout, [b'a', 0x80, b'z']);
        assert_eq!(output.stderr, [0x81, b'e', b'r', b'r']);
        assert_eq!(output.stdout_total_bytes, 3);
        assert_eq!(output.stderr_total_bytes, 4);
    }

    #[tokio::test]
    async fn huge_newline_free_output_is_fully_drained_into_a_bounded_tail() {
        let output = execute(
            shell("dd if=/dev/zero bs=8192 count=2 2>/dev/null | tr '\\0' x"),
            ProcessPolicy::Test,
            ProcessInput::Null,
            None,
        )
        .await
        .unwrap();
        assert_eq!(output.stdout_total_bytes, 16_384);
        assert_eq!(output.stdout.len(), 1024);
        assert!(output.stdout.iter().all(|byte| *byte == b'x'));
        assert!(output.stdout_truncated);
    }

    #[tokio::test]
    async fn command_configured_stdin_is_preserved_by_one_shot_execution() {
        let output = execute(
            shell("cat").stdin(Stdin::from_bytes(b"picker-a\npicker-b\n".to_vec())),
            ProcessPolicy::Metadata,
            ProcessInput::Null,
            None,
        )
        .await
        .unwrap();
        assert_eq!(output.stdout, b"picker-a\npicker-b\n");
    }

    #[tokio::test]
    async fn stdin_bytes_are_written_asynchronously_and_closed() {
        let output = execute(
            shell("cat"),
            ProcessPolicy::Metadata,
            ProcessInput::Bytes(b"input\0bytes"),
            None,
        )
        .await
        .unwrap();
        assert_eq!(output.stdout, b"input\0bytes");
    }

    #[tokio::test]
    async fn prefix_bounded_execution_keeps_protocol_prefixes_independently() {
        let output = execute_prefix_bounded(
            shell("printf 'abcdefgh'; printf '123456' >&2"),
            ProcessPolicy::WorkflowStep,
            Duration::from_secs(2),
            5,
            3,
            ProcessInput::Null,
            None,
            ProcessDescriptor::new("test.workflow.prefix"),
        )
        .await
        .unwrap();
        assert_eq!(output.stdout, b"abcde");
        assert_eq!(output.stderr, b"123");
        assert_eq!(output.stdout_total_bytes, 8);
        assert_eq!(output.stderr_total_bytes, 6);
        assert!(output.stdout_truncated);
        assert!(output.stderr_truncated);
    }

    #[tokio::test]
    async fn prefix_bounded_deadline_includes_a_blocked_stdin_writer() {
        let input = vec![b'x'; 2 * 1024 * 1024];
        let output = execute_prefix_bounded(
            shell("exec sleep 30"),
            ProcessPolicy::WorkflowStep,
            Duration::from_millis(250),
            1024,
            1024,
            ProcessInput::Bytes(&input),
            None,
            ProcessDescriptor::new("test.workflow.blocked_stdin"),
        )
        .await
        .unwrap();
        assert_eq!(output.completion, ProcessCompletion::DeadlineExceeded);
        assert!(output.elapsed < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn incomplete_descendant_held_pipe_fails_loud_instead_of_weakening_capture() {
        let started = Instant::now();
        let error = execute_prefix_bounded(
            shell("(exec sleep 30) & exit 0"),
            ProcessPolicy::WorkflowStep,
            Duration::from_secs(20),
            1024,
            1024,
            ProcessInput::Null,
            None,
            ProcessDescriptor::new("test.workflow.incomplete_capture"),
        )
        .await
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("did not fully drain before completion")
        );
        assert!(!error.stdout.complete || !error.stderr.complete);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn deadline_is_stable_and_processkit_managed() {
        let output = execute(
            shell("exec sleep 30"),
            ProcessPolicy::Test,
            ProcessInput::Null,
            None,
        )
        .await
        .unwrap();
        assert_eq!(output.completion, ProcessCompletion::DeadlineExceeded);
        assert_eq!(output.termination_stage, TerminationStage::Managed);
        assert!(output.elapsed < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn cancellation_removes_a_descendant_and_reaps_the_leader() {
        let temp = std::env::temp_dir().join(format!(
            "prism-process-cancel-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&temp).unwrap();
        let leader_path = temp.join("leader.pid");
        let descendant_path = temp.join("descendant.pid");
        let script = r#"
            printf '%s\n' "$$" > "$1"
            (trap '' TERM; exec sleep 30) &
            printf '%s\n' "$!" > "$2"
            wait
        "#;
        let token = CancellationToken::new();
        let task_token = token.clone();
        let command = shell(script)
            .arg("fixture")
            .arg(&leader_path)
            .arg(&descendant_path);
        let task = tokio::spawn(async move {
            execute(
                command,
                ProcessPolicy::Metadata,
                ProcessInput::Null,
                Some(task_token),
            )
            .await
        });
        let ready_deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while !descendant_path.exists() {
            assert!(tokio::time::Instant::now() < ready_deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let leader = std::fs::read_to_string(&leader_path)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        let descendant = std::fs::read_to_string(&descendant_path)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        token.cancel();
        let error = task.await.unwrap().unwrap_err();
        assert_eq!(error.kind(), processkit::ErrorKind::Cancelled);
        for pid in [leader, descendant] {
            let observation =
                observe_process(RecordedProcess::from_stored(pid, Some(u64::MAX))).unwrap();
            assert!(
                matches!(
                    observation,
                    ProcessObservation::Missing | ProcessObservation::IdentityReused
                ),
                "canceled process {pid} is still present: {observation:?}"
            );
        }
        std::fs::remove_dir_all(temp).unwrap();
    }
}
