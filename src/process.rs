use std::env;
use std::error::Error;
use std::fmt;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::observability::{self, LogLevel};

pub struct ProcessOutput {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
    pub stdout_total_bytes: u64,
    pub stdout_truncated: bool,
    pub stderr_total_bytes: u64,
    pub stderr_truncated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessPolicy {
    Metadata,
    LocalMutation,
    NetworkQuery,
    WorkflowStep,
    TmuxPoll,
    TmuxCapture,
    #[cfg(test)]
    Test,
}

impl ProcessPolicy {
    fn settings(self) -> PolicySettings {
        match self {
            Self::Metadata => PolicySettings::new(Duration::from_secs(30), 1024 * 1024),
            Self::LocalMutation => {
                PolicySettings::new(Duration::from_secs(10 * 60), 4 * 1024 * 1024)
            }
            Self::NetworkQuery => PolicySettings::new(Duration::from_secs(5 * 60), 4 * 1024 * 1024),
            Self::WorkflowStep => {
                PolicySettings::new(Duration::from_secs(6 * 60 * 60), 4 * 1024 * 1024)
            }
            Self::TmuxPoll => PolicySettings::new(Duration::from_secs(15), 1024 * 1024),
            Self::TmuxCapture => PolicySettings::new(Duration::from_secs(4), 4 * 1024 * 1024),
            #[cfg(test)]
            Self::Test => PolicySettings {
                deadline: Duration::from_millis(250),
                termination_grace: Duration::from_millis(100),
                capture_bytes: 1024,
            },
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Metadata => "metadata",
            Self::LocalMutation => "local_mutation",
            Self::NetworkQuery => "network_query",
            Self::WorkflowStep => "workflow_step",
            Self::TmuxPoll => "tmux_poll",
            Self::TmuxCapture => "tmux_capture",
            #[cfg(test)]
            Self::Test => "test",
        }
    }

    pub fn deadline(self) -> Duration {
        self.settings().deadline
    }
}

#[derive(Clone, Copy)]
struct PolicySettings {
    deadline: Duration,
    termination_grace: Duration,
    capture_bytes: usize,
}

impl PolicySettings {
    const fn new(deadline: Duration, capture_bytes: usize) -> Self {
        Self {
            deadline,
            termination_grace: Duration::from_secs(1),
            capture_bytes,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum ProcessInput<'a> {
    Null,
    Bytes(&'a [u8]),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessCompletion {
    Exited,
    Signaled,
    DeadlineExceeded,
    Canceled,
}

impl ProcessCompletion {
    fn label(self) -> &'static str {
        match self {
            Self::Exited => "exited",
            Self::Signaled => "signaled",
            Self::DeadlineExceeded => "deadline_exceeded",
            Self::Canceled => "canceled",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TerminationStage {
    #[default]
    None,
    Term,
    Kill,
}

impl TerminationStage {
    fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Term => "term",
            Self::Kill => "kill",
        }
    }
}

#[derive(Debug)]
pub struct CapturedTail {
    pub bytes: Vec<u8>,
    pub total_bytes: u64,
    pub truncated: bool,
}

#[derive(Debug)]
pub struct ProcessOutcome {
    pub status: ExitStatus,
    pub completion: ProcessCompletion,
    pub termination_stage: TerminationStage,
    pub stdout: CapturedTail,
    pub stderr: CapturedTail,
    pub elapsed: Duration,
    pub deadline: Duration,
    pub child_pid: u32,
    #[cfg(unix)]
    pub process_group: libc::pid_t,
}

#[derive(Debug)]
pub enum ProcessError {
    Spawn(io::Error),
    Signal {
        signal: &'static str,
        source: io::Error,
    },
    Wait(io::Error),
    Reap(io::Error),
    Stdin(io::Error),
    Read {
        stream: &'static str,
        source: io::Error,
    },
    MissingPipe(&'static str),
    ThreadPanicked(&'static str),
}

impl fmt::Display for ProcessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(error) => write!(formatter, "{error}"),
            Self::Signal { signal, source } => {
                write!(formatter, "send {signal} to subprocess group: {source}")
            }
            Self::Wait(error) => write!(formatter, "wait for subprocess: {error}"),
            Self::Reap(error) => write!(formatter, "reap subprocess: {error}"),
            Self::Stdin(error) => write!(formatter, "write subprocess stdin: {error}"),
            Self::Read { stream, source } => {
                write!(formatter, "read subprocess {stream}: {source}")
            }
            Self::MissingPipe(stream) => write!(formatter, "subprocess {stream} unavailable"),
            Self::ThreadPanicked(thread) => {
                write!(formatter, "subprocess {thread} thread panicked")
            }
        }
    }
}

impl Error for ProcessError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Spawn(error) | Self::Wait(error) | Self::Reap(error) | Self::Stdin(error) => {
                Some(error)
            }
            Self::Signal { source, .. } => Some(source),
            Self::Read { source, .. } => Some(source),
            Self::MissingPipe(_) | Self::ThreadPanicked(_) => None,
        }
    }
}

