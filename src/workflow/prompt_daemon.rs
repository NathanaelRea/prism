//! On-demand user-wide Worker and socket transport for prompt Workflows.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Seek as _, Write as _};
#[cfg(unix)]
use std::os::fd::AsRawFd as _;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use sha2::{Digest as _, Sha256};

use crate::platform::SupportedOs;
use crate::workflow::worker_ipc::{self, WorkerEndpoint, WorkerStream};

const PROTOCOL_VERSION: u32 = 7;
const TRANSITION_TIMEOUT: Duration = Duration::from_secs(3);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(6);
const SOCKET_IO_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_CONNECTION_HANDLERS: usize = 32;
const WORKER_OWNER_RECORD_LIMIT: u64 = 16 * 1024;
#[cfg(not(test))]
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(test)]
const REQUEST_READ_TIMEOUT: Duration = Duration::from_millis(250);
const RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_LIST_PAGE_SIZE: usize = 64;
const INCOMPLETE_RESPONSE_ERROR: &str = "closed connection without a complete response";

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
struct WorkerOwnerRecord {
    protocol_version: u32,
    pid: u32,
    process_identity: Option<u64>,
    binary_generation: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonState {
    Running,
    Draining,
    Stopped,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct DaemonHealth {
    pub state: DaemonState,
    pub protocol_version: Option<u32>,
    pub instance_id: Option<String>,
    pub pid: Option<u32>,
    pub binary_generation: Option<String>,
    pub active: usize,
    pub notifications: bool,
}

impl DaemonHealth {
    pub fn stopped() -> Self {
        Self {
            state: DaemonState::Stopped,
            protocol_version: None,
            instance_id: None,
            pid: None,
            binary_generation: None,
            active: 0,
            notifications: false,
        }
    }
}

pub fn probe_health() -> Result<DaemonHealth, String> {
    let endpoint = validated_socket_path()?;
    let stream = match endpoint.connect() {
        Ok(stream) => stream,
        Err(error) if worker_ipc::endpoint_unavailable(&error) => {
            return Ok(DaemonHealth::stopped());
        }
        Err(error) => return Err(format!("connect to Prism worker: {error}")),
    };
    parse_health(&request_authenticated_with_timeout(
        &endpoint,
        stream,
        "health",
        SOCKET_IO_TIMEOUT,
    )?)
}

pub fn ensure_running() -> Result<(), String> {
    let generation = binary_generation()?;
    match probe_health() {
        Ok(health) if worker_matches_generation(&health, &generation) => return Ok(()),
        Ok(health) if health.state == DaemonState::Stopped => {}
        Ok(_) => {
            let _ = request_with_timeout("shutdown", SOCKET_IO_TIMEOUT);
            if let Err(error) = wait_stopped(TRANSITION_TIMEOUT)
                && !terminate_old_generation_worker(&generation)?
            {
                return Err(error);
            }
        }
        Err(error) => {
            if !terminate_old_generation_worker(&generation)? {
                return Err(error);
            }
        }
    }
    let executable =
        worker_executable().map_err(|error| format!("resolve Prism worker executable: {error}"))?;
    let command = crate::process::Command::new(executable)
        .args(["worker", "serve"])
        .env("PRISM_WORKER_GENERATION", &generation);
    let _pid = spawn_detached_worker(command)?;
    let deadline = Instant::now() + TRANSITION_TIMEOUT;
    let mut last = "worker did not become ready".to_string();
    while Instant::now() < deadline {
        match probe_health() {
            Ok(health) if worker_matches_generation(&health, &generation) => return Ok(()),
            Ok(health) => last = format!("worker state is {:?}", health.state),
            Err(error) => last = error,
        }
        thread::sleep(Duration::from_millis(25));
    }
    Err(last)
}

fn worker_executable() -> io::Result<PathBuf> {
    let current = std::env::current_exe()?;
    #[cfg(test)]
    if current.parent().and_then(Path::file_name) == Some(std::ffi::OsStr::new("deps"))
        && let Some(profile_directory) = current.parent().and_then(Path::parent)
    {
        let executable = profile_directory.join(format!("prism{}", std::env::consts::EXE_SUFFIX));
        if executable.is_file() {
            return Ok(executable);
        }
    }
    Ok(current)
}

fn spawn_detached_worker(command: crate::process::Command) -> Result<u32, String> {
    use crate::flight_recorder::{
        ExternalCallCategory, ExternalCallOutcome, ExternalCallTrace, text, unsigned,
    };

    let mut trace = ExternalCallTrace::begin(
        ExternalCallCategory::Process,
        "prism.worker.serve",
        vec![text("policy", "detached")],
    );
    match command.spawn_detached() {
        Ok(child) => {
            let pid = child.pid();
            trace.finish(
                ExternalCallOutcome::Success,
                vec![
                    text("completion", "detached"),
                    text("termination_stage", "none"),
                    unsigned("child_pid", pid),
                    unsigned("stdout_bytes", 0_u64),
                    unsigned("stderr_bytes", 0_u64),
                    crate::flight_recorder::boolean("stdout_truncated", false),
                    crate::flight_recorder::boolean("stderr_truncated", false),
                ],
            );
            // ProcessKit's detached handle intentionally has no ownership or
            // control semantics; it is dropped here without stopping the worker.
            Ok(pid)
        }
        Err(error) => {
            trace.finish(
                ExternalCallOutcome::SpawnFailed,
                vec![
                    text("completion", "spawn_failed"),
                    text("termination_stage", "none"),
                    text("error_kind", error.kind().name()),
                    unsigned("stdout_bytes", 0_u64),
                    unsigned("stderr_bytes", 0_u64),
                    crate::flight_recorder::boolean("stdout_truncated", false),
                    crate::flight_recorder::boolean("stderr_truncated", false),
                ],
            );
            Err(format!("start Prism worker daemon: {error}"))
        }
    }
}

fn worker_matches_generation(health: &DaemonHealth, generation: &str) -> bool {
    health.state == DaemonState::Running
        && health.protocol_version == Some(PROTOCOL_VERSION)
        && health.binary_generation.as_deref() == Some(generation)
}

fn ensure_compatible_running() -> Result<(), String> {
    let health = probe_health()?;
    if health.state == DaemonState::Running && health.protocol_version == Some(PROTOCOL_VERSION) {
        return Ok(());
    }
    if health.state == DaemonState::Draining {
        wait_stopped(TRANSITION_TIMEOUT)?;
    }
    ensure_running()
}

pub fn health_response() -> Result<String, String> {
    request("health")
}

pub fn shutdown() -> Result<(), String> {
    if probe_health()?.state == DaemonState::Stopped {
        return Ok(());
    }
    let response = request("shutdown")?;
    let health = parse_health(&response)?;
    if health.state != DaemonState::Draining {
        return Err(format!("Prism worker rejected shutdown: {response}"));
    }
    wait_stopped(SHUTDOWN_TIMEOUT)
}

fn wait_stopped(timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match probe_health() {
            Ok(health) if health.state == DaemonState::Stopped => return Ok(()),
            Ok(_) => {}
            Err(error) if is_worker_transition_error(&error) && worker_lock_is_available()? => {
                return Ok(());
            }
            Err(error) if is_worker_transition_error(&error) => {}
            Err(error) => return Err(error),
        }
        thread::sleep(Duration::from_millis(25));
    }
    Err("timed out waiting for Prism worker daemon to stop".into())
}

fn worker_lock_is_available() -> Result<bool, String> {
    let directory = runtime_dir();
    if !directory.exists() {
        return Ok(true);
    }
    let path = directory.join("worker.lock");
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    let file = options
        .open(&path)
        .map_err(|error| format!("open Prism worker ownership lock: {error}"))?;
    #[cfg(unix)]
    {
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            let _ = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            return Ok(false);
        }
        Err(format!("probe Prism worker ownership lock: {error}"))
    }
    #[cfg(windows)]
    {
        match fs4::FileExt::try_lock(&file) {
            Ok(()) => {
                let _ = fs4::FileExt::unlock(&file);
                Ok(true)
            }
            Err(fs4::TryLockError::WouldBlock) => Ok(false),
            Err(error) => Err(format!("probe Prism worker ownership lock: {error}")),
        }
    }
}

