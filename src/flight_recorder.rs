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
use crate::util::{single_line, stable_hash, truncate};

const EVENT_CHANNEL_CAPACITY: usize = 16_384;
const CONTROL_CHANNEL_CAPACITY: usize = 8;
const RING_EVENT_CAPACITY: usize = 65_536;
const RETENTION: Duration = Duration::from_secs(60);
const MAX_BEFORE_SECONDS: u64 = 60;
const MAX_AFTER_SECONDS: u64 = 120;
const CONTROL_POLL_INTERVAL: Duration = Duration::from_millis(5);
const RESPONSE_GRACE: Duration = Duration::from_secs(10);
const SCHEMA_VERSION: u32 = 1;

static RECORDER: OnceLock<Recorder> = OnceLock::new();
static UI_THREAD: OnceLock<ThreadId> = OnceLock::new();
static NEXT_INPUT_ID: AtomicU64 = AtomicU64::new(1);
static UI_IDLE_STARTED_US: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum FieldValue {
    Unsigned(u64),
    Signed(i64),
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

pub(crate) fn signed(name: &'static str, value: i64) -> Field {
    Field {
        name,
        value: FieldValue::Signed(value),
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
        value: FieldValue::Text(truncate(&single_line(value.as_ref()), 256)),
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

#[derive(Clone, Debug)]
pub(crate) struct InputTrace {
    id: u64,
    kind: &'static str,
    started: Instant,
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
    Stop {
        paths: Vec<PathBuf>,
    },
}

struct ServerEndpoint {
    socket_path: PathBuf,
    output_dir: PathBuf,
}

#[cfg(unix)]
struct ServerSocket {
    socket: std::os::unix::net::UnixDatagram,
    socket_path: PathBuf,
    output_dir: PathBuf,
}

#[cfg(not(unix))]
struct ServerSocket;

pub(crate) struct ServerGuard {
    paths: Vec<PathBuf>,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        if self.paths.is_empty() {
            return;
        }
        let _ = recorder().control_tx.send(Control::Stop {
            paths: std::mem::take(&mut self.paths),
        });
    }
}

pub(crate) fn serve_repositories<'a>(
    repos: impl IntoIterator<Item = &'a Repository>,
) -> ServerGuard {
    let endpoints = repos
        .into_iter()
        .map(|repo| ServerEndpoint {
            socket_path: control_socket_path(repo),
            output_dir: repo.prism_dir().join("recordings"),
        })
        .collect::<Vec<_>>();
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

#[cfg(unix)]
fn trigger_unix(repo: &Repository, options: RecordOptions) -> Result<PathBuf, String> {
    use std::os::unix::net::UnixDatagram;

    let server_path = control_socket_path(repo);
    if !server_path.exists() {
        return Err(format!(
            "no running Prism TUI recorder found for {}; start Prism for this repository first",
            repo.root.display()
        ));
    }
    let client_path = client_socket_path();
    remove_socket_if_present(&client_path)?;
    let socket = UnixDatagram::bind(&client_path)
        .map_err(|error| format!("bind debug recorder response socket: {error}"))?;
    let _cleanup = SocketPathGuard(client_path.clone());
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
    let size = socket
        .recv(&mut response)
        .map_err(|error| format!("wait for debug recording: {error}"))?;
    let response: CaptureResponse = serde_json::from_slice(&response[..size])
        .map_err(|error| format!("decode debug recorder response: {error}"))?;
    if let Some(error) = response.error {
        return Err(error);
    }
    response
        .path
        .ok_or_else(|| "debug recorder returned no artifact path".to_string())
}

#[cfg(not(unix))]
fn trigger_unix(_repo: &Repository, _options: RecordOptions) -> Result<PathBuf, String> {
    Err("debug flight recording requires a Unix platform".to_string())
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
            while let Ok(control) = self.control_rx.try_recv() {
                self.handle_control(control);
            }
            self.poll_servers();
            match self.event_rx.recv_timeout(CONTROL_POLL_INTERVAL) {
                Ok(event) => {
                    self.push(event);
                    while let Ok(event) = self.event_rx.try_recv() {
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

    #[cfg(unix)]
    fn handle_control(&mut self, control: Control) {
        match control {
            Control::Serve { endpoints, reply } => {
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
                let _ = reply.send(paths);
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
        }
    }

    #[cfg(not(unix))]
    fn handle_control(&mut self, control: Control) {
        if let Control::Serve { reply, .. } = control {
            let _ = reply.send(Vec::new());
        }
    }

    #[cfg(unix)]
    fn poll_servers(&mut self) {
        let mut requests = Vec::new();
        for server in &self.servers {
            loop {
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

    #[cfg(not(unix))]
    fn poll_servers(&mut self) {}

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
        fs::create_dir_all(&capture.output_dir).map_err(|error| {
            format!(
                "create recording directory {}: {error}",
                capture.output_dir.display()
            )
        })?;
        let path = capture.output_dir.join(format!(
            "prism-recording-{}-{}.jsonl",
            capture.trigger_unix_ms,
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
        writer
            .get_ref()
            .sync_all()
            .map_err(|error| format!("sync {}: {error}", temporary.display()))?;
        fs::rename(&temporary, &path).map_err(|error| {
            format!(
                "commit debug recording {}: {error}",
                path.display()
            )
        })?;
        Ok(path)
    }

    #[cfg(unix)]
    fn remove_all_servers(&mut self) {
        for server in self.servers.drain(..) {
            let _ = fs::remove_file(server.socket_path);
        }
    }

    #[cfg(not(unix))]
    fn remove_all_servers(&mut self) {}
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
    category: &'static str,
    operation: &'static str,
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
    let mut durations = BTreeMap::<(&'static str, &'static str), Vec<u64>>::new();
    for event in events {
        if let Some(duration_us) = event.duration_us {
            durations
                .entry((event.category, event.operation))
                .or_default()
                .push(duration_us);
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
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(|error| format!("create debug recording {}: {error}", path.display()))
}

#[cfg(unix)]
fn bind_server(endpoint: ServerEndpoint) -> Result<ServerSocket, String> {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixDatagram;

    if endpoint.socket_path.exists() {
        let probe = UnixDatagram::unbound()
            .map_err(|error| format!("create recorder socket probe: {error}"))?;
        if probe.send_to(b"ping", &endpoint.socket_path).is_ok() {
            return Err("another recorder already owns the control socket".to_string());
        }
        remove_socket_if_present(&endpoint.socket_path)?;
    }
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
    })
}

fn send_response(path: &Path, result: Result<PathBuf, String>) {
    #[cfg(unix)]
    {
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
}

fn control_socket_path(repo: &Repository) -> PathBuf {
    let hash = stable_hash(&repo.prism_dir());
    PathBuf::from("/tmp").join(format!("prism-flight-{hash:016x}.sock"))
}

fn client_socket_path() -> PathBuf {
    PathBuf::from("/tmp").join(format!(
        "prism-flight-client-{}-{}.sock",
        std::process::id(),
        unix_ms()
    ))
}

fn remove_socket_if_present(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("remove stale socket {}: {error}", path.display())),
    }
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
                category: "tui",
                operation: "frame",
                count: 100,
                p50_us: 50,
                p95_us: 95,
                max_us: 100,
            }]
        );
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
                after_seconds: 121,
            }
            .validate()
            .is_err()
        );
        assert_eq!(RecordOptions::default().validate(), Ok(RecordOptions::default()));
    }
}