pub fn run_capture(command: &mut Command, policy: ProcessPolicy) -> Result<String, String> {
    let command_display = observability::command_display(command);
    let output = run_output(command, policy)?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(format!(
            "{command_display}: {}",
            process_failure_message(&output)
        ))
    }
}

pub fn run_status(command: &mut Command, policy: ProcessPolicy) -> Result<(), String> {
    let command_display = observability::command_display(command);
    let output = run_output(command, policy)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{command_display}: {}",
            process_failure_message(&output)
        ))
    }
}

pub fn run_output(command: &mut Command, policy: ProcessPolicy) -> Result<ProcessOutput, String> {
    run_output_with_failure_level(command, policy, LogLevel::Error)
}

pub fn run_output_allow_failure(
    command: &mut Command,
    policy: ProcessPolicy,
) -> Result<ProcessOutput, String> {
    run_output_with_failure_level(command, policy, LogLevel::Debug)
}

fn run_output_with_failure_level(
    command: &mut Command,
    policy: ProcessPolicy,
    failure_level: LogLevel,
) -> Result<ProcessOutput, String> {
    run_output_with_settings(command, policy, failure_level, ProcessInput::Null)
}

fn run_output_with_settings(
    command: &mut Command,
    policy: ProcessPolicy,
    failure_level: LogLevel,
    input: ProcessInput<'_>,
) -> Result<ProcessOutput, String> {
    let settings = policy.settings();
    let include_argv = observability::enabled(LogLevel::Trace);
    let command_display = observability::command_display(command);
    let operation = observability::begin_operation(
        LogLevel::Debug,
        "process",
        "start",
        "starting subprocess",
        Some(observability::process_start_data_json(
            command,
            include_argv,
            policy.label(),
            settings.deadline.as_millis() as i64,
        )),
    );
    let started = Instant::now();
    let outcome = supervise(command, policy, input, None).map_err(|error| {
        let elapsed_ms = started.elapsed().as_millis() as i64;
        operation.finish(
            LogLevel::Error,
            "process",
            "error",
            match error {
                ProcessError::Spawn(_) => format!("subprocess failed to start: {error}"),
                _ => format!("subprocess supervision failed: {error}"),
            },
            Some(observability::command_data_json(
                command,
                include_argv,
                Some(elapsed_ms),
                None,
                Some(&error.to_string()),
            )),
        );
        format!("{command_display}: {error}")
    })?;
    let elapsed_ms = outcome.elapsed.as_millis() as i64;
    let status = outcome.status;
    let stdout = String::from_utf8_lossy(&outcome.stdout.bytes).to_string();
    let stderr = String::from_utf8_lossy(&outcome.stderr.bytes).to_string();
    let process_output = ProcessOutput {
        status,
        stdout,
        stderr,
        stdout_total_bytes: outcome.stdout.total_bytes,
        stdout_truncated: outcome.stdout.truncated,
        stderr_total_bytes: outcome.stderr.total_bytes,
        stderr_truncated: outcome.stderr.truncated,
    };
    let deadline_error = (outcome.completion == ProcessCompletion::DeadlineExceeded).then(|| {
        format!(
            "subprocess timed out after {} ms",
            outcome.deadline.as_millis()
        )
    });
    let canceled_error = (outcome.completion == ProcessCompletion::Canceled)
        .then(|| "subprocess canceled".to_string());
    let completion_error = deadline_error.or(canceled_error);
    let (level, error) = if completion_error.is_none() && process_output.status.success() {
        (LogLevel::Debug, None)
    } else {
        (
            failure_level,
            completion_error.or_else(|| Some(process_failure_message(&process_output))),
        )
    };
    operation.finish(
        level,
        "process",
        "exit",
        if error.is_none() {
            "subprocess exited successfully".to_string()
        } else {
            format!("subprocess failed: {}", outcome.completion.label())
        },
        Some(observability::process_data_json(
            command,
            include_argv,
            observability::ProcessObservation {
                policy: policy.label(),
                elapsed_ms,
                deadline_ms: outcome.deadline.as_millis() as i64,
                child_pid: outcome.child_pid,
                #[cfg(unix)]
                process_group: outcome.process_group,
                status: &process_output.status.to_string(),
                completion: outcome.completion.label(),
                termination_stage: outcome.termination_stage.label(),
                stdout_bytes: process_output.stdout_total_bytes,
                stdout_truncated: process_output.stdout_truncated,
                stderr_bytes: process_output.stderr_total_bytes,
                stderr_truncated: process_output.stderr_truncated,
                error: error.as_deref(),
            },
        )),
    );
    match error {
        Some(error) if outcome.completion == ProcessCompletion::DeadlineExceeded => {
            Err(format!("{command_display}: {error}"))
        }
        Some(error) if outcome.completion == ProcessCompletion::Canceled => {
            Err(format!("{command_display}: {error}"))
        }
        _ => Ok(process_output),
    }
}