fn write_worker_owner(lock: &mut File, generation: &str) -> Result<(), String> {
    let process = crate::process::record_process(std::process::id())
        .map_err(|error| format!("record Prism worker process identity: {error}"))?;
    let process_identity = process
        .identity
        .map(crate::process::ProcessIdentity::stored_value)
        .ok_or_else(|| {
            "record Prism worker process identity: reusable identity is unavailable".to_string()
        })?;
    let owner = WorkerOwnerRecord {
        protocol_version: PROTOCOL_VERSION,
        pid: process.pid,
        process_identity: Some(process_identity),
        binary_generation: generation.to_string(),
    };
    lock.set_len(0)
        .map_err(|error| format!("truncate Prism worker ownership record: {error}"))?;
    lock.rewind()
        .map_err(|error| format!("rewind Prism worker ownership record: {error}"))?;
    serde_json::to_writer(&mut *lock, &owner)
        .map_err(|error| format!("write Prism worker ownership record: {error}"))?;
    lock.write_all(b"\n")
        .map_err(|error| format!("finish Prism worker ownership record: {error}"))?;
    lock.flush()
        .map_err(|error| format!("flush Prism worker ownership record: {error}"))?;
    lock.sync_data()
        .map_err(|error| format!("sync Prism worker ownership record: {error}"))
}

fn read_worker_owner() -> Result<Option<WorkerOwnerRecord>, String> {
    read_worker_owner_at(&runtime_dir().join("worker.lock"))
}

fn read_worker_owner_at(path: &Path) -> Result<Option<WorkerOwnerRecord>, String> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("open Prism worker ownership record: {error}")),
    };
    let mut bytes = Vec::new();
    file.take(WORKER_OWNER_RECORD_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read Prism worker ownership record: {error}"))?;
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.len() as u64 > WORKER_OWNER_RECORD_LIMIT {
        return Err("Prism worker ownership record is too large".into());
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| format!("parse Prism worker ownership record: {error}"))
}

fn terminate_old_generation_worker(generation: &str) -> Result<bool, String> {
    let registered = read_worker_owner()?;
    let Some(owner) = registered
        .clone()
        .map_or_else(discover_legacy_worker_owner, |owner| Ok(Some(owner)))?
    else {
        return Ok(false);
    };
    if owner.binary_generation == generation {
        return Ok(false);
    }
    let Some(identity) = owner.process_identity else {
        return Err(format!(
            "cannot safely replace old Prism worker {}: process identity is unavailable",
            owner.pid
        ));
    };
    let latest = read_worker_owner()?;
    if registered.as_ref().map_or_else(
        || latest.is_some(),
        |registered| latest.as_ref() != Some(registered),
    ) {
        return Err("Prism worker ownership changed during replacement".into());
    }
    let recorded = crate::process::RecordedProcess::from_stored(owner.pid, Some(identity));
    let outcome = crate::async_runtime::block_on(crate::process::terminate_recorded_process(
        recorded,
        Duration::from_secs(1),
    ))
    .map_err(|error| format!("replace old Prism worker {}: {error}", owner.pid))?
    .map_err(|error| format!("replace old Prism worker {}: {error}", owner.pid))?;
    match outcome {
        crate::process::TerminationOutcome::Terminated
        | crate::process::TerminationOutcome::AlreadyExited => {
            wait_stopped(TRANSITION_TIMEOUT)?;
            Ok(true)
        }
        crate::process::TerminationOutcome::IdentityReused => Err(format!(
            "refusing to replace old Prism worker {}: PID identity was reused",
            owner.pid
        )),
        crate::process::TerminationOutcome::Unverifiable => Err(format!(
            "refusing to replace old Prism worker {}: process identity cannot be verified",
            owner.pid
        )),
    }
}

fn discover_legacy_worker_owner() -> Result<Option<WorkerOwnerRecord>, String> {
    if worker_lock_is_available()? {
        return Ok(None);
    }
    #[cfg(windows)]
    return Err("cannot safely replace an unregistered legacy Prism worker on Windows".to_string());
    #[cfg(unix)]
    {
        let endpoint = validated_socket_path()?;
        let stream = endpoint
            .connect()
            .map_err(|error| format!("connect to legacy Prism worker: {error}"))?;
        let pid = worker_ipc::peer_process_id(&stream)
            .map_err(|error| format!("identify legacy Prism worker socket peer: {error}"))?;
        let arguments = crate::process::process_arguments(pid)
            .map_err(|error| format!("inspect legacy Prism worker {pid}: {error}"))?;
        if !matches!(
            arguments.as_deref(),
            Some([_, worker, serve]) if worker == "worker" && serve == "serve"
        ) {
            return Err(format!(
                "refusing to replace unregistered Prism worker socket peer {pid}: unexpected process arguments"
            ));
        }
        let process = crate::process::record_process(pid)
            .map_err(|error| format!("record legacy Prism worker {pid} identity: {error}"))?;
        Ok(Some(WorkerOwnerRecord {
            protocol_version: 0,
            pid,
            process_identity: process
                .identity
                .map(crate::process::ProcessIdentity::stored_value),
            binary_generation: "legacy-unregistered".into(),
        }))
    }
}

fn is_worker_transition_error(error: &str) -> bool {
    error.contains("Connection reset by peer")
        || error.contains("Broken pipe")
        || error.contains("connection refused")
        || error.contains("Connection refused")
        || error.contains(INCOMPLETE_RESPONSE_ERROR)
}

pub async fn serve() -> Result<(), String> {
    let generation = std::env::var("PRISM_WORKER_GENERATION")
        .ok()
        .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map_or_else(binary_generation, Ok)?;
    let directory = runtime_dir();
    worker_ipc::prepare_runtime(&directory)?;
    let mut lock = acquire_lock(&directory.join("worker.lock"))?;
    write_worker_owner(&mut lock, &generation)?;
    let endpoint = validated_socket_path()?;
    match endpoint.connect() {
        Ok(_) => return Err("a live Prism worker already owns the endpoint".into()),
        Err(error) if worker_ipc::endpoint_unavailable(&error) => {}
        Err(error) => {
            return Err(format!(
                "cannot safely classify existing Prism worker endpoint: {error}"
            ));
        }
    }
    endpoint
        .remove_stale_address()
        .map_err(|error| format!("remove stale worker endpoint: {error}"))?;
    let service = crate::PromptWorkflowService::open(
        &crate::PromptWorkflowService::database_path(),
        &crate::PromptWorkflowService::state_root(),
    )
    .await
    .map_err(|error| format!("open prompt Workflow service: {error}"))?;
    let (shutdown, mut shutdown_receiver) = tokio::sync::watch::channel(false);
    let background = service.clone();
    let scheduler = tokio::spawn(async move {
        loop {
            if *shutdown_receiver.borrow() {
                break;
            }
            if let Err(error) = background
                .tick_active(crate::workflow::prompt_worker::now_unix_ms())
                .await
            {
                eprintln!("Prism prompt Workflow scheduler failed: {error}");
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(250)) => {}
                changed = shutdown_receiver.changed() => {
                    if changed.is_err() || *shutdown_receiver.borrow() { break; }
                }
            }
        }
    });
    let result = serve_socket(&service, &generation, &lock).await;
    let _ = shutdown.send(true);
    scheduler
        .await
        .map_err(|error| format!("join prompt Workflow scheduler: {error}"))?;
    result
}

#[derive(Default)]
struct DaemonControl {
    draining: AtomicBool,
    handlers: AtomicUsize,
    requests: AtomicUsize,
}

struct HandlerGuard(Arc<DaemonControl>);

impl Drop for HandlerGuard {
    fn drop(&mut self) {
        self.0.handlers.fetch_sub(1, Ordering::AcqRel);
    }
}

struct RequestGuard<'a>(&'a DaemonControl);

impl Drop for RequestGuard<'_> {
    fn drop(&mut self) {
        self.0.requests.fetch_sub(1, Ordering::AcqRel);
    }
}

