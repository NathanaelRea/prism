#[cfg(test)]
use std::cell::Cell;
use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::repo::Repository;
use crate::util::stable_hash;

const EVENT_CHANNEL_CAPACITY: usize = 16_384;
const CONTROL_CHANNEL_CAPACITY: usize = 8;
const RING_EVENT_CAPACITY: usize = 65_536;
const RETENTION: Duration = Duration::from_secs(60);
const MAX_BEFORE_SECONDS: u64 = 60;
const MAX_AFTER_SECONDS: u64 = 30;
const CONTROL_POLL_INTERVAL: Duration = Duration::from_millis(5);
const RESPONSE_GRACE: Duration = Duration::from_secs(10);
const SCHEMA_VERSION: u32 = 1;
const MAX_CONTROL_BATCH: usize = 8;
const MAX_EVENT_BATCH: usize = 1_024;
const MAX_REQUEST_BATCH: usize = 64;
const RECORDER_SOCKET_PATH_BUDGET: usize = 103;

static RECORDER: OnceLock<Recorder> = OnceLock::new();
static UI_THREAD: OnceLock<ThreadId> = OnceLock::new();
static NEXT_INPUT_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_CALL_ID: AtomicU64 = AtomicU64::new(1);
static UI_IDLE_STARTED_US: AtomicU64 = AtomicU64::new(0);
static UI_LAST_INPUT_US: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static PENDING_INPUT: RefCell<Option<InputTrace>> = const { RefCell::new(None) };
    static CURRENT_JOB_CONTEXT: RefCell<Option<JobDiagnosticContext>> = const { RefCell::new(None) };
    #[cfg(test)]
    static EXTERNAL_CALLS_FORBIDDEN: Cell<bool> = const { Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn deny_external_calls_on_current_thread<T>(operation: impl FnOnce() -> T) -> T {
    struct Reset(bool);

    impl Drop for Reset {
        fn drop(&mut self) {
            EXTERNAL_CALLS_FORBIDDEN.with(|forbidden| forbidden.set(self.0));
        }
    }

    let previous = EXTERNAL_CALLS_FORBIDDEN.with(|forbidden| forbidden.replace(true));
    let _reset = Reset(previous);
    operation()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct JobDiagnosticContext {
    job_id: u64,
    job_type: &'static str,
}

pub(crate) fn with_job_context<T>(
    job_id: u64,
    job_type: &'static str,
    operation: impl FnOnce() -> T,
) -> T {
    struct ResetJobContext(Option<JobDiagnosticContext>);

    impl Drop for ResetJobContext {
        fn drop(&mut self) {
            CURRENT_JOB_CONTEXT.with(|current| current.replace(self.0.take()));
        }
    }

    let context = JobDiagnosticContext { job_id, job_type };
    let previous = CURRENT_JOB_CONTEXT.with(|current| current.replace(Some(context)));
    let _reset = ResetJobContext(previous);
    operation()
}

fn current_job_context() -> Option<JobDiagnosticContext> {
    CURRENT_JOB_CONTEXT.with(|current| *current.borrow())
}

#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum FieldValue {
    Unsigned(u64),
    Bool(bool),
    Text(String),
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Field {
    name: &'static str,
    value: FieldValue,
}

pub(crate) fn unsigned(name: &'static str, value: impl TryInto<u64>) -> Field {
    Field {
        name,
        value: FieldValue::Unsigned(value.try_into().unwrap_or(u64::MAX)),
    }
}

pub(crate) fn boolean(name: &'static str, value: bool) -> Field {
    Field {
        name,
        value: FieldValue::Bool(value),
    }
}

pub(crate) fn text(name: &'static str, value: impl AsRef<str>) -> Field {
    Field {
        name,
        value: FieldValue::Text(crate::observability::redact_freeform(value.as_ref(), 256)),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExternalCallCategory {
    Process,
    Http,
}

impl ExternalCallCategory {
    const fn label(self) -> &'static str {
        match self {
            Self::Process => "process",
            Self::Http => "http",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExternalCallOutcome {
    Success,
    Failed,
    TimedOut,
    Canceled,
    SpawnFailed,
    Closed,
}

impl ExternalCallOutcome {
    const fn label(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::Canceled => "canceled",
            Self::SpawnFailed => "spawn_failed",
            Self::Closed => "closed",
        }
    }
}

pub(crate) struct ExternalCallTrace {
    call_id: u64,
    category: ExternalCallCategory,
    name: &'static str,
    started: Instant,
    terminal_base_fields: Vec<Field>,
    job_context: Option<JobDiagnosticContext>,
    finished: bool,
}

impl ExternalCallTrace {
    pub(crate) fn begin(
        category: ExternalCallCategory,
        name: &'static str,
        terminal_base_fields: Vec<Field>,
    ) -> Self {
        #[cfg(test)]
        EXTERNAL_CALLS_FORBIDDEN.with(|forbidden| {
            assert!(
                !forbidden.get(),
                "external calls are forbidden on this thread"
            );
        });
        let trace = Self {
            call_id: NEXT_CALL_ID.fetch_add(1, Ordering::Relaxed),
            category,
            name,
            started: Instant::now(),
            terminal_base_fields,
            job_context: current_job_context(),
            finished: false,
        };
        trace.emit("start", None, None, Vec::new());
        trace
    }

    pub(crate) fn finish(&mut self, outcome: ExternalCallOutcome, terminal_fields: Vec<Field>) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.emit(
            "complete",
            Some(outcome.label()),
            Some(self.started.elapsed()),
            terminal_fields,
        );
    }

    fn emit(
        &self,
        phase: &'static str,
        outcome: Option<&'static str>,
        duration: Option<Duration>,
        mut fields: Vec<Field>,
    ) {
        let mut common = Vec::with_capacity(
            3 + self.terminal_base_fields.len() + fields.len() + usize::from(outcome.is_some()),
        );
        common.push(unsigned("call_id", self.call_id));
        common.push(text("name", self.name));
        common.push(text("phase", phase));
        if let Some(outcome) = outcome {
            common.push(text("outcome", outcome));
            common.extend(self.terminal_base_fields.iter().cloned());
        }
        if let Some(context) = self.job_context {
            common.push(unsigned("job_id", context.job_id));
            common.push(text("job_type", context.job_type));
        }
        common.append(&mut fields);
        record(self.category.label(), "call", duration, common);
    }
}

impl Drop for ExternalCallTrace {
    fn drop(&mut self) {
        if !self.finished {
            self.finished = true;
            self.emit(
                "complete",
                Some("abandoned"),
                Some(self.started.elapsed()),
                Vec::new(),
            );
        }
    }
}

#[derive(Clone, Debug)]
struct RecordInput {
    recorded_us: u64,
    category: &'static str,
    operation: &'static str,
    duration_us: Option<u64>,
    ui_thread: bool,
    fields: Vec<Field>,
}

#[derive(Clone, Debug, Serialize)]
struct StoredEvent {
    #[serde(rename = "type")]
    record_type: &'static str,
    sequence: u64,
    monotonic_us: u64,
    category: &'static str,
    operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_us: Option<u64>,
    ui_thread: bool,
    fields: Vec<Field>,
}

pub(crate) fn record(
    category: &'static str,
    operation: &'static str,
    duration: Option<Duration>,
    fields: Vec<Field>,
) {
    let recorder = recorder();
    let input = RecordInput {
        recorded_us: duration_us(recorder.origin.elapsed()),
        category,
        operation,
        duration_us: duration.map(duration_us),
        ui_thread: is_ui_thread(),
        fields,
    };
    if recorder.event_tx.try_send(input).is_err() {
        recorder.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) fn mark_ui_thread() {
    let _ = UI_THREAD.set(thread::current().id());
    UI_LAST_INPUT_US.store(
        duration_us(recorder().origin.elapsed()).saturating_add(1),
        Ordering::Release,
    );
}

pub(crate) fn is_ui_thread() -> bool {
    UI_THREAD
        .get()
        .is_some_and(|thread_id| *thread_id == thread::current().id())
}

pub(crate) fn idle_for() -> Option<Duration> {
    let started = UI_IDLE_STARTED_US.load(Ordering::Acquire);
    if started == 0 {
        return None;
    }
    let now = duration_us(recorder().origin.elapsed()).saturating_add(1);
    Some(Duration::from_micros(now.saturating_sub(started)))
}

pub(crate) fn start_idle() {
    let recorder = recorder();
    let started = duration_us(recorder.origin.elapsed()).saturating_add(1);
    if UI_IDLE_STARTED_US
        .compare_exchange(0, started, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        record("lifecycle", "idle_start", None, Vec::new());
    }
}

pub(crate) fn end_idle(reason: &'static str) {
    let started = UI_IDLE_STARTED_US.swap(0, Ordering::AcqRel);
    if started == 0 {
        return;
    }
    let now = duration_us(recorder().origin.elapsed()).saturating_add(1);
    record(
        "lifecycle",
        "idle_end",
        Some(Duration::from_micros(now.saturating_sub(started))),
        vec![text("reason", reason)],
    );
}

pub(crate) fn terminal_input(kind: &'static str) {
    end_idle("terminal_event");
    UI_LAST_INPUT_US.store(
        duration_us(recorder().origin.elapsed()).saturating_add(1),
        Ordering::Release,
    );
    let previous = PENDING_INPUT.with(|pending| pending.replace(Some(InputTrace::begin(kind))));
    if let Some(previous) = previous {
        previous.handled();
    }
}

pub(crate) fn terminal_poll_timed_out() {
    let last_input = UI_LAST_INPUT_US.load(Ordering::Acquire);
    if last_input == 0 {
        return;
    }
    let now = duration_us(recorder().origin.elapsed()).saturating_add(1);
    if now.saturating_sub(last_input) >= duration_us(Duration::from_secs(1)) {
        start_idle();
    }
}

pub(crate) fn finish_pending_input_without_frame() {
    if let Some(input) = PENDING_INPUT.with(|pending| pending.take()) {
        input.handled();
    }
}

pub(crate) fn take_input_for_frame() -> Option<InputTrace> {
    let input = PENDING_INPUT.with(|pending| pending.take());
    if let Some(input) = &input {
        input.handled();
    }
    input
}

#[derive(Clone, Debug)]
pub(crate) struct InputTrace {
    id: u64,
    kind: &'static str,
    started: Instant,
}

pub(crate) struct TransactionTrace {
    name: &'static str,
    started: Instant,
    finished: bool,
}

impl TransactionTrace {
    pub(crate) fn begin(name: &'static str) -> Self {
        Self {
            name,
            started: Instant::now(),
            finished: false,
        }
    }

    pub(crate) fn committed(mut self) {
        self.record(true);
        self.finished = true;
    }

    fn record(&self, committed: bool) {
        record(
            "sqlite",
            "transaction",
            Some(self.started.elapsed()),
            vec![text("name", self.name), boolean("committed", committed)],
        );
    }
}

impl Drop for TransactionTrace {
    fn drop(&mut self) {
        if !self.finished {
            self.record(false);
        }
    }
}

impl InputTrace {
    pub(crate) fn begin(kind: &'static str) -> Self {
        let trace = Self {
            id: NEXT_INPUT_ID.fetch_add(1, Ordering::Relaxed),
            kind,
            started: Instant::now(),
        };
        record(
            "input",
            "received",
            None,
            vec![unsigned("input_id", trace.id), text("kind", trace.kind)],
        );
        trace
    }

    pub(crate) fn handled(&self) {
        record(
            "input",
            "handled",
            Some(self.started.elapsed()),
            vec![unsigned("input_id", self.id), text("kind", self.kind)],
        );
    }

    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    pub(crate) fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
}

struct Recorder {
    event_tx: mpsc::SyncSender<RecordInput>,
    control_tx: mpsc::SyncSender<Control>,
    dropped: Arc<AtomicU64>,
    origin: Instant,
}

fn recorder() -> &'static Recorder {
    RECORDER.get_or_init(|| {
        let (event_tx, event_rx) = mpsc::sync_channel(EVENT_CHANNEL_CAPACITY);
        let (control_tx, control_rx) = mpsc::sync_channel(CONTROL_CHANNEL_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        let origin = Instant::now();
        let process_started_unix_ms = unix_ms();
        let worker_dropped = dropped.clone();
        let _ = thread::Builder::new()
            .name("prism-flight-recorder".to_string())
            .spawn(move || {
                RecorderState::new(
                    event_rx,
                    control_rx,
                    worker_dropped,
                    origin,
                    process_started_unix_ms,
                )
                .run();
            });
        Recorder {
            event_tx,
            control_tx,
            dropped,
            origin,
        }
    })
}

enum Control {
    Serve {
        endpoints: Vec<ServerEndpoint>,
        reply: mpsc::SyncSender<Vec<PathBuf>>,
    },
    Register {
        endpoints: Vec<ServerEndpoint>,
    },
    Stop {
        paths: Vec<PathBuf>,
    },
    StopAll,
    #[cfg(test)]
    Drain {
        reply: mpsc::SyncSender<()>,
    },
}

struct ServerEndpoint {
    socket_path: PathBuf,
    output_dir: PathBuf,
}

struct ServerSocket {
    socket: std::os::unix::net::UnixDatagram,
    socket_path: PathBuf,
    output_dir: PathBuf,
    _lock: File,
}

pub(crate) struct ServerGuard {
    paths: Vec<PathBuf>,
}

impl ServerGuard {
    pub(crate) fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        if self.paths.is_empty() {
            return;
        }
        let _ = recorder().control_tx.try_send(Control::Stop {
            paths: std::mem::take(&mut self.paths),
        });
    }
}

pub(crate) fn serve_repositories<'a>(
    repos: impl IntoIterator<Item = &'a Repository>,
) -> ServerGuard {
    let endpoints = server_endpoints(repos);
    if endpoints.is_empty() {
        return ServerGuard { paths: Vec::new() };
    }
    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    if recorder()
        .control_tx
        .send(Control::Serve {
            endpoints,
            reply: reply_tx,
        })
        .is_err()
    {
        return ServerGuard { paths: Vec::new() };
    }
    ServerGuard {
        paths: reply_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap_or_default(),
    }
}

pub(crate) fn register_repositories<'a>(repos: impl IntoIterator<Item = &'a Repository>) {
    let endpoints = server_endpoints(repos);
    if endpoints.is_empty() {
        return;
    }
    let _ = recorder()
        .control_tx
        .try_send(Control::Register { endpoints });
}

pub(crate) fn stop_all_servers() {
    let _ = recorder().control_tx.try_send(Control::StopAll);
}

#[cfg(test)]
fn drain_events_for_test() -> bool {
    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    recorder()
        .control_tx
        .send(Control::Drain { reply: reply_tx })
        .is_ok()
        && reply_rx.recv_timeout(Duration::from_secs(1)).is_ok()
}

fn server_endpoints<'a>(repos: impl IntoIterator<Item = &'a Repository>) -> Vec<ServerEndpoint> {
    repos
        .into_iter()
        .map(|repo| ServerEndpoint {
            socket_path: control_socket_path(repo),
            output_dir: repo.prism_dir().join("recordings"),
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RecordOptions {
    pub before_seconds: u64,
    pub after_seconds: u64,
}

impl Default for RecordOptions {
    fn default() -> Self {
        Self {
            before_seconds: 60,
            after_seconds: 30,
        }
    }
}

impl RecordOptions {
    pub(crate) fn validate(self) -> Result<Self, String> {
        if self.before_seconds > MAX_BEFORE_SECONDS {
            return Err(format!(
                "--before must be between 0 and {MAX_BEFORE_SECONDS} seconds"
            ));
        }
        if self.after_seconds > MAX_AFTER_SECONDS {
            return Err(format!(
                "--after must be between 0 and {MAX_AFTER_SECONDS} seconds"
            ));
        }
        Ok(self)
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct CaptureRequest {
    schema_version: u32,
    before_seconds: u64,
    after_seconds: u64,
}

#[derive(Debug, Deserialize, Serialize)]
struct CaptureResponse {
    schema_version: u32,
    path: Option<PathBuf>,
    error: Option<String>,
}

pub(crate) fn trigger(repo: &Repository, options: RecordOptions) -> Result<PathBuf, String> {
    let options = options.validate()?;
    trigger_unix(repo, options)
}

fn trigger_unix(repo: &Repository, options: RecordOptions) -> Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixDatagram;

    let server_path = control_socket_path(repo);
    validate_recorder_socket_path(&server_path, "recorder control")?;
    if !server_path.exists() {
        return Err(format!(
            "no running Prism TUI recorder found for {}; start Prism for this repository first",
            repo.root.display()
        ));
    }
    let client_path = client_socket_path()?;
    remove_socket_if_present(&client_path)?;
    let socket = UnixDatagram::bind(&client_path)
        .map_err(|error| format!("bind debug recorder response socket: {error}"))?;
    let _cleanup = SocketPathGuard(client_path.clone());
    fs::set_permissions(&client_path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("secure debug recorder response socket: {error}"))?;
    socket
        .set_read_timeout(Some(
            Duration::from_secs(options.after_seconds).saturating_add(RESPONSE_GRACE),
        ))
        .map_err(|error| format!("configure debug recorder timeout: {error}"))?;
    let request = serde_json::to_vec(&CaptureRequest {
        schema_version: SCHEMA_VERSION,
        before_seconds: options.before_seconds,
        after_seconds: options.after_seconds,
    })
    .map_err(|error| format!("encode debug recorder request: {error}"))?;
    socket.send_to(&request, &server_path).map_err(|error| {
        format!(
            "contact running Prism TUI recorder at {}: {error}",
            server_path.display()
        )
    })?;
    let mut response = [0_u8; 4096];
    let size = loop {
        match socket.recv(&mut response) {
            Ok(size) => break size,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(format!("wait for debug recording: {error}")),
        }
    };
    let response: CaptureResponse = serde_json::from_slice(&response[..size])
        .map_err(|error| format!("decode debug recorder response: {error}"))?;
    if let Some(error) = response.error {
        return Err(error);
    }
    response
        .path
        .ok_or_else(|| "debug recorder returned no artifact path".to_string())
}

struct SocketPathGuard(PathBuf);

impl Drop for SocketPathGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

struct PendingCapture {
    trigger_us: u64,
    trigger_unix_ms: u64,
    before: Duration,
    after: Duration,
    response_path: PathBuf,
    output_dir: PathBuf,
    dropped_at_trigger: u64,
}

struct RecorderState {
    event_rx: mpsc::Receiver<RecordInput>,
    control_rx: mpsc::Receiver<Control>,
    dropped: Arc<AtomicU64>,
    origin: Instant,
    process_started_unix_ms: u64,
    ring: VecDeque<StoredEvent>,
    next_sequence: u64,
    servers: Vec<ServerSocket>,
    capture: Option<PendingCapture>,
}

impl RecorderState {
    fn new(
        event_rx: mpsc::Receiver<RecordInput>,
        control_rx: mpsc::Receiver<Control>,
        dropped: Arc<AtomicU64>,
        origin: Instant,
        process_started_unix_ms: u64,
    ) -> Self {
        Self {
            event_rx,
            control_rx,
            dropped,
            origin,
            process_started_unix_ms,
            ring: VecDeque::with_capacity(RING_EVENT_CAPACITY),
            next_sequence: 1,
            servers: Vec::new(),
            capture: None,
        }
    }

    fn run(&mut self) {
        loop {
            self.finish_capture_if_due();
            for _ in 0..MAX_CONTROL_BATCH {
                let Ok(control) = self.control_rx.try_recv() else {
                    break;
                };
                self.handle_control(control);
            }
            self.poll_servers();
            match self.event_rx.recv_timeout(CONTROL_POLL_INTERVAL) {
                Ok(event) => {
                    self.push(event);
                    for _ in 1..MAX_EVENT_BATCH {
                        let Ok(event) = self.event_rx.try_recv() else {
                            break;
                        };
                        self.push(event);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            self.finish_capture_if_due();
            self.prune();
        }
        self.remove_all_servers();
    }

    fn push(&mut self, input: RecordInput) {
        if self.ring.len() == RING_EVENT_CAPACITY {
            self.ring.pop_front();
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        self.ring.push_back(StoredEvent {
            record_type: "event",
            sequence: self.next_sequence,
            monotonic_us: input.recorded_us,
            category: input.category,
            operation: input.operation,
            duration_us: input.duration_us,
            ui_thread: input.ui_thread,
            fields: input.fields,
        });
        self.next_sequence = self.next_sequence.saturating_add(1);
    }

    fn prune(&mut self) {
        let latest_us = duration_us(self.origin.elapsed());
        let earliest_us = self.capture.as_ref().map_or_else(
            || latest_us.saturating_sub(duration_us(RETENTION)),
            |capture| {
                capture
                    .trigger_us
                    .saturating_sub(duration_us(capture.before))
            },
        );
        while self
            .ring
            .front()
            .is_some_and(|event| event.monotonic_us < earliest_us)
        {
            self.ring.pop_front();
        }
    }

    fn handle_control(&mut self, control: Control) {
        match control {
            Control::Serve { endpoints, reply } => {
                let _ = reply.send(self.add_servers(endpoints));
            }
            Control::Register { endpoints } => {
                self.add_servers(endpoints);
            }
            Control::Stop { paths } => {
                let mut retained = Vec::new();
                for server in self.servers.drain(..) {
                    if paths.contains(&server.socket_path) {
                        let _ = fs::remove_file(&server.socket_path);
                    } else {
                        retained.push(server);
                    }
                }
                self.servers = retained;
            }
            Control::StopAll => self.remove_all_servers(),
            #[cfg(test)]
            Control::Drain { reply } => {
                for _ in 0..EVENT_CHANNEL_CAPACITY {
                    let Ok(event) = self.event_rx.try_recv() else {
                        break;
                    };
                    self.push(event);
                }
                let _ = reply.send(());
            }
        }
    }

    fn add_servers(&mut self, endpoints: Vec<ServerEndpoint>) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        for endpoint in endpoints {
            if self
                .servers
                .iter()
                .any(|server| server.socket_path == endpoint.socket_path)
            {
                continue;
            }
            if let Ok(server) = bind_server(endpoint) {
                paths.push(server.socket_path.clone());
                self.servers.push(server);
            }
        }
        paths
    }

    fn poll_servers(&mut self) {
        let mut requests = Vec::new();
        'servers: for server in &self.servers {
            loop {
                if requests.len() == MAX_REQUEST_BATCH {
                    break 'servers;
                }
                let mut bytes = [0_u8; 1024];
                match server.socket.recv_from(&mut bytes) {
                    Ok((size, source)) => {
                        if let Some(response_path) = source.as_pathname() {
                            requests.push((
                                server.output_dir.clone(),
                                response_path.to_path_buf(),
                                bytes[..size].to_vec(),
                            ));
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
        }
        for (output_dir, response_path, bytes) in requests {
            self.handle_request(output_dir, response_path, &bytes);
        }
    }

    fn handle_request(&mut self, output_dir: PathBuf, response_path: PathBuf, bytes: &[u8]) {
        let request = match serde_json::from_slice::<CaptureRequest>(bytes) {
            Ok(request) if request.schema_version == SCHEMA_VERSION => request,
            _ => return,
        };
        let options = RecordOptions {
            before_seconds: request.before_seconds,
            after_seconds: request.after_seconds,
        };
        if let Err(error) = options.validate() {
            send_response(&response_path, Err(error));
            return;
        }
        if self.capture.is_some() {
            send_response(
                &response_path,
                Err("a debug recording is already in progress".to_string()),
            );
            return;
        }
        let trigger_us = duration_us(self.origin.elapsed());
        self.capture = Some(PendingCapture {
            trigger_us,
            trigger_unix_ms: unix_ms(),
            before: Duration::from_secs(options.before_seconds),
            after: Duration::from_secs(options.after_seconds),
            response_path,
            output_dir,
            dropped_at_trigger: self.dropped.load(Ordering::Relaxed),
        });
    }

    fn finish_capture_if_due(&mut self) {
        let Some(capture) = self.capture.as_ref() else {
            return;
        };
        let latest_us = duration_us(self.origin.elapsed());
        let deadline_us = capture
            .trigger_us
            .saturating_add(duration_us(capture.after));
        if latest_us < deadline_us && !capture.after.is_zero() {
            return;
        }
        for _ in 0..EVENT_CHANNEL_CAPACITY {
            let Ok(event) = self.event_rx.try_recv() else {
                break;
            };
            self.push(event);
        }
        let capture = self.capture.take().expect("capture was checked above");
        let result = self.write_capture(&capture);
        send_response(&capture.response_path, result);
    }

    fn write_capture(&self, capture: &PendingCapture) -> Result<PathBuf, String> {
        let start_us = capture
            .trigger_us
            .saturating_sub(duration_us(capture.before));
        let end_us = capture
            .trigger_us
            .saturating_add(duration_us(capture.after));
        let events = self
            .ring
            .iter()
            .filter(|event| event.monotonic_us >= start_us && event.monotonic_us <= end_us)
            .cloned()
            .collect::<Vec<_>>();
        crate::durability::create_dir_all(
            &capture.output_dir,
            crate::durability::DurabilityIntent::Maximum,
        )
        .map_err(|error| {
            format!(
                "create recording directory {}: {error}",
                capture.output_dir.display()
            )
        })?;
        let path = capture.output_dir.join(format!(
            "prism-recording-{}-{}-{}.jsonl",
            capture.trigger_unix_ms,
            capture.trigger_us,
            std::process::id()
        ));
        let temporary = path.with_extension(format!("jsonl.tmp-{}", std::process::id()));
        let file = create_recording_file(&temporary)?;
        let mut writer = BufWriter::new(file);
        let header = CaptureHeader {
            record_type: "capture",
            schema_version: SCHEMA_VERSION,
            prism_version: env!("CARGO_PKG_VERSION"),
            process_id: std::process::id(),
            process_started_unix_ms: self.process_started_unix_ms,
            trigger_unix_ms: capture.trigger_unix_ms,
            trigger_monotonic_us: capture.trigger_us,
            window_start_monotonic_us: start_us,
            window_end_monotonic_us: end_us,
            before_seconds: capture.before.as_secs(),
            after_seconds: capture.after.as_secs(),
            event_count: events.len(),
            diagnostics_dropped_at_trigger: capture.dropped_at_trigger,
            diagnostics_dropped_total: self.dropped.load(Ordering::Relaxed),
        };
        write_json_line(&mut writer, &header)?;
        for event in &events {
            write_json_line(&mut writer, event)?;
        }
        write_json_line(&mut writer, &capture_summary(&events))?;
        writer
            .flush()
            .map_err(|error| format!("flush {}: {error}", temporary.display()))?;
        crate::durability::sync_file(
            writer.get_ref(),
            crate::durability::DurabilityIntent::Maximum,
        )
        .map_err(|error| {
            let stage = match error.stage() {
                crate::durability::FileSyncStage::File => "sync",
                crate::durability::FileSyncStage::FullFile => "fully sync",
            };
            format!("{stage} {}: {}", temporary.display(), error.into_source())
        })?;
        drop(writer);
        fs::rename(&temporary, &path)
            .map_err(|error| format!("commit debug recording {}: {error}", path.display()))?;
        crate::durability::sync_directory(
            &capture.output_dir,
            crate::durability::DurabilityIntent::Maximum,
        )
        .map_err(|error| {
            format!(
                "sync debug recording directory {} after committing {}: {error}",
                capture.output_dir.display(),
                path.display()
            )
        })?;
        Ok(path)
    }

    fn remove_all_servers(&mut self) {
        for server in self.servers.drain(..) {
            let _ = fs::remove_file(server.socket_path);
        }
    }
}

#[derive(Serialize)]
struct CaptureHeader {
    #[serde(rename = "type")]
    record_type: &'static str,
    schema_version: u32,
    prism_version: &'static str,
    process_id: u32,
    process_started_unix_ms: u64,
    trigger_unix_ms: u64,
    trigger_monotonic_us: u64,
    window_start_monotonic_us: u64,
    window_end_monotonic_us: u64,
    before_seconds: u64,
    after_seconds: u64,
    event_count: usize,
    diagnostics_dropped_at_trigger: u64,
    diagnostics_dropped_total: u64,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
struct MetricSummary {
    category: String,
    operation: String,
    count: usize,
    p50_us: u64,
    p95_us: u64,
    max_us: u64,
}

#[derive(Serialize)]
struct CaptureSummary {
    #[serde(rename = "type")]
    record_type: &'static str,
    metrics: Vec<MetricSummary>,
}

fn capture_summary(events: &[StoredEvent]) -> CaptureSummary {
    let mut durations = BTreeMap::<(String, String), Vec<u64>>::new();
    for event in events {
        let operation = event
            .fields
            .iter()
            .find_map(|field| match (field.name, &field.value) {
                ("name", FieldValue::Text(name))
                    if event.category == "sqlite"
                        || matches!(
                            (event.category, event.operation),
                            ("process", "call") | ("http", "call")
                        ) =>
                {
                    Some(format!("{}:{name}", event.operation))
                }
                _ => None,
            })
            .unwrap_or_else(|| event.operation.to_string());
        if let Some(duration_us) = event.duration_us {
            durations
                .entry((event.category.to_string(), operation.clone()))
                .or_default()
                .push(duration_us);
        }
        for field in &event.fields {
            if !field.name.ends_with("_us") {
                continue;
            }
            let FieldValue::Unsigned(duration_us) = &field.value else {
                continue;
            };
            durations
                .entry((
                    event.category.to_string(),
                    format!("{}.{}", operation, field.name.trim_end_matches("_us")),
                ))
                .or_default()
                .push(*duration_us);
        }
    }
    let metrics = durations
        .into_iter()
        .map(|((category, operation), mut values)| {
            values.sort_unstable();
            MetricSummary {
                category,
                operation,
                count: values.len(),
                p50_us: percentile(&values, 50),
                p95_us: percentile(&values, 95),
                max_us: values.last().copied().unwrap_or_default(),
            }
        })
        .collect();
    CaptureSummary {
        record_type: "summary",
        metrics,
    }
}

fn percentile(values: &[u64], percentile: usize) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let rank = (values.len() * percentile).div_ceil(100);
    values[rank.saturating_sub(1).min(values.len() - 1)]
}

fn write_json_line(writer: &mut impl Write, value: &impl Serialize) -> Result<(), String> {
    serde_json::to_writer(&mut *writer, value)
        .map_err(|error| format!("encode debug recording: {error}"))?;
    writer
        .write_all(b"\n")
        .map_err(|error| format!("write debug recording: {error}"))
}

fn create_recording_file(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
    options
        .open(path)
        .map_err(|error| format!("create debug recording {}: {error}", path.display()))
}

fn bind_server(endpoint: ServerEndpoint) -> Result<ServerSocket, String> {
    bind_server_in(endpoint, &control_runtime_dir())
}

fn bind_server_in(endpoint: ServerEndpoint, runtime_dir: &Path) -> Result<ServerSocket, String> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::os::unix::net::UnixDatagram;

    validate_recorder_socket_path(&endpoint.socket_path, "recorder control")?;
    let runtime_dir = ensure_control_runtime_dir_at(runtime_dir)?;
    if endpoint.socket_path.parent() != Some(runtime_dir.as_path()) {
        return Err("recorder socket is outside its private runtime directory".to_string());
    }
    let lock_path = endpoint.socket_path.with_extension("lock");
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
        .map_err(|error| format!("open recorder lock {}: {error}", lock_path.display()))?;
    fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("secure recorder lock {}: {error}", lock_path.display()))?;
    let locked = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if locked != 0 {
        return Err("another recorder already owns the control socket".to_string());
    }
    remove_socket_if_present(&endpoint.socket_path)?;
    let socket = UnixDatagram::bind(&endpoint.socket_path).map_err(|error| {
        format!(
            "bind recorder control socket {}: {error}",
            endpoint.socket_path.display()
        )
    })?;
    fs::set_permissions(&endpoint.socket_path, fs::Permissions::from_mode(0o600)).map_err(
        |error| {
            format!(
                "secure recorder control socket {}: {error}",
                endpoint.socket_path.display()
            )
        },
    )?;
    socket
        .set_nonblocking(true)
        .map_err(|error| format!("configure recorder control socket: {error}"))?;
    Ok(ServerSocket {
        socket,
        socket_path: endpoint.socket_path,
        output_dir: endpoint.output_dir,
        _lock: lock,
    })
}

fn send_response(path: &Path, result: Result<PathBuf, String>) {
    use std::os::unix::net::UnixDatagram;

    let response = match result {
        Ok(path) => CaptureResponse {
            schema_version: SCHEMA_VERSION,
            path: Some(path),
            error: None,
        },
        Err(error) => CaptureResponse {
            schema_version: SCHEMA_VERSION,
            path: None,
            error: Some(error),
        },
    };
    if let Ok(bytes) = serde_json::to_vec(&response)
        && let Ok(socket) = UnixDatagram::unbound()
    {
        let _ = socket.send_to(&bytes, path);
    }
}

pub(crate) fn control_socket_path(repo: &Repository) -> PathBuf {
    let hash = stable_hash(&repo.root);
    control_runtime_dir().join(format!("repo-{hash:016x}.sock"))
}

fn client_socket_path() -> Result<PathBuf, String> {
    let runtime_dir = control_runtime_dir();
    let path = runtime_dir.join(format!(
        "prism-flight-client-{}-{}.sock",
        std::process::id(),
        unix_ms()
    ));
    validate_recorder_socket_path(&path, "recorder response")?;
    ensure_control_runtime_dir()?;
    Ok(path)
}

fn control_runtime_dir() -> PathBuf {
    PathBuf::from("/tmp").join(format!("prism-flight-{}", unsafe { libc::geteuid() }))
}

fn ensure_control_runtime_dir() -> Result<PathBuf, String> {
    ensure_control_runtime_dir_at(&control_runtime_dir())
}

fn ensure_control_runtime_dir_at(path: &Path) -> Result<PathBuf, String> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    match builder.create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(format!(
                "create recorder runtime directory {}: {error}",
                path.display()
            ));
        }
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("inspect recorder runtime directory: {error}"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(format!(
            "recorder runtime path is not a directory: {}",
            path.display()
        ));
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(format!(
            "recorder runtime directory is owned by another user: {}",
            path.display()
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("secure recorder runtime directory: {error}"))?;
    Ok(path.to_path_buf())
}

fn remove_socket_if_present(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("remove stale socket {}: {error}", path.display())),
    }
}

#[cfg(unix)]
fn validate_recorder_socket_path(path: &Path, purpose: &str) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;

    let bytes = path.as_os_str().as_bytes();
    if bytes.contains(&0) {
        return Err(format!(
            "{purpose} socket path {} contains a NUL byte",
            path.display()
        ));
    }
    if bytes.len() > RECORDER_SOCKET_PATH_BUDGET {
        return Err(format!(
            "{purpose} socket path {} is {} bytes, exceeding the supported maximum of {RECORDER_SOCKET_PATH_BUDGET} bytes",
            path.display(),
            bytes.len()
        ));
    }
    Ok(())
}

fn duration_us(duration: Duration) -> u64 {
    duration.as_micros().min(u128::from(u64::MAX)) as u64
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored(duration_us: u64) -> StoredEvent {
        StoredEvent {
            record_type: "event",
            sequence: duration_us,
            monotonic_us: duration_us,
            category: "tui",
            operation: "frame",
            duration_us: Some(duration_us),
            ui_thread: true,
            fields: Vec::new(),
        }
    }

    #[test]
    fn capture_summary_reports_nearest_rank_percentiles() {
        let events = (1..=100).map(stored).collect::<Vec<_>>();

        assert_eq!(
            capture_summary(&events).metrics,
            vec![MetricSummary {
                category: "tui".to_string(),
                operation: "frame".to_string(),
                count: 100,
                p50_us: 50,
                p95_us: 95,
                max_us: 100,
            }]
        );
    }

    #[test]
    fn capture_summary_includes_named_duration_fields() {
        let mut event = stored(100);
        event.fields.push(unsigned("render_us", 25_u64));

        let summary = capture_summary(&[event]);

        assert!(summary.metrics.contains(&MetricSummary {
            category: "tui".to_string(),
            operation: "frame.render".to_string(),
            count: 1,
            p50_us: 25,
            p95_us: 25,
            max_us: 25,
        }));
    }

    #[test]
    fn capture_summary_separates_stable_external_call_names() {
        let mut process = stored(100);
        process.category = "process";
        process.operation = "call";
        process.fields.push(text("name", "gh.pr.view"));
        let mut http = stored(200);
        http.category = "http";
        http.operation = "call";
        http.fields.push(text("name", "opencode.session.status"));
        http.fields.push(unsigned("connect_us", 25_u64));

        let summary = capture_summary(&[process, http]);

        assert!(summary.metrics.contains(&MetricSummary {
            category: "process".to_string(),
            operation: "call:gh.pr.view".to_string(),
            count: 1,
            p50_us: 100,
            p95_us: 100,
            max_us: 100,
        }));
        assert!(summary.metrics.contains(&MetricSummary {
            category: "http".to_string(),
            operation: "call:opencode.session.status.connect".to_string(),
            count: 1,
            p50_us: 25,
            p95_us: 25,
            max_us: 25,
        }));
    }

    #[test]
    fn external_call_job_context_is_nested_and_panic_safe() {
        assert_eq!(current_job_context(), None);
        with_job_context(1, "outer", || {
            assert_eq!(
                current_job_context(),
                Some(JobDiagnosticContext {
                    job_id: 1,
                    job_type: "outer"
                })
            );
            with_job_context(2, "inner", || {
                assert_eq!(
                    current_job_context(),
                    Some(JobDiagnosticContext {
                        job_id: 2,
                        job_type: "inner"
                    })
                );
            });
            assert_eq!(current_job_context().map(|context| context.job_id), Some(1));
        });
        assert_eq!(current_job_context(), None);

        let _ = std::panic::catch_unwind(|| {
            with_job_context(3, "panic", || panic!("injected panic"));
        });
        assert_eq!(current_job_context(), None);
    }

    #[test]
    fn text_fields_apply_observability_redaction() {
        let field = text("value", "token=ghp_not-a-real-token");

        assert!(matches!(field.value, FieldValue::Text(value) if !value.contains("ghp_")));
    }

    #[test]
    fn record_options_bound_capture_memory_window() {
        assert!(
            RecordOptions {
                before_seconds: 61,
                after_seconds: 30,
            }
            .validate()
            .is_err()
        );
        assert!(
            RecordOptions {
                before_seconds: 60,
                after_seconds: 31,
            }
            .validate()
            .is_err()
        );
        assert_eq!(
            RecordOptions::default().validate(),
            Ok(RecordOptions::default())
        );
    }

    #[test]
    fn control_socket_identity_uses_repository_root() {
        let repo = Repository {
            root: PathBuf::from("/work/example"),
        };

        assert_eq!(
            control_socket_path(&repo),
            control_runtime_dir().join(format!("repo-{:016x}.sock", stable_hash(&repo.root)))
        );
    }

    #[cfg(unix)]
    #[test]
    fn trigger_writes_an_atomic_jsonl_capture_without_sqlite() {
        use std::os::unix::fs::PermissionsExt;

        let temp = crate::compact_runtime::CompactTempDir::new("flight-recorder");
        let base = temp.path().to_path_buf();
        let repo = Repository::with_config_dir_for_test(base.join("repo"), base.join("config"));
        let server = serve_repositories([&repo]);
        let mut in_flight = ExternalCallTrace::begin(
            ExternalCallCategory::Process,
            "test.external.in_flight",
            vec![text("policy", "test")],
        );
        record(
            "test",
            "probe",
            Some(Duration::from_micros(42)),
            vec![unsigned("value", 7_u64)],
        );

        let path = trigger(
            &repo,
            RecordOptions {
                before_seconds: 60,
                after_seconds: 0,
            },
        )
        .unwrap();
        let contents = fs::read_to_string(&path).unwrap();

        assert!(
            contents
                .lines()
                .next()
                .unwrap()
                .contains("\"type\":\"capture\"")
        );
        assert!(contents.contains("\"category\":\"test\""));
        assert!(contents.contains("\"operation\":\"probe\""));
        assert!(contents.lines().any(|line| {
            line.contains("test.external.in_flight")
                && line.contains("\"phase\",\"value\":\"start\"")
        }));
        assert!(!contents.lines().any(|line| {
            line.contains("test.external.in_flight")
                && line.contains("\"phase\",\"value\":\"complete\"")
        }));
        assert!(
            contents
                .lines()
                .last()
                .unwrap()
                .contains("\"type\":\"summary\"")
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!fs::read_dir(path.parent().unwrap()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp-")
        }));

        assert!(drain_events_for_test());
        in_flight.finish(ExternalCallOutcome::Success, Vec::new());
        let secret = "flight-secret-argv-env-output-stderr";
        let mut command = std::process::Command::new("sh");
        command
            .args([
                "-c",
                "printf '%s' \"$FLIGHT_SECRET\"; printf '%s' \"$1\" >&2",
                "sh",
                secret,
            ])
            .env("FLIGHT_SECRET", secret);
        with_job_context(77, "test_job", || {
            crate::process::run_output_named(
                &mut command,
                crate::process::ProcessPolicy::Test,
                crate::process::ProcessDescriptor::new("test.external.private"),
            )
        })
        .unwrap();
        let mut missing = std::process::Command::new("/prism-test/missing-executable");
        assert!(
            crate::process::run_output_named(
                &mut missing,
                crate::process::ProcessPolicy::Test,
                crate::process::ProcessDescriptor::new("test.external.spawn_failed"),
            )
            .is_err()
        );
        let mut timed_out = std::process::Command::new("sh");
        timed_out.args(["-c", "exec sleep 2"]);
        assert!(
            crate::process::run_output_named(
                &mut timed_out,
                crate::process::ProcessPolicy::Test,
                crate::process::ProcessDescriptor::new("test.external.timed_out"),
            )
            .is_err()
        );
        let mut canceled = std::process::Command::new("sh");
        canceled.args(["-c", "exec sleep 2"]);
        let canceled_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        assert!(
            crate::process::with_cancellation(canceled_flag, || {
                crate::process::run_output_named(
                    &mut canceled,
                    crate::process::ProcessPolicy::Test,
                    crate::process::ProcessDescriptor::new("test.external.canceled"),
                )
            })
            .is_err()
        );
        let completed_path = trigger(
            &repo,
            RecordOptions {
                before_seconds: 60,
                after_seconds: 0,
            },
        )
        .unwrap();
        let completed = fs::read_to_string(completed_path).unwrap();
        assert_eq!(
            completed
                .lines()
                .filter(|line| {
                    line.contains("test.external.in_flight")
                        && line.contains("\"phase\",\"value\":\"complete\"")
                })
                .count(),
            1
        );
        assert!(completed.lines().any(|line| {
            line.contains("test.external.spawn_failed")
                && line.contains("\"outcome\",\"value\":\"spawn_failed\"")
        }));
        assert!(completed.lines().any(|line| {
            line.contains("test.external.timed_out")
                && line.contains("\"outcome\",\"value\":\"timed_out\"")
        }));
        assert!(completed.lines().any(|line| {
            line.contains("test.external.canceled")
                && line.contains("\"outcome\",\"value\":\"canceled\"")
        }));
        assert!(completed.lines().any(|line| {
            line.contains("test.external.private")
                && line.contains("\"job_id\",\"value\":77")
                && line.contains("\"job_type\",\"value\":\"test_job\"")
        }));
        assert!(!completed.contains(secret));
        drop(server);
    }

    #[cfg(unix)]
    #[test]
    fn platform_smoke_native_recorder_lock_prevents_socket_ownership_races() {
        let runtime = crate::compact_runtime::CompactTempDir::new("recorder-lock");
        let socket_path = runtime.runtime_path().join("lock.sock");
        let endpoint = || ServerEndpoint {
            socket_path: socket_path.clone(),
            output_dir: runtime.path().to_path_buf(),
        };
        let server = bind_server_in(endpoint(), runtime.runtime_path()).unwrap();

        assert!(bind_server_in(endpoint(), runtime.runtime_path()).is_err());

        drop(server);
    }

    #[cfg(unix)]
    #[test]
    fn recorder_socket_validation_uses_the_common_raw_byte_budget() {
        use std::os::unix::ffi::OsStringExt;

        let at_budget = PathBuf::from(std::ffi::OsString::from_vec(vec![
            b'a';
            RECORDER_SOCKET_PATH_BUDGET
        ]));
        let over_budget = PathBuf::from(std::ffi::OsString::from_vec(vec![
            b'a';
            RECORDER_SOCKET_PATH_BUDGET
                + 1
        ]));
        let non_utf8 = PathBuf::from(std::ffi::OsString::from_vec(b"/tmp/flight-\xff".to_vec()));

        assert!(validate_recorder_socket_path(&at_budget, "test").is_ok());
        assert!(validate_recorder_socket_path(&over_budget, "test").is_err());
        assert!(validate_recorder_socket_path(&non_utf8, "test").is_ok());
    }
}