pub fn run_status_with_stdin(
    command: &mut Command,
    stdin: &str,
    policy: ProcessPolicy,
) -> Result<(), String> {
    let command_display = observability::command_display(command);
    let output = run_output_with_settings(
        command,
        policy,
        LogLevel::Error,
        ProcessInput::Bytes(stdin.as_bytes()),
    )?;
    output
        .status
        .success()
        .then_some(())
        .ok_or_else(|| format!("{command_display}: {}", process_failure_message(&output)))
}

pub fn supervise(
    command: &mut Command,
    policy: ProcessPolicy,
    input: ProcessInput<'_>,
    canceled: Option<&AtomicBool>,
) -> Result<ProcessOutcome, ProcessError> {
    supervise_with_settings(command, policy, policy.settings(), input, canceled)
}

fn supervise_with_settings(
    command: &mut Command,
    _policy: ProcessPolicy,
    settings: PolicySettings,
    input: ProcessInput<'_>,
    canceled: Option<&AtomicBool>,
) -> Result<ProcessOutcome, ProcessError> {
    command
        .stdin(match input {
            ProcessInput::Null => Stdio::null(),
            ProcessInput::Bytes(_) => Stdio::piped(),
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    let started = Instant::now();
    let mut child = command.spawn().map_err(ProcessError::Spawn)?;
    let child_pid = child.id();
    #[cfg(unix)]
    let process_group = child_pid as libc::pid_t;
    let stop_readers = Arc::new(AtomicBool::new(false));
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdin = match input {
        ProcessInput::Null => None,
        ProcessInput::Bytes(_) => child.stdin.take(),
    };
    let missing_pipe = if stdout.is_none() {
        Some("stdout")
    } else if stderr.is_none() {
        Some("stderr")
    } else if matches!(input, ProcessInput::Bytes(_)) && stdin.is_none() {
        Some("stdin")
    } else {
        None
    };
    if let Some(stream) = missing_pipe {
        let _ = terminate_active_child(&mut child, settings.termination_grace);
        return Err(ProcessError::MissingPipe(stream));
    }
    let stdout = stdout.expect("checked stdout pipe");
    let stderr = stderr.expect("checked stderr pipe");
    let stdout_reader =
        spawn_capture_reader(stdout, settings.capture_bytes, Arc::clone(&stop_readers));
    let stderr_reader =
        spawn_capture_reader(stderr, settings.capture_bytes, Arc::clone(&stop_readers));
    let stdin_writer = match input {
        ProcessInput::Null => None,
        ProcessInput::Bytes(bytes) => {
            let mut stdin = stdin.expect("checked stdin pipe");
            let bytes = bytes.to_vec();
            Some(std::thread::spawn(move || stdin.write_all(&bytes)))
        }
    };

    let mut status = None;
    let mut wait_error = None;
    let mut termination_error = None;
    let completion = loop {
        match child.try_wait() {
            Ok(Some(exit_status)) => {
                status = Some(exit_status);
                break completion_from_status(exit_status);
            }
            Ok(None) => {}
            Err(error) => {
                wait_error = Some(error);
                break ProcessCompletion::Canceled;
            }
        }
        if canceled.is_some_and(|flag| flag.load(Ordering::Acquire)) {
            break ProcessCompletion::Canceled;
        }
        if started.elapsed() >= settings.deadline {
            break ProcessCompletion::DeadlineExceeded;
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    let mut termination_stage = TerminationStage::None;
    if matches!(
        completion,
        ProcessCompletion::DeadlineExceeded | ProcessCompletion::Canceled
    ) {
        match signal_term(child_pid) {
            Ok(stage) => termination_stage = stage,
            Err(error) => termination_error = Some(error),
        }
        let grace_deadline = Instant::now() + settings.termination_grace;
        while Instant::now() < grace_deadline {
            if status.is_none() {
                match child.try_wait() {
                    Ok(child_status) => status = child_status,
                    Err(error) => {
                        wait_error.get_or_insert(error);
                        break;
                    }
                }
            }
            if status.is_some()
                && stdout_reader.is_finished()
                && stderr_reader.is_finished()
                && !process_group_exists(child_pid)
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        match signal_kill(child_pid) {
            Ok(true) => termination_stage = TerminationStage::Kill,
            Ok(false) => {}
            Err(error) => {
                termination_error.get_or_insert(error);
                let _ = child.kill();
            }
        }
        #[cfg(not(unix))]
        let _ = child.kill();
    } else {
        let drain_deadline = Instant::now() + settings.termination_grace;
        while (!stdout_reader.is_finished() || !stderr_reader.is_finished())
            && Instant::now() < drain_deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        if !stdout_reader.is_finished() || !stderr_reader.is_finished() {
            match signal_term(child_pid) {
                Ok(stage) => termination_stage = stage,
                Err(error) => termination_error = Some(error),
            }
            let term_deadline = Instant::now() + settings.termination_grace;
            while (process_group_exists(child_pid)
                || !stdout_reader.is_finished()
                || !stderr_reader.is_finished())
                && Instant::now() < term_deadline
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            if process_group_exists(child_pid)
                || !stdout_reader.is_finished()
                || !stderr_reader.is_finished()
            {
                match signal_kill(child_pid) {
                    Ok(true) => termination_stage = TerminationStage::Kill,
                    Ok(false) => {}
                    Err(error) => {
                        termination_error.get_or_insert(error);
                        let _ = child.kill();
                    }
                }
                #[cfg(not(unix))]
                let _ = child.kill();
            }
        }
    }

    let reap_error = if status.is_none() {
        match child.wait() {
            Ok(child_status) => {
                status = Some(child_status);
                None
            }
            Err(error) => Some(ProcessError::Reap(error)),
        }
    } else {
        None
    };
    stop_readers.store(true, Ordering::Release);
    let stdin_result = join_stdin(stdin_writer);
    let stdout = join_capture_reader(stdout_reader, "stdout");
    let stderr = join_capture_reader(stderr_reader, "stderr");

    if let Some(error) = wait_error {
        return Err(ProcessError::Wait(error));
    }
    if let Some(error) = reap_error {
        return Err(error);
    }
    if let Some(error) = termination_error {
        return Err(error);
    }
    if !matches!(
        completion,
        ProcessCompletion::DeadlineExceeded | ProcessCompletion::Canceled
    ) {
        stdin_result?;
    }
    let stdout = stdout?;
    let stderr = stderr?;
    Ok(ProcessOutcome {
        status: status.expect("subprocess was reaped"),
        completion,
        termination_stage,
        stdout,
        stderr,
        elapsed: started.elapsed(),
        deadline: settings.deadline,
        child_pid,
        #[cfg(unix)]
        process_group,
    })
}

fn completion_from_status(status: ExitStatus) -> ProcessCompletion {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if status.signal().is_some() {
            return ProcessCompletion::Signaled;
        }
    }
    ProcessCompletion::Exited
}

#[cfg(unix)]
fn spawn_capture_reader<R>(
    reader: R,
    max_bytes: usize,
    stop: Arc<AtomicBool>,
) -> JoinHandle<io::Result<CapturedTail>>
where
    R: Read + std::os::fd::AsRawFd + Send + 'static,
{
    std::thread::spawn(move || read_captured_tail(reader, max_bytes, &stop))
}

#[cfg(not(unix))]
fn spawn_capture_reader<R>(
    reader: R,
    max_bytes: usize,
    stop: Arc<AtomicBool>,
) -> JoinHandle<io::Result<CapturedTail>>
where
    R: Read + Send + 'static,
{
    std::thread::spawn(move || read_captured_tail(reader, max_bytes, &stop))
}

#[cfg(unix)]
fn read_captured_tail(
    mut reader: impl Read + std::os::fd::AsRawFd,
    max_bytes: usize,
    stop: &AtomicBool,
) -> io::Result<CapturedTail> {
    let fd = reader.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let mut tail = TailBuffer::new(max_bytes);
    let mut buffer = [0_u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(tail.finish()),
            Ok(read) => tail.push(&buffer[..read]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if stop.load(Ordering::Acquire) {
                    return Ok(tail.finish());
                }
                let mut descriptor = libc::pollfd {
                    fd,
                    events: libc::POLLIN | libc::POLLHUP,
                    revents: 0,
                };
                let result = unsafe { libc::poll(&mut descriptor, 1, 25) };
                if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                    return Err(io::Error::last_os_error());
                }
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(not(unix))]
fn read_captured_tail(
    mut reader: impl Read,
    max_bytes: usize,
    _stop: &AtomicBool,
) -> io::Result<CapturedTail> {
    let mut tail = TailBuffer::new(max_bytes);
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(tail.finish());
        }
        tail.push(&buffer[..read]);
    }
}

struct TailBuffer {
    bytes: Vec<u8>,
    max_bytes: usize,
    total_bytes: u64,
}

impl TailBuffer {
    fn new(max_bytes: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(max_bytes),
            max_bytes,
            total_bytes: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.total_bytes = self.total_bytes.saturating_add(bytes.len() as u64);
        if bytes.len() >= self.max_bytes {
            self.bytes.clear();
            self.bytes
                .extend_from_slice(&bytes[bytes.len() - self.max_bytes..]);
            return;
        }
        let overflow = self
            .bytes
            .len()
            .saturating_add(bytes.len())
            .saturating_sub(self.max_bytes);
        if overflow > 0 {
            self.bytes.drain(..overflow);
        }
        self.bytes.extend_from_slice(bytes);
    }

    fn finish(self) -> CapturedTail {
        CapturedTail {
            truncated: self.total_bytes > self.bytes.len() as u64,
            total_bytes: self.total_bytes,
            bytes: self.bytes,
        }
    }
}

fn join_capture_reader(
    reader: JoinHandle<io::Result<CapturedTail>>,
    stream: &'static str,
) -> Result<CapturedTail, ProcessError> {
    reader
        .join()
        .map_err(|_| ProcessError::ThreadPanicked(stream))?
        .map_err(|source| ProcessError::Read { stream, source })
}

fn join_stdin(writer: Option<JoinHandle<io::Result<()>>>) -> Result<(), ProcessError> {
    let Some(writer) = writer else {
        return Ok(());
    };
    writer
        .join()
        .map_err(|_| ProcessError::ThreadPanicked("stdin writer"))?
        .map_err(ProcessError::Stdin)
}

#[cfg(unix)]
fn signal_term(process_id: u32) -> Result<TerminationStage, ProcessError> {
    signal_process_group(process_id, libc::SIGTERM).map(|_| TerminationStage::Term)
}

#[cfg(not(unix))]
fn signal_term(_process_id: u32) -> Result<TerminationStage, ProcessError> {
    Ok(TerminationStage::Term)
}

#[cfg(unix)]
fn signal_kill(process_id: u32) -> Result<bool, ProcessError> {
    signal_process_group(process_id, libc::SIGKILL)
}

#[cfg(not(unix))]
fn signal_kill(_process_id: u32) -> Result<bool, ProcessError> {
    Ok(true)
}

#[cfg(unix)]
fn signal_process_group(process_id: u32, signal: libc::c_int) -> Result<bool, ProcessError> {
    let result = unsafe { libc::kill(-(process_id as libc::pid_t), signal) };
    if result == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(false)
    } else {
        Err(ProcessError::Signal {
            signal: match signal {
                libc::SIGTERM => "SIGTERM",
                libc::SIGKILL => "SIGKILL",
                _ => "signal",
            },
            source: error,
        })
    }
}

#[cfg(unix)]
fn process_group_exists(process_id: u32) -> bool {
    let result = unsafe { libc::kill(-(process_id as libc::pid_t), 0) };
    result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn process_group_exists(_process_id: u32) -> bool {
    false
}

pub fn terminate_active_child(
    child: &mut Child,
    grace: Duration,
) -> Result<TerminationStage, ProcessError> {
    let process_id = child.id();
    let mut first_error = None;
    let mut status = match child.try_wait() {
        Ok(status) => status,
        Err(error) => {
            first_error = Some(ProcessError::Wait(error));
            None
        }
    };
    let mut stage = TerminationStage::None;
    match signal_term(process_id) {
        Ok(term_stage) => stage = term_stage,
        Err(error) => {
            first_error.get_or_insert(error);
        }
    };
    let deadline = Instant::now() + grace;
    while process_group_exists(process_id) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
        if status.is_none() {
            match child.try_wait() {
                Ok(child_status) => status = child_status,
                Err(error) => {
                    first_error.get_or_insert(ProcessError::Wait(error));
                    break;
                }
            }
        }
    }

    // The leader may have honored TERM while a descendant retained the group.
    match signal_kill(process_id) {
        Ok(true) => stage = TerminationStage::Kill,
        Ok(false) => {}
        Err(error) => {
            first_error.get_or_insert(error);
            let _ = child.kill();
        }
    }
    #[cfg(not(unix))]
    let _ = child.kill();
    if status.is_none()
        && let Err(error) = child.wait()
    {
        first_error.get_or_insert(ProcessError::Reap(error));
    }
    if let Some(error) = first_error {
        Err(error)
    } else {
        Ok(stage)
    }
}

// This is the explicit interactive exception: the child owns the inherited terminal and
// remains unbounded so the user can control its lifetime.
pub fn run_status_inherited(command: &mut Command) -> Result<(), String> {
    let include_argv = observability::enabled(LogLevel::Trace);
    let command_display = observability::command_display(command);
    let operation = observability::begin_operation(
        LogLevel::Debug,
        "process",
        "start",
        "starting subprocess",
        Some(observability::command_data_json(
            command,
            include_argv,
            None,
            None,
            None,
        )),
    );
    let started = Instant::now();
    let status = command.status().map_err(|error| {
        let elapsed_ms = started.elapsed().as_millis() as i64;
        operation.finish(
            LogLevel::Error,
            "process",
            "error",
            format!("subprocess failed to start: {error}"),
            Some(observability::command_data_json(
                command,
                include_argv,
                Some(elapsed_ms),
                None,
                Some(&error.to_string()),
            )),
        );
        format!("{command_display}: {error}")
    })?;
    let elapsed_ms = started.elapsed().as_millis() as i64;
    if status.success() {
        operation.finish(
            LogLevel::Debug,
            "process",
            "exit",
            "subprocess exited successfully",
            Some(observability::command_data_json(
                command,
                include_argv,
                Some(elapsed_ms),
                Some(&status.to_string()),
                None,
            )),
        );
        Ok(())
    } else {
        let message = format!("exited with {status}");
        operation.finish(
            LogLevel::Error,
            "process",
            "exit",
            format!("subprocess failed: {status}"),
            Some(observability::command_data_json(
                command,
                include_argv,
                Some(elapsed_ms),
                Some(&status.to_string()),
                Some(&message),
            )),
        );
        Err(format!("{command_display}: {message}"))
    }
}

fn process_failure_message(output: &ProcessOutput) -> String {
    let stderr = first_non_empty_line(&output.stderr);
    let stdout = first_non_empty_line(&output.stdout);
    if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        format!("exited with {}", output.status)
    }
}

fn first_non_empty_line(output: &str) -> String {
    output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_string()
}

pub fn command_exists(command: &str) -> bool {
    if command.contains('/') {
        return Path::new(command).is_file();
    }
    let Some(path) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&path).any(|dir| dir.join(command).is_file())
}