async fn serve_socket(
    service: &crate::PromptWorkflowService,
    generation: &str,
    _lock: &File,
) -> Result<(), String> {
    let endpoint = validated_socket_path()?;
    let secret = worker_ipc::create_secret(&endpoint)?;
    let listener = endpoint
        .bind()
        .map_err(|error| format!("bind Prism worker endpoint {}: {error}", endpoint.display()))?;
    worker_ipc::secure_listener(&endpoint)?;
    worker_ipc::set_listener_nonblocking(&listener)
        .map_err(|error| format!("configure worker endpoint: {error}"))?;
    let instance = format!(
        "daemon-{}-{}",
        std::process::id(),
        crate::workflow::prompt_worker::now_unix_ms()
    );
    let control = Arc::new(DaemonControl::default());
    let mut handlers = tokio::task::JoinSet::new();
    let mut serve_error = None;
    loop {
        while let Some(result) = handlers.try_join_next() {
            if let Err(error) = result {
                serve_error.get_or_insert_with(|| {
                    format!("join Prism worker connection handler: {error}")
                });
            }
        }
        if control.draining.load(Ordering::Acquire) {
            if control.handlers.load(Ordering::Acquire) == 0
                && control.requests.load(Ordering::Acquire) == 0
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            continue;
        }
        match worker_ipc::accept(&listener) {
            Ok(mut stream) => {
                if control.handlers.load(Ordering::Acquire) >= MAX_CONNECTION_HANDLERS {
                    handlers.spawn(async move {
                        let _ = tokio::task::spawn_blocking(move || {
                            write_with_deadline(
                                &mut stream,
                                b"error worker-busy\n",
                                RESPONSE_WRITE_TIMEOUT,
                            )
                        })
                        .await;
                    });
                    continue;
                }
                control.handlers.fetch_add(1, Ordering::AcqRel);
                let service = service.clone();
                let secret = secret.clone();
                let instance = instance.clone();
                let generation = generation.to_string();
                let handler_control = Arc::clone(&control);
                handlers.spawn(async move {
                    let _guard = HandlerGuard(Arc::clone(&handler_control));
                    respond(
                        &service,
                        stream,
                        &secret,
                        &instance,
                        &generation,
                        &handler_control,
                    )
                    .await;
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => {
                serve_error = Some(format!("accept Prism worker connection: {error}"));
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    drop(listener);
    while let Some(result) = handlers.join_next().await {
        if let Err(error) = result {
            serve_error
                .get_or_insert_with(|| format!("join Prism worker connection handler: {error}"));
        }
    }
    let cleanup = endpoint
        .remove_stale_address()
        .map_err(|error| format!("remove Prism worker endpoint: {error}"));
    match (serve_error, cleanup) {
        (Some(error), _) => Err(error),
        (None, result) => result,
    }
}

async fn respond(
    service: &crate::PromptWorkflowService,
    stream: WorkerStream,
    secret: &str,
    instance: &str,
    generation: &str,
    control: &DaemonControl,
) {
    // Local-socket streams expose synchronous I/O. Keep each accepted
    // connection's bounded read/write deadlines off Tokio worker threads.
    let read = tokio::task::spawn_blocking(move || {
        let mut stream = stream;
        let request = read_request_line(&mut stream);
        (stream, request)
    })
    .await;
    let Ok((mut stream, request)) = read else {
        return;
    };
    let Ok(request) = request else {
        let _ = tokio::task::spawn_blocking(move || {
            write_with_deadline(
                &mut stream,
                b"error invalid-request\n",
                RESPONSE_WRITE_TIMEOUT,
            )
        })
        .await;
        return;
    };
    let request = request.trim();
    let Some(request) = authenticate_command(secret, request) else {
        let _ = tokio::task::spawn_blocking(move || {
            write_with_deadline(
                &mut stream,
                b"error authentication-failed\n",
                RESPONSE_WRITE_TIMEOUT,
            )
        })
        .await;
        return;
    };
    let active = service.active_count().await.unwrap_or(0);
    let response = match request {
        "health" | "wake" => health_line(
            instance,
            generation,
            control.draining.load(Ordering::Acquire),
            active,
        ),
        "shutdown" => {
            control.draining.store(true, Ordering::Release);
            health_line(instance, generation, true, active)
        }
        request if request.starts_with('{') => {
            if control.draining.load(Ordering::Acquire) {
                json_error("Prism worker is draining".to_string())
            } else {
                control.requests.fetch_add(1, Ordering::AcqRel);
                let _guard = RequestGuard(control);
                prompt_response(service, request).await
            }
        }
        _ => "error unknown-command\n".into(),
    };
    let response = bounded_response(response);
    let _ = tokio::task::spawn_blocking(move || {
        write_with_deadline(&mut stream, response.as_bytes(), RESPONSE_WRITE_TIMEOUT)
    })
    .await;
}

fn read_request_line(stream: &mut WorkerStream) -> io::Result<String> {
    worker_ipc::set_stream_nonblocking(stream, true)?;
    let deadline = Instant::now() + REQUEST_READ_TIMEOUT;
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "worker request deadline exceeded",
            ));
        }
        let available = MAX_REQUEST_BYTES + 1 - request.len();
        if available == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "worker request exceeds the size limit",
            ));
        }
        let read_len = available.min(buffer.len());
        let count = match stream.read(&mut buffer[..read_len]) {
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                wait_for_io(deadline)?;
                continue;
            }
            Err(error) => return Err(error),
        };
        if count == 0 {
            #[cfg(windows)]
            {
                wait_for_io(deadline)?;
                continue;
            }
            #[cfg(unix)]
            break;
        }
        let line_end = buffer[..count]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(count, |index| index + 1);
        request.extend_from_slice(&buffer[..line_end]);
        if request.len() > MAX_REQUEST_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "worker request exceeds the size limit",
            ));
        }
        if line_end < count || request.last() == Some(&b'\n') {
            break;
        }
    }
    String::from_utf8(request).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn write_with_deadline(
    stream: &mut WorkerStream,
    bytes: &[u8],
    timeout: Duration,
) -> io::Result<()> {
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "worker response exceeds the size limit",
        ));
    }
    let deadline = Instant::now() + timeout;
    let mut written = 0;
    while written < bytes.len() {
        match stream.write(&bytes[written..]) {
            Ok(0) => {
                #[cfg(windows)]
                {
                    wait_for_io(deadline)?;
                    continue;
                }
                #[cfg(unix)]
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "worker endpoint stopped accepting response bytes",
                ));
            }
            Ok(count) => written += count,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => wait_for_io(deadline)?,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn wait_for_io(deadline: Instant) -> io::Result<()> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "worker endpoint I/O deadline exceeded",
        ));
    }
    thread::sleep(remaining.min(Duration::from_millis(5)));
    Ok(())
}

fn health_line(instance: &str, generation: &str, draining: bool, active: usize) -> String {
    format!(
        "ok {PROTOCOL_VERSION} {instance} pid={} generation={generation} state={} active={active} notifications={}\n",
        std::process::id(),
        if draining { "draining" } else { "running" },
        u8::from(!cfg!(windows)),
    )
}

#[derive(serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SocketRequest {
    #[serde(rename = "prompt_workflow_launch")]
    Launch {
        workflow: Box<crate::CompiledWorkflow>,
        run_id: String,
        subject: crate::TriggerSubject,
        now_unix_ms: i64,
    },
    #[serde(rename = "prompt_workflow_list")]
    List {
        repository: Option<String>,
        page_size: usize,
        cursor: Option<crate::persistence::workflow_kernel::WorkflowRunCursor>,
    },
    #[serde(rename = "prompt_workflow_inspect")]
    Inspect { run_id: String },
    #[serde(rename = "prompt_workflow_command")]
    Command {
        run_id: String,
        command: SocketControl,
        now_unix_ms: i64,
    },
    RemoteObserve {
        repository: PathBuf,
        worktree: PathBuf,
        #[serde(flatten)]
        operation: super::remote_operation::RemoteObservationOperation,
        subject: String,
    },
    RemoteMutate {
        repository: PathBuf,
        worktree: PathBuf,
        request_id: String,
        #[serde(flatten)]
        operation: Box<super::remote_operation::RemoteMutationOperation>,
        subject: String,
    },
    RemoteReconcile {
        repository: PathBuf,
        worktree: PathBuf,
        request_id: String,
        #[serde(flatten)]
        operation: Box<super::remote_operation::RemoteMutationOperation>,
        subject: String,
        reconciliation: crate::remote::request_coordinator::RemoteMutationReconciliation,
    },
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum SocketControl {
    Pause,
    Resume,
    Cancel,
    Retry,
    Recover {
        resolution: crate::workflow::kernel::RecoveryResolution,
    },
    Discard,
}

