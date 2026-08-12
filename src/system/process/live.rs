//! Narrow async ownership/control actors for long-lived ProcessKit children.

use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

#[cfg(any(test, windows))]
use processkit::StdioMode;
use processkit::{Command, Outcome, OutputBufferPolicy, Stdin};
use tokio::io::AsyncWrite;
use tokio::sync::{mpsc, oneshot, watch};

use super::capture::{BoundedCapture, CapturedBytes};
use super::telemetry::{LiveTermination, ProcessTelemetry};
use super::{CancellationToken, ProcessDescriptor, ProcessInput, ProcessPolicy, record_process};

const TERMINATION_NATURAL: u8 = 0;
const TERMINATION_CANCELED: u8 = 1;
const TERMINATION_DROPPED: u8 = 2;

#[cfg(test)]
static ACTIVE_CANCELLATION_FORWARDERS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
struct CancellationForwarderGuard;

#[cfg(test)]
impl CancellationForwarderGuard {
    fn new() -> Self {
        ACTIVE_CANCELLATION_FORWARDERS.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Self
    }
}

#[cfg(test)]
impl Drop for CancellationForwarderGuard {
    fn drop(&mut self) {
        ACTIVE_CANCELLATION_FORWARDERS.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessStream {
    Stdout,
    Stderr,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveOutputLine {
    pub stream: ProcessStream,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LiveProcessCompletion {
    Exited(Outcome),
    Canceled,
    Failed(String),
}

#[derive(Debug)]
enum ControlRequest {
    Shutdown {
        reply: oneshot::Sender<Result<(), String>>,
    },
}

/// Cloneable, pipe-free control capability for one actor-owned child.
#[derive(Clone, Debug)]
pub struct ProcessControl {
    pid: u32,
    identity: Option<u64>,
    sender: mpsc::Sender<ControlRequest>,
    completion: watch::Receiver<Option<LiveProcessCompletion>>,
}

impl ProcessControl {
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    pub const fn identity(&self) -> Option<u64> {
        self.identity
    }

    pub async fn shutdown(&self) -> Result<(), String> {
        if self.completion.borrow().is_some() {
            return completion_result(self.completion.borrow().as_ref().expect("checked"));
        }
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ControlRequest::Shutdown { reply })
            .await
            .map_err(|_| format!("owned process {} control actor stopped", self.pid))?;
        response
            .await
            .map_err(|_| format!("owned process {} shutdown response was dropped", self.pid))?
    }

    pub async fn wait(&self) -> LiveProcessCompletion {
        let mut completion = self.completion.clone();
        loop {
            if let Some(completion) = completion.borrow().clone() {
                return completion;
            }
            if completion.changed().await.is_err() {
                return LiveProcessCompletion::Failed(format!(
                    "owned process {} completion actor stopped",
                    self.pid
                ));
            }
        }
    }
}

fn completion_result(completion: &LiveProcessCompletion) -> Result<(), String> {
    match completion {
        LiveProcessCompletion::Exited(_) | LiveProcessCompletion::Canceled => Ok(()),
        LiveProcessCompletion::Failed(error) => Err(error.clone()),
    }
}

/// Streaming owner. Dropping the last control handle cancels and reaps the child.
pub struct StreamingProcess {
    control: ProcessControl,
    lines: mpsc::Receiver<LiveOutputLine>,
    stdout: BoundedCapture,
    stderr: BoundedCapture,
}

impl StreamingProcess {
    pub fn control(&self) -> ProcessControl {
        self.control.clone()
    }

    pub const fn pid(&self) -> u32 {
        self.control.pid()
    }

    pub async fn next_line(&mut self) -> Option<LiveOutputLine> {
        self.lines.recv().await
    }

    pub async fn wait(&self) -> LiveProcessCompletion {
        self.control.wait().await
    }

    pub async fn shutdown(&self) -> Result<(), String> {
        self.control.shutdown().await
    }

    pub fn captured_output(&self) -> (CapturedBytes, CapturedBytes) {
        (self.stdout.snapshot(), self.stderr.snapshot())
    }
}

/// Start an actor-owned child and emit both raw ProcessKit streams as bounded lines.
///
/// The raw tees are awaited by ProcessKit's pipe pumps. A full line channel therefore
/// applies backpressure all the way to the child without exposing a raw pipe or growing
/// an unbounded intermediate queue.
pub async fn spawn_streaming(
    command: Command,
    policy: ProcessPolicy,
    input: ProcessInput<'_>,
    descriptor: ProcessDescriptor,
    channel_capacity: usize,
    max_line_bytes: usize,
) -> Result<StreamingProcess, String> {
    spawn_streaming_configured(
        command,
        policy,
        policy.settings().deadline,
        input,
        None,
        descriptor,
        channel_capacity,
        max_line_bytes,
        policy.settings().capture_bytes,
        policy.settings().capture_bytes,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn spawn_streaming_configured(
    command: Command,
    policy: ProcessPolicy,
    deadline: Duration,
    input: ProcessInput<'_>,
    cancellation: Option<CancellationToken>,
    descriptor: ProcessDescriptor,
    channel_capacity: usize,
    max_line_bytes: usize,
    stdout_capture_bytes: usize,
    stderr_capture_bytes: usize,
) -> Result<StreamingProcess, String> {
    let (line_sender, lines) = mpsc::channel(channel_capacity.max(1));
    let owner_cancellation = CancellationToken::new();
    let termination = Arc::new(AtomicU8::new(TERMINATION_NATURAL));
    let cancellation_forwarder = cancellation.map(|external_cancellation| {
        let cancellation = owner_cancellation.clone();
        let termination = Arc::clone(&termination);
        tokio::spawn(async move {
            #[cfg(test)]
            let _guard = CancellationForwarderGuard::new();
            external_cancellation.cancelled().await;
            let _ = termination.compare_exchange(
                TERMINATION_NATURAL,
                TERMINATION_CANCELED,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            cancellation.cancel();
        })
    });
    let stdout_capture = BoundedCapture::prefix(stdout_capture_bytes);
    let stderr_capture = BoundedCapture::prefix(stderr_capture_bytes);
    let stdout = BoundedLineWriter::new(
        line_sender.clone(),
        ProcessStream::Stdout,
        max_line_bytes,
        owner_cancellation.clone(),
        stdout_capture.clone(),
    );
    let stderr = BoundedLineWriter::new(
        line_sender,
        ProcessStream::Stderr,
        max_line_bytes,
        owner_cancellation.clone(),
        stderr_capture.clone(),
    );
    let settings = policy.settings();
    let mut configured = command
        .clone()
        .timeout(deadline)
        .timeout_grace(settings.termination_grace)
        .cancel_grace(settings.termination_grace)
        .cancel_on(owner_cancellation.clone())
        .output_buffer(OutputBufferPolicy::bounded(0).with_max_bytes(max_line_bytes.max(1)))
        .stdout_raw_tee(stdout)
        .stderr_raw_tee(stderr);
    configured = match input {
        ProcessInput::Null => configured.stdin(Stdin::empty()),
        ProcessInput::Bytes(bytes) => configured.stdin(Stdin::from_bytes(bytes.to_vec())),
    };
    let telemetry = ProcessTelemetry::begin(&command, policy, descriptor, deadline);
    let control = start_actor(
        configured,
        owner_cancellation,
        termination,
        cancellation_forwarder,
        telemetry,
        stdout_capture.clone(),
        stderr_capture.clone(),
        descriptor,
    )
    .await?;
    Ok(StreamingProcess {
        control,
        lines,
        stdout: stdout_capture,
        stderr: stderr_capture,
    })
}

/// Start a silent actor-owned long-lived child. The actor keeps ProcessKit's Windows
/// Job Object (and the corresponding Unix process group on supported callers) alive
/// until shutdown or natural exit.
#[cfg(any(test, windows))]
pub async fn spawn_owned(
    command: Command,
    descriptor: ProcessDescriptor,
) -> Result<ProcessControl, String> {
    let cancellation = CancellationToken::new();
    let termination = Arc::new(AtomicU8::new(TERMINATION_NATURAL));
    let stdout = BoundedCapture::prefix(0);
    let stderr = BoundedCapture::prefix(0);
    let telemetry = ProcessTelemetry::begin_owned(&command, descriptor);
    start_actor(
        command
            .stdin(Stdin::empty())
            .stdout(StdioMode::Null)
            .stderr(StdioMode::Null)
            .cancel_grace(Duration::from_secs(1))
            .cancel_on(cancellation.clone()),
        cancellation,
        termination,
        None,
        telemetry,
        stdout,
        stderr,
        descriptor,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn start_actor(
    command: Command,
    cancellation: CancellationToken,
    termination: Arc<AtomicU8>,
    cancellation_forwarder: Option<tokio::task::JoinHandle<()>>,
    mut telemetry: ProcessTelemetry,
    stdout: BoundedCapture,
    stderr: BoundedCapture,
    descriptor: ProcessDescriptor,
) -> Result<ProcessControl, String> {
    let started = Instant::now();
    let program = command.program().to_string_lossy().into_owned();
    let process = match command.start().await {
        Ok(process) => process,
        Err(error) => {
            if let Some(forwarder) = cancellation_forwarder {
                forwarder.abort();
                let _ = forwarder.await;
            }
            let stdout = stdout.snapshot();
            let stderr = stderr.snapshot();
            telemetry.finish_error(
                &error,
                started.elapsed(),
                None,
                stdout.total_bytes,
                stderr.total_bytes,
                stdout.truncated,
                stderr.truncated,
            );
            return Err(format!(
                "start {} process '{program}': {error}",
                descriptor.name
            ));
        }
    };
    let pid = match process.pid() {
        Some(pid) => pid,
        None => {
            cancellation.cancel();
            let _ = process.finish().await;
            if let Some(forwarder) = cancellation_forwarder {
                forwarder.abort();
                let _ = forwarder.await;
            }
            let message = format!(
                "start {} process '{program}': no process ID",
                descriptor.name
            );
            telemetry.finish_supervision_message(started.elapsed(), 0, &message);
            return Err(message);
        }
    };
    let identity = record_process(pid)
        .ok()
        .and_then(|recorded| recorded.identity)
        .map(super::ProcessIdentity::stored_value);
    let (completion_sender, completion) = watch::channel(None);
    let finish_sender = completion_sender.clone();
    let finish_termination = Arc::clone(&termination);
    tokio::spawn(async move {
        let result = process.finish().await;
        if let Some(forwarder) = cancellation_forwarder {
            forwarder.abort();
            let _ = forwarder.await;
        }
        let stdout = stdout.snapshot();
        let stderr = stderr.snapshot();
        let termination = match finish_termination.load(Ordering::Acquire) {
            TERMINATION_CANCELED => LiveTermination::Canceled,
            TERMINATION_DROPPED => LiveTermination::Dropped,
            _ => LiveTermination::Natural,
        };
        let finished = match result {
            Ok(finished) => {
                telemetry.finish_live_outcome(
                    &finished.outcome,
                    started.elapsed(),
                    pid,
                    &stdout,
                    &stderr,
                    termination,
                );
                successful_completion(finished.outcome, termination)
            }
            Err(error) => {
                telemetry.finish_live_error(
                    &error,
                    started.elapsed(),
                    pid,
                    &stdout,
                    &stderr,
                    termination,
                );
                if error.kind() == processkit::ErrorKind::Cancelled {
                    LiveProcessCompletion::Canceled
                } else {
                    LiveProcessCompletion::Failed(error.to_string())
                }
            }
        };
        finish_sender.send_replace(Some(finished));
    });

    let (sender, mut requests) = mpsc::channel::<ControlRequest>(4);
    let actor_completion = completion.clone();
    tokio::spawn(async move {
        let mut completion = actor_completion;
        while let Some(request) = requests.recv().await {
            match request {
                ControlRequest::Shutdown { reply } => {
                    let _ = termination.compare_exchange(
                        TERMINATION_NATURAL,
                        TERMINATION_CANCELED,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                    cancellation.cancel();
                    let result = loop {
                        if let Some(done) = completion.borrow().as_ref() {
                            break completion_result(done);
                        }
                        if completion.changed().await.is_err() {
                            break Err("owned process completion actor stopped".to_string());
                        }
                    };
                    let _ = reply.send(result);
                }
            }
        }
        // The registry/owner dropped every control capability.
        if completion.borrow().is_none() {
            termination.store(TERMINATION_DROPPED, Ordering::Release);
            cancellation.cancel();
        }
    });

    Ok(ProcessControl {
        pid,
        identity,
        sender,
        completion,
    })
}

fn successful_completion(outcome: Outcome, termination: LiveTermination) -> LiveProcessCompletion {
    if termination == LiveTermination::Canceled {
        LiveProcessCompletion::Canceled
    } else {
        LiveProcessCompletion::Exited(outcome)
    }
}

type PermitFuture = Pin<
    Box<
        dyn Future<Output = Result<mpsc::OwnedPermit<LiveOutputLine>, mpsc::error::SendError<()>>>
            + Send,
    >,
>;

struct BoundedLineWriter {
    sender: mpsc::Sender<LiveOutputLine>,
    stream: ProcessStream,
    max_line_bytes: usize,
    cancellation: CancellationToken,
    capture: BoundedCapture,
    line: Vec<u8>,
    truncated: bool,
    pending: VecDeque<LiveOutputLine>,
    permit: Option<PermitFuture>,
    pending_write_len: Option<usize>,
}

impl BoundedLineWriter {
    fn new(
        sender: mpsc::Sender<LiveOutputLine>,
        stream: ProcessStream,
        max_line_bytes: usize,
        cancellation: CancellationToken,
        capture: BoundedCapture,
    ) -> Self {
        Self {
            sender,
            stream,
            max_line_bytes: max_line_bytes.max(1),
            cancellation,
            capture,
            line: Vec::new(),
            truncated: false,
            pending: VecDeque::new(),
            permit: None,
            pending_write_len: None,
        }
    }

    fn accept(&mut self, bytes: &[u8]) {
        self.capture.accept(bytes);
        let mut remaining = bytes;
        while let Some(newline) = remaining.iter().position(|byte| *byte == b'\n') {
            self.extend_line(&remaining[..newline]);
            self.finish_line();
            remaining = &remaining[newline + 1..];
        }
        self.extend_line(remaining);
    }

    fn extend_line(&mut self, bytes: &[u8]) {
        let available = self.max_line_bytes.saturating_sub(self.line.len());
        self.line
            .extend_from_slice(&bytes[..bytes.len().min(available)]);
        self.truncated |= bytes.len() > available;
    }

    fn finish_line(&mut self) {
        if self.line.ends_with(b"\r") {
            self.line.pop();
        }
        let mut text = String::from_utf8_lossy(&self.line).into_owned();
        if self.truncated {
            text.push_str(" [line truncated]");
        }
        self.pending.push_back(LiveOutputLine {
            stream: self.stream,
            text,
        });
        self.line.clear();
        self.truncated = false;
    }

    fn finish_tail(&mut self) {
        if !self.line.is_empty() || self.truncated {
            self.finish_line();
        }
    }

    fn poll_pending(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while !self.pending.is_empty() {
            if self.permit.is_none() {
                self.permit = Some(Box::pin(self.sender.clone().reserve_owned()));
            }
            let permit = match self.permit.as_mut().expect("set").as_mut().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(permit)) => permit,
                Poll::Ready(Err(_)) => {
                    self.cancellation.cancel();
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "harness output receiver closed",
                    )));
                }
            };
            self.permit = None;
            permit.send(self.pending.pop_front().expect("not empty"));
        }
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for BoundedLineWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        if self.pending_write_len.is_none() {
            self.accept(bytes);
            self.pending_write_len = Some(bytes.len());
        }
        match self.poll_pending(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Ready(Ok(())) => Poll::Ready(Ok(self.pending_write_len.take().expect("set"))),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        self.finish_tail();
        match self.poll_pending(cx) {
            Poll::Ready(Ok(())) => {
                self.capture.mark_complete();
                Poll::Ready(Ok(()))
            }
            result => result,
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        self.poll_flush(cx)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn streaming_actor_bounds_lines_applies_backpressure_and_completes_stdin() {
        let command = Command::new("sh").args([
            "-c",
            "read value; printf '%s\\n' \"$value\"; printf '123456789\\n' >&2",
        ]);
        let mut process = spawn_streaming(
            command,
            ProcessPolicy::Test,
            ProcessInput::Bytes(b"hello\n"),
            ProcessDescriptor::new("test.live.stream"),
            1,
            5,
        )
        .await
        .unwrap();
        assert!(process.pid() > 0);
        let lines = [
            process.next_line().await.unwrap(),
            process.next_line().await.unwrap(),
        ];
        assert!(
            lines
                .iter()
                .any(|line| { line.stream == ProcessStream::Stdout && line.text == "hello" })
        );
        assert!(lines.iter().any(|line| {
            line.stream == ProcessStream::Stderr && line.text == "12345 [line truncated]"
        }));
        assert!(matches!(
            process.wait().await,
            LiveProcessCompletion::Exited(Outcome::Exited(0))
        ));
    }

    #[tokio::test]
    async fn raw_capture_is_exact_for_unterminated_non_utf8_and_large_lines() {
        let command = Command::new("sh").args([
            "-c",
            "printf 'a\\200z'; dd if=/dev/zero bs=4096 count=2 2>/dev/null | tr '\\0' x >&2",
        ]);
        let mut process = spawn_streaming_configured(
            command,
            ProcessPolicy::Test,
            Duration::from_secs(2),
            ProcessInput::Null,
            None,
            ProcessDescriptor::new("test.live.raw_capture"),
            2,
            8,
            16,
            32,
        )
        .await
        .unwrap();
        while process.next_line().await.is_some() {}
        assert!(matches!(
            process.wait().await,
            LiveProcessCompletion::Exited(Outcome::Exited(0))
        ));
        let (stdout, stderr) = process.captured_output();
        assert_eq!(stdout.bytes, [b'a', 0x80, b'z']);
        assert_eq!(stdout.total_bytes, 3);
        assert!(!stdout.truncated);
        assert_eq!(stderr.bytes, vec![b'x'; 32]);
        assert_eq!(stderr.total_bytes, 8192);
        assert!(stderr.truncated);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn live_process_emits_one_start_and_one_terminal_telemetry_event() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = std::env::temp_dir().join(format!(
            "prism-live-telemetry-{}-{}",
            std::process::id(),
            crate::util::timestamp_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let executable = root.join("live-telemetry-fixture");
        std::fs::write(&executable, "#!/bin/sh\nprintf done\n").unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions).unwrap();
        let _ = crate::observability::take_captured_events();

        let mut process = spawn_streaming(
            Command::new(&executable),
            ProcessPolicy::Test,
            ProcessInput::Null,
            ProcessDescriptor::new("test.live.telemetry"),
            1,
            16,
        )
        .await
        .unwrap();
        while process.next_line().await.is_some() {}
        let _ = process.wait().await;
        let marker = executable.display().to_string();
        let events = crate::observability::take_captured_events()
            .into_iter()
            .filter(|event| {
                event.target == "process"
                    && event
                        .data_json
                        .as_deref()
                        .is_some_and(|data| data.contains(&marker))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.action == "start")
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.action.as_str(), "exit" | "error"))
                .count(),
            1
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropping_live_owner_reports_drop_and_reaps_the_process() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = std::env::temp_dir().join(format!(
            "prism-live-drop-{}-{}",
            std::process::id(),
            crate::util::timestamp_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let executable = root.join("live-drop-fixture");
        std::fs::write(&executable, "#!/bin/sh\nexec sleep 30\n").unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions).unwrap();
        let _ = crate::observability::take_captured_events();

        let process = spawn_streaming(
            Command::new(&executable),
            ProcessPolicy::Test,
            ProcessInput::Null,
            ProcessDescriptor::new("test.live.drop"),
            1,
            16,
        )
        .await
        .unwrap();
        let recorded = super::super::record_process(process.pid()).unwrap();
        drop(process);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while super::super::observe_process(recorded).unwrap()
            == super::super::ProcessObservation::RunningSameProcess
        {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let marker = executable.display().to_string();
        let terminals = crate::observability::take_captured_events()
            .into_iter()
            .filter(|event| {
                event.target == "process"
                    && event.action == "error"
                    && event.data_json.as_deref().is_some_and(|data| {
                        data.contains(&marker) && data.contains("\"completion\":\"dropped\"")
                    })
            })
            .count();
        assert_eq!(terminals, 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn canceled_signal_shaped_finished_is_publicly_canceled() {
        assert_eq!(
            successful_completion(
                Outcome::Signalled(Some(libc::SIGTERM)),
                LiveTermination::Canceled,
            ),
            LiveProcessCompletion::Canceled
        );
    }

    #[tokio::test]
    async fn external_cancellation_reports_canceled_completion_and_telemetry() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = std::env::temp_dir().join(format!(
            "prism-live-canceled-{}-{}",
            std::process::id(),
            crate::util::timestamp_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let executable = root.join("live-canceled-fixture");
        std::fs::write(&executable, "#!/bin/sh\nexec sleep 30\n").unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions).unwrap();
        let _ = crate::observability::take_captured_events();

        let external = CancellationToken::new();
        let process = spawn_streaming_configured(
            Command::new(&executable),
            ProcessPolicy::Test,
            Duration::from_secs(5),
            ProcessInput::Null,
            Some(external.clone()),
            ProcessDescriptor::new("test.live.external_canceled"),
            1,
            16,
            16,
            16,
        )
        .await
        .unwrap();
        external.cancel();
        assert_eq!(process.wait().await, LiveProcessCompletion::Canceled);

        let marker = executable.display().to_string();
        let terminals = crate::observability::take_captured_events()
            .into_iter()
            .filter(|event| {
                event.target == "process"
                    && event.action == "error"
                    && event.data_json.as_deref().is_some_and(|data| {
                        data.contains(&marker) && data.contains("\"completion\":\"canceled\"")
                    })
            })
            .count();
        assert_eq!(terminals, 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn completed_live_process_joins_external_cancellation_forwarder() {
        let baseline = ACTIVE_CANCELLATION_FORWARDERS.load(std::sync::atomic::Ordering::Acquire);
        let external = CancellationToken::new();
        let mut process = spawn_streaming_configured(
            Command::new("sh").args(["-c", "printf done"]),
            ProcessPolicy::Test,
            Duration::from_secs(2),
            ProcessInput::Null,
            Some(external),
            ProcessDescriptor::new("test.live.forwarder_cleanup"),
            1,
            16,
            16,
            16,
        )
        .await
        .unwrap();
        while process.next_line().await.is_some() {}
        let _ = process.wait().await;
        assert_eq!(
            ACTIVE_CANCELLATION_FORWARDERS.load(std::sync::atomic::Ordering::Acquire),
            baseline
        );
    }

    #[tokio::test]
    async fn control_shutdown_cancels_and_reaps_the_owned_tree() {
        let control = spawn_owned(
            Command::new("sh").args(["-c", "exec sleep 30"]),
            ProcessDescriptor::new("test.live.owned"),
        )
        .await
        .unwrap();
        let recorded = super::super::record_process(control.pid()).unwrap();
        control.shutdown().await.unwrap();
        assert_eq!(
            super::super::observe_process(recorded).unwrap(),
            super::super::ProcessObservation::Missing
        );
    }
}