pub fn command_version(command: &str) -> Option<String> {
    let argv = split_command_words(command);
    let program = argv.first()?;
    if !command_exists(program) {
        return None;
    }
    let output = run_output_allow_failure(
        Command::new(program).arg("--version"),
        ProcessPolicy::Metadata,
    )
    .ok()?;
    if !output.status.success() {
        return None;
    }
    output
        .stdout
        .lines()
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
}

pub fn split_command_words(command: &str) -> Vec<String> {
    parse_command_words(command).unwrap_or_else(|_| {
        command
            .split_whitespace()
            .map(ToString::to_string)
            .collect()
    })
}

pub fn parse_command_words(command: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut word_started = false;
    let mut chars = command.chars().peekable();

    while let Some(ch) = chars.next() {
        if let Some(active_quote) = quote {
            if ch == active_quote {
                quote = None;
            } else if ch == '\\' && active_quote == '"' {
                match chars.peek().copied() {
                    Some(next @ ('\\' | '"' | '$' | '`')) => {
                        chars.next();
                        current.push(next);
                    }
                    Some('\n') => {
                        chars.next();
                    }
                    Some(_) => current.push('\\'),
                    None => {
                        return Err("command ends with an incomplete escape".to_string());
                    }
                }
            } else {
                current.push(ch);
            }
            continue;
        }
        match ch {
            '\\' => {
                word_started = true;
                current.push(
                    chars
                        .next()
                        .ok_or_else(|| "command ends with an incomplete escape".to_string())?,
                );
            }
            '\'' | '"' => {
                word_started = true;
                quote = Some(ch);
            }
            ch if ch.is_whitespace() => {
                if word_started {
                    words.push(std::mem::take(&mut current));
                    word_started = false;
                }
            }
            ch => {
                word_started = true;
                current.push(ch);
            }
        }
    }
    if quote.is_some() {
        return Err("command contains an unterminated quote".to_string());
    }
    if word_started {
        words.push(current);
    }
    if words.is_empty() {
        Err("command cannot be empty".to_string())
    } else {
        Ok(words)
    }
}