async fn prompt_response(service: &crate::PromptWorkflowService, request: &str) -> String {
    let request = match serde_json::from_str::<SocketRequest>(request) {
        Ok(request) => request,
        Err(error) => return json_error(format!("invalid Worker request: {error}")),
    };
    let result = match request {
        SocketRequest::Launch {
            workflow,
            run_id,
            subject,
            now_unix_ms,
        } => service
            .launch(*workflow, &run_id, subject, now_unix_ms)
            .await
            .map(|run| serde_json::json!({"ok": true, "run_id": run.id}))
            .map_err(|error| error.to_string()),
        SocketRequest::List {
            repository,
            page_size,
            cursor,
        } => service
            .list_page(
                repository.as_deref().map(Path::new),
                page_size.clamp(1, MAX_LIST_PAGE_SIZE),
                cursor.as_ref(),
            )
            .await
            .map(|page| {
                serde_json::json!({
                    "ok": true,
                    "runs": page.runs,
                    "next_cursor": page.next_cursor,
                })
            })
            .map_err(|error| error.to_string()),
        SocketRequest::Inspect { run_id } => service
            .inspect(&run_id)
            .await
            .map(|run| serde_json::json!({"ok": true, "run": run}))
            .map_err(|error| error.to_string()),
        SocketRequest::Command {
            run_id,
            command,
            now_unix_ms,
        } => {
            let result = match command {
                SocketControl::Pause => service.pause(&run_id, now_unix_ms).await,
                SocketControl::Resume => service.resume(&run_id, now_unix_ms).await,
                SocketControl::Cancel => service.cancel(&run_id, now_unix_ms).await,
                SocketControl::Retry => service.retry(&run_id, now_unix_ms).await,
                SocketControl::Recover { resolution } => {
                    service.recover(&run_id, now_unix_ms, resolution).await
                }
                SocketControl::Discard => service.discard(&run_id, now_unix_ms).await,
            };
            result
                .map(|()| serde_json::json!({"ok": true}))
                .map_err(|error| error.to_string())
        }
        SocketRequest::RemoteObserve {
            repository,
            worktree,
            operation,
            subject,
        } => service
            .remote_observe(&repository, &worktree, operation, subject)
            .await
            .and_then(|result| match result {
                crate::remote::request_coordinator::RemoteObservationResult::Fresh(value) => {
                    Ok(serde_json::json!({"ok": true, "state": "fresh", "value": value.value}))
                }
                crate::remote::request_coordinator::RemoteObservationResult::Pending(wait) => {
                    Ok(serde_json::json!({"ok": true, "state": "pending", "wait": wait}))
                }
                crate::remote::request_coordinator::RemoteObservationResult::Failed(reason) => {
                    Err(reason)
                }
            }),
        SocketRequest::RemoteMutate {
            repository,
            worktree,
            request_id,
            operation,
            subject,
        } => service
            .remote_mutate(&repository, &worktree, request_id, *operation, subject)
            .await
            .map(|result| match result {
                crate::remote::request_coordinator::RemoteMutationResult::Applied(value) => {
                    serde_json::json!({"ok": true, "state": "applied", "value": value})
                }
                crate::remote::request_coordinator::RemoteMutationResult::Pending(wait) => {
                    serde_json::json!({"ok": true, "state": "pending", "wait": wait})
                }
                crate::remote::request_coordinator::RemoteMutationResult::Failed {
                    reason,
                    disposition,
                } => serde_json::json!({
                    "ok": true,
                    "state": "failed",
                    "reason": reason,
                    "disposition": disposition,
                }),
            }),
        SocketRequest::RemoteReconcile {
            repository,
            worktree,
            request_id,
            operation,
            subject,
            reconciliation,
        } => service
            .reconcile_remote_mutation(
                &repository,
                &worktree,
                request_id,
                *operation,
                subject,
                reconciliation,
            )
            .await
            .map(|()| serde_json::json!({"ok": true})),
    };
    match result {
        Ok(value) => format!("{value}\n"),
        Err(error) => json_error(error.to_string()),
    }
}

fn json_error(error: String) -> String {
    bounded_response(format!(
        "{}\n",
        serde_json::json!({"ok": false, "error": error})
    ))
}

fn bounded_response(response: String) -> String {
    if response.len() <= MAX_RESPONSE_BYTES {
        response
    } else {
        "{\"ok\":false,\"error\":\"worker response exceeds the size limit\"}\n".to_string()
    }
}

const WORKER_CHANNEL_ERROR_PREFIX: &str = "worker channel error: ";

fn worker_request(value: serde_json::Value) -> Result<serde_json::Value, String> {
    let response = request(&value.to_string())
        .map_err(|error| format!("{WORKER_CHANNEL_ERROR_PREFIX}{error}"))?;
    let response: serde_json::Value = serde_json::from_str(&response)
        .map_err(|error| format!("{WORKER_CHANNEL_ERROR_PREFIX}decode Worker response: {error}"))?;
    if response["ok"] == true {
        Ok(response)
    } else {
        Err(response["error"]
            .as_str()
            .unwrap_or("Workflow operation failed")
            .to_string())
    }
}

pub fn launch_prompt_workflow(
    workflow: &crate::CompiledWorkflow,
    run_id: &str,
    subject: &crate::TriggerSubject,
) -> Result<String, String> {
    ensure_running()?;
    let response = worker_request(serde_json::json!({
        "type": "prompt_workflow_launch",
        "workflow": workflow,
        "run_id": run_id,
        "subject": subject,
        "now_unix_ms": crate::workflow::prompt_worker::now_unix_ms(),
    }))?;
    response["run_id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "Worker launch response omitted run_id".into())
}

pub fn list_prompt_workflows(
    repository: Option<&Path>,
    limit: usize,
) -> Result<Vec<crate::WorkflowRunState>, String> {
    let target = limit.min(10_000);
    let mut runs = Vec::with_capacity(target.min(MAX_LIST_PAGE_SIZE));
    let mut cursor: Option<crate::persistence::workflow_kernel::WorkflowRunCursor> = None;
    while runs.len() < target {
        let response = worker_request(serde_json::json!({
            "type": "prompt_workflow_list",
            "repository": repository.map(|path| path.to_string_lossy().into_owned()),
            "page_size": (target - runs.len()).min(MAX_LIST_PAGE_SIZE),
            "cursor": cursor,
        }))?;
        let mut page: Vec<crate::WorkflowRunState> =
            serde_json::from_value(response["runs"].clone())
                .map_err(|error| format!("decode Workflow list: {error}"))?;
        runs.append(&mut page);
        let next: Option<crate::persistence::workflow_kernel::WorkflowRunCursor> =
            serde_json::from_value(response["next_cursor"].clone())
                .map_err(|error| format!("decode Workflow list cursor: {error}"))?;
        let Some(next) = next else {
            break;
        };
        if cursor.as_ref() == Some(&next) {
            return Err("Worker Workflow list cursor did not advance".to_string());
        }
        cursor = Some(next);
    }
    runs.truncate(target);
    Ok(runs)
}

pub fn inspect_prompt_workflow(run_id: &str) -> Result<Option<crate::WorkflowRunState>, String> {
    ensure_running()?;
    let response = worker_request(serde_json::json!({
        "type": "prompt_workflow_inspect",
        "run_id": run_id,
    }))?;
    serde_json::from_value(response["run"].clone())
        .map_err(|error| format!("decode Workflow inspection: {error}"))
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptWorkflowControl {
    Pause,
    Resume,
    Cancel,
    Retry,
    Recover {
        resolution: crate::workflow::kernel::RecoveryResolution,
    },
    Discard,
}

pub fn command_prompt_workflow(run_id: &str, command: PromptWorkflowControl) -> Result<(), String> {
    ensure_running()?;
    worker_request(serde_json::json!({
        "type": "prompt_workflow_command",
        "run_id": run_id,
        "command": command,
        "now_unix_ms": crate::workflow::prompt_worker::now_unix_ms(),
    }))?;
    Ok(())
}

pub(crate) fn observe_remote<T: serde::de::DeserializeOwned>(
    repository: &Path,
    worktree: &Path,
    operation: super::remote_operation::RemoteObservationOperation,
    subject: &str,
) -> Result<T, String> {
    observe_remote_with_progress(repository, worktree, operation, subject, |_| {}, || false)
}

pub(crate) fn observe_remote_with_progress<T, F, C>(
    repository: &Path,
    worktree: &Path,
    operation: super::remote_operation::RemoteObservationOperation,
    subject: &str,
    on_wait: F,
    is_cancelled: C,
) -> Result<T, String>
where
    T: serde::de::DeserializeOwned,
    F: FnMut(crate::remote::request_coordinator::RemoteWait),
    C: Fn() -> bool,
{
    ensure_compatible_running()?;
    let mut request = serde_json::to_value(operation)
        .map_err(|error| format!("encode remote observation: {error}"))?;
    let object = request
        .as_object_mut()
        .expect("operation serializes as object");
    object.insert("type".into(), "remote_observe".into());
    object.insert(
        "repository".into(),
        serde_json::to_value(repository).unwrap(),
    );
    object.insert("worktree".into(), serde_json::to_value(worktree).unwrap());
    object.insert("subject".into(), subject.into());
    coordinated_remote_request(request, "fresh", on_wait, is_cancelled)
}

pub(crate) struct RemoteRequestProgress<F, C> {
    on_wait: F,
    is_cancelled: C,
}

impl<F, C> RemoteRequestProgress<F, C> {
    pub(crate) fn new(on_wait: F, is_cancelled: C) -> Self {
        Self {
            on_wait,
            is_cancelled,
        }
    }
}

const UNCERTAIN_REMOTE_MUTATION_PREFIX: &str = "uncertain remote mutation: ";

pub(crate) fn remote_mutation_error_is_uncertain(error: &str) -> bool {
    error.starts_with(UNCERTAIN_REMOTE_MUTATION_PREFIX)
}

pub(crate) fn mutate_remote_with_progress<T, F, C>(
    repository: &Path,
    worktree: &Path,
    request_id: &str,
    operation: super::remote_operation::RemoteMutationOperation,
    subject: &str,
    progress: RemoteRequestProgress<F, C>,
) -> Result<T, String>
where
    T: serde::de::DeserializeOwned,
    F: FnMut(crate::remote::request_coordinator::RemoteWait),
    C: Fn() -> bool,
{
    ensure_running()?;
    let mut request = serde_json::to_value(operation)
        .map_err(|error| format!("encode remote mutation: {error}"))?;
    let object = request
        .as_object_mut()
        .expect("operation serializes as object");
    object.insert("type".into(), "remote_mutate".into());
    object.insert(
        "repository".into(),
        serde_json::to_value(repository).unwrap(),
    );
    object.insert("worktree".into(), serde_json::to_value(worktree).unwrap());
    object.insert("request_id".into(), request_id.into());
    object.insert("subject".into(), subject.into());
    coordinated_remote_request(request, "applied", progress.on_wait, progress.is_cancelled)
}

#[derive(serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum CoordinatedRemoteResponse {
    Fresh {
        value: serde_json::Value,
    },
    Applied {
        value: serde_json::Value,
    },
    Pending {
        wait: crate::remote::request_coordinator::RemoteWait,
    },
    Failed {
        reason: String,
        disposition: crate::remote::request_coordinator::RemoteMutationFailureDisposition,
    },
}

pub(crate) fn reconcile_remote_mutation(
    repository: &Path,
    worktree: &Path,
    request_id: &str,
    operation: super::remote_operation::RemoteMutationOperation,
    subject: &str,
    reconciliation: crate::remote::request_coordinator::RemoteMutationReconciliation,
) -> Result<(), String> {
    ensure_running()?;
    let mut request = serde_json::to_value(operation)
        .map_err(|error| format!("encode remote reconciliation: {error}"))?;
    let object = request
        .as_object_mut()
        .expect("operation serializes as object");
    object.insert("type".into(), "remote_reconcile".into());
    object.insert(
        "repository".into(),
        serde_json::to_value(repository).unwrap(),
    );
    object.insert("worktree".into(), serde_json::to_value(worktree).unwrap());
    object.insert("request_id".into(), request_id.into());
    object.insert("subject".into(), subject.into());
    object.insert(
        "reconciliation".into(),
        serde_json::to_value(reconciliation)
            .map_err(|error| format!("encode remote reconciliation result: {error}"))?,
    );
    worker_request(request)?;
    Ok(())
}

fn coordinated_remote_request<T, F, C>(
    request_value: serde_json::Value,
    completed_state: &str,
    mut on_wait: F,
    is_cancelled: C,
) -> Result<T, String>
where
    T: serde::de::DeserializeOwned,
    F: FnMut(crate::remote::request_coordinator::RemoteWait),
    C: Fn() -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(90);
    let mutation = completed_state == "applied";
    let uncertain = |reason: String| format!("{UNCERTAIN_REMOTE_MUTATION_PREFIX}{reason}");
    let mut pending = false;
    loop {
        if is_cancelled() {
            let reason = "remote request cancelled while queued".to_string();
            return Err(if mutation && pending {
                uncertain(reason)
            } else {
                reason
            });
        }
        let response = match worker_request(request_value.clone()) {
            Ok(response) => response,
            Err(error) if mutation && error.starts_with(WORKER_CHANNEL_ERROR_PREFIX) => {
                return Err(uncertain(
                    error
                        .strip_prefix(WORKER_CHANNEL_ERROR_PREFIX)
                        .unwrap_or(&error)
                        .to_string(),
                ));
            }
            Err(error) => return Err(error),
        };
        let response: CoordinatedRemoteResponse = match serde_json::from_value(response) {
            Ok(response) => response,
            Err(error) if mutation => {
                return Err(uncertain(format!(
                    "decode coordinated remote response: {error}"
                )));
            }
            Err(error) => {
                return Err(format!("decode coordinated remote response: {error}"));
            }
        };
        let wait = match response {
            CoordinatedRemoteResponse::Fresh { value } if completed_state == "fresh" => {
                return serde_json::from_value(value)
                    .map_err(|error| format!("decode coordinated remote response: {error}"));
            }
            CoordinatedRemoteResponse::Applied { value } if completed_state == "applied" => {
                return serde_json::from_value(value).map_err(|error| {
                    uncertain(format!("decode coordinated remote response: {error}"))
                });
            }
            CoordinatedRemoteResponse::Failed {
                reason,
                disposition: crate::remote::request_coordinator::RemoteMutationFailureDisposition::OutcomeUncertain,
            } => return Err(uncertain(reason)),
            CoordinatedRemoteResponse::Failed { reason, .. } => return Err(reason),
            CoordinatedRemoteResponse::Pending { wait } => {
                pending = true;
                on_wait(wait.clone());
                wait
            }
            _ if mutation => {
                return Err(uncertain(
                    "Worker returned the wrong coordinated remote response state".into(),
                ));
            }
            _ => return Err("Worker returned the wrong coordinated remote response state".into()),
        };
        if Instant::now() >= deadline {
            return Err(if mutation {
                uncertain(wait.summary)
            } else {
                wait.summary
            });
        }
        let wake = wait.wake_at_unix_ms;
        let delay = wake
            .saturating_sub(crate::workflow::prompt_worker::now_unix_ms())
            .clamp(25, 250);
        thread::sleep(Duration::from_millis(u64::try_from(delay).unwrap_or(250)));
    }
}