pub fn run_configured_commands(commands: &[String], cwd: &Path, label: &str) -> Result<(), String> {
    for command in commands {
        let argv = split_command_words(command);
        let Some(program) = argv.first() else {
            continue;
        };
        run_status(
            Command::new(program).args(&argv[1..]).current_dir(cwd),
            ProcessPolicy::WorkflowStep,
        )
        .map_err(|error| format!("{label} check `{command}` failed: {error}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_command_words_handles_quotes() {
        let words = split_command_words(r#"my-agent --mode "two words" 'three words'"#);
        assert_eq!(
            words,
            vec!["my-agent", "--mode", "two words", "three words"]
        );
    }

    #[test]
    fn split_command_words_falls_back_for_incomplete_input() {
        assert_eq!(
            split_command_words("my-agent --mode 'incomplete"),
            ["my-agent", "--mode", "'incomplete"]
        );
    }

    #[test]
    fn parse_command_words_rejects_incomplete_input() {
        assert!(parse_command_words("agent '").is_err());
        assert!(parse_command_words("agent \\").is_err());
        assert!(parse_command_words("   ").is_err());
    }

    #[test]
    fn parse_command_words_preserves_empty_and_single_quoted_arguments() {
        assert_eq!(
            parse_command_words(r#"agent --empty "" '\d+'"#).unwrap(),
            ["agent", "--empty", "", "\\d+"]
        );
        assert_eq!(
            parse_command_words(r#"agent "\d+""#).unwrap(),
            ["agent", "\\d+"]
        );
    }

    #[test]
    fn first_non_empty_line_trims_and_discards_later_lines() {
        assert_eq!(
            first_non_empty_line("\n  first line  \nsecond line"),
            "first line"
        );
    }

    #[test]
    fn output_timeout_terminates_long_running_process() {
        let error = run_output_allow_failure(
            Command::new("sh").args(["-c", "exec sleep 1"]),
            ProcessPolicy::Test,
        )
        .err()
        .expect("long-running process should time out");

        assert!(error.contains("subprocess timed out"), "{error}");
    }

    #[test]
    fn supervisor_uses_null_stdin_unless_input_is_supplied() {
        let outcome = supervise(
            Command::new("sh").args([
                "-c",
                "if read value; then exit 9; else printf 'stdin-eof'; fi",
            ]),
            ProcessPolicy::Metadata,
            ProcessInput::Null,
            None,
        )
        .unwrap();

        assert!(outcome.status.success());
        assert_eq!(outcome.stdout.bytes, b"stdin-eof");
    }

    #[test]
    fn spawn_error_retains_its_io_source() {
        let error = supervise(
            &mut Command::new("/prism-test/command-that-does-not-exist"),
            ProcessPolicy::Metadata,
            ProcessInput::Null,
            None,
        )
        .unwrap_err();

        assert!(matches!(error, ProcessError::Spawn(_)));
        assert!(error.source().is_some());
    }

    #[test]
    #[cfg(unix)]
    fn supervisor_reports_requested_cancellation() {
        let canceled = AtomicBool::new(true);
        let outcome = supervise(
            Command::new("sh").args(["-c", "exec sleep 30"]),
            ProcessPolicy::Test,
            ProcessInput::Null,
            Some(&canceled),
        )
        .unwrap();

        assert_eq!(outcome.completion, ProcessCompletion::Canceled);
        assert!(matches!(
            outcome.termination_stage,
            TerminationStage::Term | TerminationStage::Kill
        ));
    }

    #[test]
    #[cfg(unix)]
    fn supervisor_kills_term_ignoring_pipe_descendant_and_bounds_each_capture() {
        let temp = std::env::temp_dir().join(format!(
            "prism-process-tree-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&temp).unwrap();
        let descendant_path = temp.join("descendant.pid");
        let script = r#"
            trap '' TERM
            (
                trap '' TERM
                i=0
                while [ "$i" -lt 400 ]; do
                    printf 'stdout-%04d-xxxxxxxx\n' "$i"
                    printf 'stderr-%04d-yyyyyyyy\n' "$i" >&2
                    i=$((i + 1))
                done
                while :; do :; done
            ) &
            descendant=$!
            printf '%s\n' "$descendant" > "$1"
            wait "$descendant"
        "#;
        let started = Instant::now();
        let outcome = supervise(
            Command::new("sh")
                .arg("-c")
                .arg(script)
                .arg("process-fixture")
                .arg(&descendant_path),
            ProcessPolicy::Test,
            ProcessInput::Null,
            None,
        )
        .unwrap();

        assert_eq!(outcome.completion, ProcessCompletion::DeadlineExceeded);
        assert_eq!(outcome.termination_stage, TerminationStage::Kill);
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(outcome.stdout.bytes.len(), 1024);
        assert_eq!(outcome.stderr.bytes.len(), 1024);
        assert!(outcome.stdout.total_bytes > outcome.stdout.bytes.len() as u64);
        assert!(outcome.stderr.total_bytes > outcome.stderr.bytes.len() as u64);
        assert!(outcome.stdout.truncated);
        assert!(outcome.stderr.truncated);

        let descendant = std::fs::read(&descendant_path).unwrap();
        let descendant = std::str::from_utf8(&descendant)
            .unwrap()
            .trim()
            .parse::<libc::pid_t>()
            .unwrap();
        let gone_deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let result = unsafe { libc::kill(descendant, 0) };
            if result != 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                break;
            }
            assert!(
                Instant::now() < gone_deadline,
                "descendant survived group kill"
            );
            std::thread::yield_now();
        }

        assert!(!outcome.stdout.bytes.is_empty());
        assert!(!outcome.stderr.bytes.is_empty());
        std::fs::remove_dir_all(temp).unwrap();
    }
}