fn request(command: &str) -> Result<String, String> {
    request_with_timeout(command, Duration::from_secs(30))
}

fn request_with_timeout(command: &str, timeout: Duration) -> Result<String, String> {
    let endpoint = validated_socket_path()?;
    let stream = endpoint
        .connect()
        .map_err(|error| format!("connect to Prism worker: {error}"))?;
    request_authenticated_with_timeout(&endpoint, stream, command, timeout)
}

fn request_authenticated_with_timeout(
    endpoint: &WorkerEndpoint,
    stream: WorkerStream,
    command: &str,
    timeout: Duration,
) -> Result<String, String> {
    verify_worker_stream(endpoint, &stream)?;
    let secret = worker_ipc::read_secret(endpoint)?;
    request_stream_with_timeout(stream, &authenticated_command(&secret, command), timeout)
}

fn verify_worker_stream(endpoint: &WorkerEndpoint, stream: &WorkerStream) -> Result<(), String> {
    let owner = read_worker_owner_at(&endpoint.owner_path())?
        .ok_or_else(|| "Prism worker endpoint has no ownership record".to_string())?;
    let identity = owner.process_identity.ok_or_else(|| {
        format!(
            "Prism worker {} ownership record has no reusable process identity",
            owner.pid
        )
    })?;
    let recorded = crate::process::RecordedProcess::from_stored(owner.pid, Some(identity));
    let verify_identity = || {
        let observation = crate::process::observe_process(recorded)
            .map_err(|error| format!("verify Prism worker {} identity: {error}", owner.pid))?;
        if observation != crate::process::ProcessObservation::RunningSameProcess {
            return Err(format!(
                "Prism worker {} ownership identity is not running: {observation:?}",
                owner.pid
            ));
        }
        Ok(())
    };
    verify_identity()?;
    let peer_pid = worker_ipc::peer_process_id(stream)
        .map_err(|error| format!("identify Prism worker endpoint peer: {error}"))?;
    if peer_pid != owner.pid {
        return Err(format!(
            "Prism worker endpoint peer {peer_pid} does not match owner {}",
            owner.pid
        ));
    }
    // Re-check after connecting and reading peer credentials so an owner exit
    // between the first observation and connection cannot authorize a reused PID.
    verify_identity()
}

fn authenticated_command(secret: &str, command: &str) -> String {
    format!("auth {secret} {command}")
}

fn authenticate_command<'a>(secret: &str, command: &'a str) -> Option<&'a str> {
    command.strip_prefix(&format!("auth {secret} "))
}

#[cfg(test)]
fn request_stream(stream: WorkerStream, command: &str) -> Result<String, String> {
    request_stream_with_timeout(stream, command, Duration::from_secs(30))
}

fn request_stream_with_timeout(
    stream: WorkerStream,
    command: &str,
    timeout: Duration,
) -> Result<String, String> {
    request_stream_with_limit(stream, command, timeout, MAX_RESPONSE_BYTES)
}

fn request_stream_with_limit(
    mut stream: WorkerStream,
    command: &str,
    timeout: Duration,
    response_byte_limit: usize,
) -> Result<String, String> {
    worker_ipc::set_stream_nonblocking(&stream, true)
        .map_err(|error| format!("configure Prism worker endpoint: {error}"))?;
    let request = format!("{command}\n");
    write_with_deadline(&mut stream, request.as_bytes(), timeout)
        .map_err(|error| format!("write Prism worker request: {error}"))?;
    let deadline = Instant::now() + timeout;
    let mut response = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let available = response_byte_limit + 1 - response.len();
        if available == 0 {
            return Err("read Prism worker response: response exceeds the size limit".to_string());
        }
        let read_len = available.min(buffer.len());
        match stream.read(&mut buffer[..read_len]) {
            Ok(0) => {
                #[cfg(windows)]
                wait_for_io(deadline)
                    .map_err(|error| format!("read Prism worker response: {error}"))?;
                #[cfg(unix)]
                return Err(format!(
                    "read Prism worker response: {INCOMPLETE_RESPONSE_ERROR}"
                ));
            }
            Ok(count) => {
                let newline = buffer[..count].iter().position(|byte| *byte == b'\n');
                let frame_len = newline.map_or(count, |index| index + 1);
                response.extend_from_slice(&buffer[..frame_len]);
                if response.len() > response_byte_limit {
                    return Err(
                        "read Prism worker response: response exceeds the size limit".to_string(),
                    );
                }
                if frame_len < count {
                    return Err(
                        "read Prism worker response: trailing bytes after response frame"
                            .to_string(),
                    );
                }
                if newline.is_some() {
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                wait_for_io(deadline)
                    .map_err(|error| format!("read Prism worker response: {error}"))?;
            }
            Err(error) => return Err(format!("read Prism worker response: {error}")),
        }
    }
    response.pop();
    String::from_utf8(response)
        .map(|response| response.trim().to_string())
        .map_err(|error| format!("read Prism worker response: {error}"))
}

fn parse_health(response: &str) -> Result<DaemonHealth, String> {
    let mut fields = response.split_whitespace();
    if fields.next() != Some("ok") {
        return Err(format!("invalid Prism worker response: {response}"));
    }
    let protocol_version = fields.next().and_then(|value| value.parse().ok());
    let instance_id = fields.next().map(str::to_string);
    let mut health = DaemonHealth {
        state: DaemonState::Stopped,
        protocol_version,
        instance_id,
        pid: None,
        binary_generation: None,
        active: 0,
        notifications: false,
    };
    for field in fields {
        if let Some(value) = field.strip_prefix("pid=") {
            health.pid = value.parse().ok();
        } else if let Some(value) = field.strip_prefix("generation=") {
            health.binary_generation = Some(value.into());
        } else if let Some(value) = field.strip_prefix("state=") {
            health.state = match value {
                "running" => DaemonState::Running,
                "draining" => DaemonState::Draining,
                other => return Err(format!("unknown Prism worker state: {other}")),
            };
        } else if let Some(value) = field.strip_prefix("active=") {
            health.active = value.parse().unwrap_or(0);
        } else if let Some(value) = field.strip_prefix("notifications=") {
            health.notifications = value == "1";
        }
    }
    Ok(health)
}

fn binary_generation() -> Result<String, String> {
    static GENERATION: OnceLock<Result<String, String>> = OnceLock::new();
    GENERATION
        .get_or_init(|| {
            let path = std::env::current_exe()
                .map_err(|error| format!("resolve Prism executable: {error}"))?;
            let bytes =
                fs::read(path).map_err(|error| format!("read Prism executable: {error}"))?;
            Ok(format!("{:x}", Sha256::digest(bytes)))
        })
        .clone()
}

fn acquire_lock(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    let file = options
        .open(path)
        .map_err(|error| format!("open worker lock: {error}"))?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("secure worker lock: {error}"))?;
    #[cfg(windows)]
    crate::system::windows_security::secure_path(path, false)
        .map_err(|error| format!("secure worker lock: {error}"))?;
    #[cfg(unix)]
    let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    #[cfg(windows)]
    let locked = fs4::FileExt::try_lock(&file).is_ok();
    if !locked {
        return Err("Prism worker is already running".to_string());
    }
    Ok(file)
}

pub fn runtime_dir() -> PathBuf {
    runtime_dir_for(
        crate::platform::current_os(),
        std::env::var_os("PRISM_RUNTIME_DIR")
            .filter(|path| !path.is_empty())
            .as_deref(),
        std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|path| !path.is_empty())
            .as_deref(),
        std::env::var_os("HOME")
            .filter(|path| !path.is_empty())
            .as_deref(),
        std::env::var_os("LOCALAPPDATA")
            .filter(|path| !path.is_empty())
            .as_deref(),
        &crate::util::prism_config_dir(),
    )
}

fn runtime_dir_for(
    os: SupportedOs,
    override_path: Option<&std::ffi::OsStr>,
    xdg_runtime: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
    local_app_data: Option<&std::ffi::OsStr>,
    fallback: &Path,
) -> PathBuf {
    if let Some(path) = override_path {
        return path.into();
    }
    if os == SupportedOs::Linux
        && let Some(path) = xdg_runtime
    {
        return PathBuf::from(path).join("prism");
    }
    if os == SupportedOs::MacOs
        && let Some(path) = home
    {
        return PathBuf::from(path).join("Library/Application Support/Prism/runtime");
    }
    if os == SupportedOs::Windows
        && let Some(path) = local_app_data
    {
        return PathBuf::from(path).join("Prism").join("runtime");
    }
    fallback.join("runtime")
}

pub fn socket_path() -> PathBuf {
    let runtime = runtime_dir();
    WorkerEndpoint::for_runtime(&runtime)
        .map(|endpoint| PathBuf::from(endpoint.display()))
        .unwrap_or_else(|_| runtime.join("worker.endpoint"))
}

fn validated_socket_path() -> Result<WorkerEndpoint, String> {
    WorkerEndpoint::for_runtime(&runtime_dir())
}

/// Notifications remain worker-owned, but this cutover does not require a TUI stream.
#[allow(dead_code)]
pub(crate) struct NotificationSubscription;

#[allow(dead_code)]
pub(crate) fn subscribe_notifications() -> Result<NotificationSubscription, String> {
    Ok(NotificationSubscription)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recover_control_requires_explicit_typed_evidence() {
        let missing = serde_json::json!({
            "type": "prompt_workflow_command",
            "run_id": "run-1",
            "command": "recover",
            "now_unix_ms": 1
        });
        assert!(serde_json::from_value::<SocketRequest>(missing).is_err());

        let explicit = serde_json::json!({
            "type": "prompt_workflow_command",
            "run_id": "run-1",
            "command": {"recover": {"resolution": {
                "outcome": "rejected_before_effect",
                "evidence": "authoritative provider observation"
            }}},
            "now_unix_ms": 1
        });
        assert!(matches!(
            serde_json::from_value::<SocketRequest>(explicit).unwrap(),
            SocketRequest::Command {
                command: SocketControl::Recover {
                    resolution: crate::RecoveryResolution::RejectedBeforeEffect { evidence },
                },
                ..
            } if evidence == "authoritative provider observation"
        ));
    }

    #[test]
    fn same_protocol_stale_worker_is_not_current_generation() {
        let health = DaemonHealth {
            state: DaemonState::Running,
            protocol_version: Some(PROTOCOL_VERSION),
            instance_id: Some("stale-worker".into()),
            pid: Some(42),
            binary_generation: Some("stale-generation".into()),
            active: 0,
            notifications: true,
        };

        assert!(!worker_matches_generation(&health, "current-generation"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_connection_handler_keeps_health_responsive() {
        let temporary = crate::compact_runtime::CompactTempDir::new("worker-concurrency");
        let database = temporary.path().join("workflow.db");
        let state_root = temporary.path().join("state");
        let service = crate::PromptWorkflowService::open(&database, &state_root)
            .await
            .expect("open test service");
        let runtime = temporary.path().join("runtime");
        worker_ipc::prepare_runtime(&runtime).expect("prepare worker runtime");
        let endpoint = WorkerEndpoint::for_runtime(&runtime).expect("create worker endpoint");
        endpoint
            .remove_stale_address()
            .expect("remove stale worker endpoint");
        let secret = worker_ipc::create_secret(&endpoint).expect("create worker secret");
        let listener = endpoint.bind().expect("bind worker endpoint");
        let client = endpoint.connect().expect("connect health client");
        let server = worker_ipc::accept(&listener).expect("accept health client");

        let control = Arc::new(DaemonControl::default());
        control.handlers.fetch_add(1, Ordering::AcqRel);
        let health_control = Arc::clone(&control);
        let health_service = service.clone();
        let health_secret = secret.clone();
        let health = tokio::spawn(async move {
            respond(
                &health_service,
                server,
                &health_secret,
                "test-instance",
                "test-generation",
                &health_control,
            )
            .await;
        });
        let command = authenticated_command(&secret, "health");
        let response = tokio::task::spawn_blocking(move || request_stream(client, &command))
            .await
            .expect("join health client")
            .expect("read health response");

        assert!(
            response.starts_with(&format!("ok {PROTOCOL_VERSION} test-instance")),
            "{response}"
        );
        assert_eq!(control.handlers.load(Ordering::Acquire), 1);

        control.handlers.fetch_sub(1, Ordering::AcqRel);
        health.await.expect("join health handler");
        drop(listener);
        endpoint
            .remove_stale_address()
            .expect("remove worker endpoint");
    }

    #[test]
    fn daemon_authenticates_the_first_request_to_the_verified_owner() {
        let temporary = crate::compact_runtime::CompactTempDir::new("worker-auth-first");
        let runtime = temporary.path().join("runtime");
        worker_ipc::prepare_runtime(&runtime).expect("prepare worker runtime");
        let endpoint = WorkerEndpoint::for_runtime(&runtime).expect("create worker endpoint");
        endpoint
            .remove_stale_address()
            .expect("remove stale worker endpoint");
        let secret = worker_ipc::create_secret(&endpoint).expect("create worker secret");
        let mut owner = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(endpoint.owner_path())
            .expect("open worker owner record");
        write_worker_owner(&mut owner, "test-generation").expect("write worker owner record");
        let listener = endpoint.bind().expect("bind worker endpoint");
        let client = endpoint.connect().expect("connect authenticated client");
        let server_secret = secret.clone();
        let server = thread::spawn(move || {
            let mut stream = worker_ipc::accept(&listener).expect("accept authenticated request");
            assert_eq!(
                read_request_line(&mut stream).expect("read authenticated request"),
                format!("auth {server_secret} health\n")
            );
            write_with_deadline(
                &mut stream,
                format!(
                    "ok {PROTOCOL_VERSION} test-instance pid={} generation=test state=running active=0 notifications=0\n",
                    std::process::id()
                )
                .as_bytes(),
                RESPONSE_WRITE_TIMEOUT,
            )
            .expect("write authenticated response");
        });

        let response =
            request_authenticated_with_timeout(&endpoint, client, "health", SOCKET_IO_TIMEOUT)
                .expect("complete authenticated request");

        assert!(
            response.starts_with(&format!("ok {PROTOCOL_VERSION} test-instance")),
            "{response}"
        );
        server.join().expect("join authentication server");
        endpoint
            .remove_stale_address()
            .expect("remove worker endpoint");
    }

    #[test]
    fn unowned_endpoint_is_rejected_before_any_request_bytes_are_sent() {
        let temporary = crate::compact_runtime::CompactTempDir::new("worker-unowned-endpoint");
        let runtime = temporary.path().join("runtime");
        worker_ipc::prepare_runtime(&runtime).expect("prepare worker runtime");
        let endpoint = WorkerEndpoint::for_runtime(&runtime).expect("create worker endpoint");
        endpoint
            .remove_stale_address()
            .expect("remove stale worker endpoint");
        let listener = endpoint.bind().expect("bind impersonating endpoint");
        let client = endpoint.connect().expect("connect impersonating endpoint");
        let (accepted_tx, accepted_rx) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            let mut stream = worker_ipc::accept(&listener).expect("accept client");
            accepted_tx.send(()).expect("report accepted client");
            let request = read_request_line(&mut stream);
            assert!(
                request.as_ref().map_or(true, String::is_empty),
                "unowned endpoint received request bytes: {request:?}"
            );
        });
        accepted_rx
            .recv_timeout(SOCKET_IO_TIMEOUT)
            .expect("impersonating endpoint must accept client");

        let error =
            request_authenticated_with_timeout(&endpoint, client, "health", SOCKET_IO_TIMEOUT)
                .unwrap_err();
        assert!(error.contains("no ownership record"), "{error}");
        server.join().expect("join impersonating endpoint");
        endpoint
            .remove_stale_address()
            .expect("remove worker endpoint");
    }

    #[cfg(unix)]
    #[test]
    fn response_reader_rejects_a_frame_over_the_byte_limit() {
        const TEST_RESPONSE_BYTE_LIMIT: usize = 1024;

        let (mut server, client) =
            std::os::unix::net::UnixStream::pair().expect("create response socket pair");
        let writer = thread::spawn(move || {
            let mut request = [0_u8; 16];
            let _ = server.read(&mut request).expect("read client request");
            server
                .write_all(&vec![b'x'; TEST_RESPONSE_BYTE_LIMIT + 1])
                .expect("write oversized response");
        });

        let error = request_stream_with_limit(
            client,
            "health",
            SOCKET_IO_TIMEOUT,
            TEST_RESPONSE_BYTE_LIMIT,
        )
        .unwrap_err();
        assert!(error.contains("response exceeds the size limit"), "{error}");
        writer.join().expect("join oversized response writer");
    }

    #[cfg(unix)]
    #[test]
    fn closed_incomplete_response_is_a_worker_transition() {
        let (mut server, client) =
            std::os::unix::net::UnixStream::pair().expect("create response socket pair");
        let closer = thread::spawn(move || {
            let mut request = [0_u8; 16];
            let _ = server.read(&mut request).expect("read client request");
        });

        let error = request_stream(client, "health").unwrap_err();
        assert!(
            error.contains("closed connection without a complete response"),
            "{error}"
        );
        assert!(
            is_worker_transition_error(&error),
            "closed response must be retried while the worker exits: {error}"
        );
        closer.join().expect("join response closer");
    }

    #[test]
    fn oversized_server_response_is_replaced_by_a_bounded_error() {
        let response = bounded_response("x".repeat(MAX_RESPONSE_BYTES + 1));
        assert!(response.len() < 256);
        assert!(response.contains("response exceeds the size limit"));
        assert!(response.ends_with('\n'));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn draining_worker_rejects_new_json_requests() {
        let temporary = crate::compact_runtime::CompactTempDir::new("worker-draining");
        let service = crate::PromptWorkflowService::open(
            &temporary.path().join("workflow.db"),
            &temporary.path().join("state"),
        )
        .await
        .expect("open test service");
        let control = DaemonControl::default();
        control.draining.store(true, Ordering::Release);
        let (server, mut client) =
            std::os::unix::net::UnixStream::pair().expect("create socket pair");
        let secret = "test-secret";
        let command = authenticated_command(
            secret,
            "{\"type\":\"prompt_workflow_list\",\"repository\":null,\"page_size\":1,\"cursor\":null}",
        );
        client
            .write_all(format!("{command}\n").as_bytes())
            .expect("write request");
        respond(
            &service,
            server,
            secret,
            "test-instance",
            "test-generation",
            &control,
        )
        .await;
        let mut response = String::new();
        client
            .read_to_string(&mut response)
            .expect("read draining response");
        assert!(response.contains("Prism worker is draining"), "{response}");
    }

    #[test]
    fn runtime_paths_cover_supported_platforms() {
        assert_eq!(
            runtime_dir_for(
                SupportedOs::Linux,
                None,
                Some(std::ffi::OsStr::new("/run/user/1000")),
                None,
                None,
                Path::new("/fallback"),
            ),
            Path::new("/run/user/1000/prism")
        );
        assert!(
            runtime_dir_for(
                SupportedOs::MacOs,
                None,
                None,
                Some(std::ffi::OsStr::new("/Users/test")),
                None,
                Path::new("/fallback"),
            )
            .ends_with("Library/Application Support/Prism/runtime")
        );
        assert_eq!(
            runtime_dir_for(
                SupportedOs::Windows,
                None,
                None,
                None,
                Some(std::ffi::OsStr::new(r"C:\Users\test\AppData\Local")),
                Path::new("fallback"),
            ),
            PathBuf::from(r"C:\Users\test\AppData\Local")
                .join("Prism")
                .join("runtime")
        );
    }

    #[test]
    fn worker_commands_require_the_exact_runtime_secret() {
        let command = authenticated_command("secret-one", r#"{"type":"workflow_list"}"#);
        assert_eq!(
            authenticate_command("secret-one", &command),
            Some(r#"{"type":"workflow_list"}"#)
        );
        assert_eq!(authenticate_command("secret-two", &command), None);
        assert_eq!(authenticate_command("secret-one", "health"), None);
    }

    #[cfg(windows)]
    #[test]
    fn windows_prompt_request_deadline_rejects_slow_drip_clients() {
        let directory = std::env::temp_dir().join(format!(
            "prism-prompt-slow-drip-{}-{}",
            std::process::id(),
            crate::util::timestamp_nanos()
        ));
        worker_ipc::prepare_runtime(&directory).unwrap();
        let endpoint = WorkerEndpoint::for_runtime(&directory).unwrap();
        let listener = endpoint.bind().unwrap();
        let client_endpoint = endpoint.clone();
        let client = std::thread::spawn(move || {
            let mut stream = client_endpoint.connect().unwrap();
            for byte in b"auth deliberately-slow" {
                if stream.write_all(&[*byte]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        let mut stream = worker_ipc::accept(&listener).unwrap();
        let started = Instant::now();
        assert!(read_request_line(&mut stream).is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(stream);
        drop(listener);
        client.join().unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn prompt_request_deadline_rejects_slow_drip_clients() {
        use std::io::Write as _;

        let (mut server, mut client) = std::os::unix::net::UnixStream::pair().unwrap();
        let writer = std::thread::spawn(move || {
            for byte in b"auth deliberately-slow" {
                if client.write_all(&[*byte]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        let started = Instant::now();
        assert!(read_request_line(&mut server).is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(server);
        writer.join().unwrap();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn synchronous_daemon_io_does_not_starve_the_tokio_runtime() {
        use std::io::Write as _;

        let (server, mut client) = std::os::unix::net::UnixStream::pair().unwrap();
        let reader = tokio::task::spawn_blocking(move || {
            let mut server = server;
            read_request_line(&mut server)
        });
        tokio::time::timeout(
            Duration::from_millis(100),
            tokio::time::sleep(Duration::from_millis(20)),
        )
        .await
        .expect("Tokio timer was starved by synchronous daemon I/O");
        client.write_all(b"health\n").unwrap();
        assert_eq!(reader.await.unwrap().unwrap(), "health\n");
    }

    #[cfg(unix)]
    #[test]
    fn socket_path_is_bounded_before_bind() {
        let root = PathBuf::from("x".repeat(worker_ipc::UNIX_SOCKET_PATH_BUDGET));
        assert!(WorkerEndpoint::for_runtime(&root).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn detached_worker_survives_handle_drop_and_can_be_recovered_by_identity() {
        let directory = crate::compact_runtime::CompactTempDir::new("detached-worker");
        let marker = directory.path().join("ready");
        let pid = spawn_detached_worker(
            crate::process::Command::new("sh")
                .args(["-c", "printf ready > \"$1\"; exec sleep 30", "worker"])
                .arg(&marker),
        )
        .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while !marker.exists() {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let recorded = crate::process::record_process(pid).unwrap();
        assert_eq!(
            crate::process::observe_process(recorded).unwrap(),
            crate::process::ProcessObservation::RunningSameProcess
        );
        assert_eq!(
            crate::process::terminate_recorded_process(recorded, Duration::from_millis(250))
                .await
                .unwrap(),
            crate::process::TerminationOutcome::Terminated
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn detached_worker_exit_is_reaped_by_processkit() {
        let pid = spawn_detached_worker(crate::process::Command::new("sh").args(["-c", "exit 0"]))
            .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            if matches!(
                crate::process::observe_process(crate::process::RecordedProcess::from_stored(
                    pid,
                    Some(u64::MAX)
                ))
                .unwrap(),
                crate::process::ProcessObservation::Missing
            ) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "detached child {pid} was not reaped"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let wait = unsafe { libc::waitpid(pid.cast_signed(), std::ptr::null_mut(), libc::WNOHANG) };
        assert_eq!(wait, -1, "detached child must not remain waitable by Prism");
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }
}
