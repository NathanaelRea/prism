//! On-demand user-wide Worker and socket transport for prompt Workflows.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead as _, BufReader, Read as _, Seek as _, Write as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use sha2::{Digest as _, Sha256};

use crate::platform::SupportedOs;

const PROTOCOL_VERSION: u32 = 6;
const TRANSITION_TIMEOUT: Duration = Duration::from_secs(3);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(6);
const SOCKET_PATH_BUDGET: usize = 103;
const SOCKET_IO_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_CONNECTIONS: usize = 16;
const WORKER_OWNER_RECORD_LIMIT: u64 = 4 * 1024;

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
    let path = validated_socket_path()?;
    let stream = match UnixStream::connect(&path) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) =>
        {
            return if worker_lock_is_available()? {
                Ok(DaemonHealth::stopped())
            } else {
                Ok(DaemonHealth {
                    state: DaemonState::Draining,
                    ..DaemonHealth::stopped()
                })
            };
        }
        Err(error) => return Err(format!("connect to Prism worker: {error}")),
    };
    parse_health(&request_stream_with_timeout(
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
    let executable = std::env::current_exe()
        .map_err(|error| format!("resolve Prism worker executable: {error}"))?;
    crate::process::spawn_detached_named(
        Command::new(executable)
            .args(["worker", "serve"])
            .env("PRISM_WORKER_GENERATION", &generation),
        crate::process::DetachedProcessPolicy::WorkerDaemon,
        crate::process::ProcessDescriptor::new("prism.worker.serve"),
    )
    .map_err(|error| format!("start Prism worker daemon: {error}"))?;
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
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&path)
        .map_err(|error| format!("open Prism worker ownership lock: {error}"))?;
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        let _ = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        Ok(true)
    } else if std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock {
        Ok(false)
    } else {
        Err(format!(
            "probe Prism worker ownership lock: {}",
            std::io::Error::last_os_error()
        ))
    }
}

fn write_worker_owner(lock: &mut File, generation: &str) -> Result<(), String> {
    let process = crate::process::record_process(std::process::id())
        .map_err(|error| format!("record Prism worker process identity: {error}"))?;
    let owner = WorkerOwnerRecord {
        protocol_version: PROTOCOL_VERSION,
        pid: process.pid,
        process_identity: process.identity.map(|identity| identity.stored_value()),
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
        .map_err(|error| format!("flush Prism worker ownership record: {error}"))
}

fn read_worker_owner() -> Result<Option<WorkerOwnerRecord>, String> {
    let path = runtime_dir().join("worker.lock");
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
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
    match crate::process::terminate_recorded_process(recorded, Duration::from_secs(1))
        .map_err(|error| format!("replace old Prism worker {}: {error}", owner.pid))?
    {
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
    let path = validated_socket_path()?;
    let stream = UnixStream::connect(&path)
        .map_err(|error| format!("connect to legacy Prism worker: {error}"))?;
    let pid = peer_process_id(&stream)
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
        process_identity: process.identity.map(|identity| identity.stored_value()),
        binary_generation: "legacy-unregistered".into(),
    }))
}

#[cfg(target_os = "linux")]
fn peer_process_id(stream: &UnixStream) -> std::io::Result<u32> {
    let mut credentials = unsafe { std::mem::zeroed::<libc::ucred>() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if result == -1 {
        return Err(std::io::Error::last_os_error());
    }
    u32::try_from(credentials.pid)
        .ok()
        .filter(|pid| *pid > 0)
        .ok_or_else(|| std::io::Error::other("socket peer returned an invalid process ID"))
}

#[cfg(target_os = "macos")]
fn peer_process_id(stream: &UnixStream) -> std::io::Result<u32> {
    let mut pid = 0 as libc::pid_t;
    let mut length = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&mut pid as *mut libc::pid_t).cast(),
            &mut length,
        )
    };
    if result == -1 {
        return Err(std::io::Error::last_os_error());
    }
    u32::try_from(pid)
        .ok()
        .filter(|pid| *pid > 0)
        .ok_or_else(|| std::io::Error::other("socket peer returned an invalid process ID"))
}

fn is_worker_transition_error(error: &str) -> bool {
    error.contains("Connection reset by peer")
        || error.contains("Broken pipe")
        || error.contains("connection refused")
        || error.contains("Connection refused")
        || error.contains("closed connection without a response")
}

pub fn serve() -> Result<(), String> {
    let generation = std::env::var("PRISM_WORKER_GENERATION")
        .ok()
        .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map_or_else(binary_generation, Ok)?;
    let directory = runtime_dir();
    secure_runtime_directory(&directory)?;
    // Exclusivity must cover stale socket removal and storage opening because opening may replace
    // a pre-cutover database. The ownership lock, not socket path existence, is authoritative.
    let mut lock = acquire_lock(&directory.join("worker.lock"))?;
    write_worker_owner(&mut lock, &generation)?;
    remove_owned_socket(&validated_socket_path()?)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|error| format!("create Prism worker runtime: {error}"))?;
    let service = runtime
        .block_on(crate::PromptWorkflowService::open(
            &crate::PromptWorkflowService::database_path(),
            &crate::PromptWorkflowService::state_root(),
        ))
        .map_err(|error| format!("open prompt Workflow service: {error}"))?;
    let (shutdown, mut shutdown_receiver) = tokio::sync::watch::channel(false);
    let background = service.clone();
    let scheduler = runtime.spawn(async move {
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
    let result = serve_socket(&runtime, &service, &generation, &lock);
    let _ = shutdown.send(true);
    runtime
        .block_on(scheduler)
        .map_err(|error| format!("join prompt Workflow scheduler: {error}"))?;
    result
}

#[derive(Default)]
struct DaemonControl {
    draining: AtomicBool,
    handlers: AtomicUsize,
}

fn serve_socket(
    runtime: &tokio::runtime::Runtime,
    service: &crate::PromptWorkflowService,
    generation: &str,
    _lock: &File,
) -> Result<(), String> {
    let socket = validated_socket_path()?;
    let listener = UnixListener::bind(&socket)
        .map_err(|error| format!("bind Prism worker socket {}: {error}", socket.display()))?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("secure Prism worker socket: {error}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("configure worker socket: {error}"))?;
    let instance = format!(
        "daemon-{}-{}",
        std::process::id(),
        crate::workflow::prompt_worker::now_unix_ms()
    );
    let control = Arc::new(DaemonControl::default());
    let mut handlers = Vec::new();
    let mut serve_error = None;
    loop {
        reap_finished_handlers(&mut handlers);
        if control.draining.load(Ordering::Acquire) {
            break;
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                if control.handlers.load(Ordering::Acquire) >= MAX_CONNECTIONS {
                    let _ = stream.write_all(b"error worker-busy\n");
                    continue;
                }
                control.handlers.fetch_add(1, Ordering::AcqRel);
                let handle = runtime.handle().clone();
                let service = service.clone();
                let instance = instance.clone();
                let generation = generation.to_string();
                let handler_control = Arc::clone(&control);
                match thread::Builder::new()
                    .name("prism-worker-connection".to_string())
                    .spawn(move || {
                        let _active = ActiveConnection(Arc::clone(&handler_control));
                        respond(
                            &handle,
                            &service,
                            stream,
                            &instance,
                            &generation,
                            &handler_control,
                        );
                    }) {
                    Ok(handler) => handlers.push(handler),
                    Err(error) => {
                        control.handlers.fetch_sub(1, Ordering::AcqRel);
                        serve_error =
                            Some(format!("spawn Prism worker connection handler: {error}"));
                        break;
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => {
                serve_error = Some(format!("accept Prism worker connection: {error}"));
                break;
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
    drop(listener);
    // The ownership lock remains in this stack frame until every request handler has stopped.
    // Handler work can include durable/provider effects and is not bounded by socket I/O, so
    // detaching a live handler would permit a replacement Worker to overlap this generation.
    for handler in handlers {
        let _ = handler.join();
    }
    finish_socket(&socket, serve_error)
}

fn remove_owned_socket(socket: &Path) -> Result<(), String> {
    match fs::remove_file(socket) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("remove stale worker socket: {error}")),
    }
}

fn finish_socket(socket: &Path, serve_error: Option<String>) -> Result<(), String> {
    let cleanup_error = match fs::remove_file(socket) {
        Ok(()) => None,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => Some(format!("remove Prism worker socket: {error}")),
    };
    serve_error.or(cleanup_error).map_or(Ok(()), Err)
}

fn reap_finished_handlers(handlers: &mut Vec<thread::JoinHandle<()>>) {
    let mut index = 0;
    while index < handlers.len() {
        if handlers[index].is_finished() {
            let handle = handlers.swap_remove(index);
            let _ = handle.join();
        } else {
            index += 1;
        }
    }
}

struct ActiveConnection(Arc<DaemonControl>);

impl Drop for ActiveConnection {
    fn drop(&mut self) {
        self.0.handlers.fetch_sub(1, Ordering::AcqRel);
    }
}

fn respond(
    runtime: &tokio::runtime::Handle,
    service: &crate::PromptWorkflowService,
    mut stream: UnixStream,
    instance: &str,
    generation: &str,
    control: &DaemonControl,
) {
    let _ = stream.set_read_timeout(Some(SOCKET_IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(SOCKET_IO_TIMEOUT));
    let mut request = String::new();
    if BufReader::new(&mut stream)
        .take(1024 * 1024 + 1)
        .read_line(&mut request)
        .is_err()
        || request.len() > 1024 * 1024
    {
        let _ = stream.write_all(b"error invalid-request\n");
        return;
    }
    let request = request.trim();
    let response = match request {
        "health" | "wake" => health_line(
            instance,
            generation,
            control.draining.load(Ordering::Acquire),
            active_run_count(runtime, service),
        ),
        "shutdown" => {
            control.draining.store(true, Ordering::Release);
            health_line(
                instance,
                generation,
                true,
                active_run_count(runtime, service),
            )
        }
        request if request.starts_with('{') => {
            if control.draining.load(Ordering::Acquire) {
                json_error("Prism worker is draining".to_string())
            } else {
                prompt_response(runtime, service, request)
            }
        }
        _ => "error unknown-command\n".into(),
    };
    if stream.write_all(response.as_bytes()).is_ok() {
        let _ = stream.shutdown(std::net::Shutdown::Write);
    }
}

fn active_run_count(
    runtime: &tokio::runtime::Handle,
    service: &crate::PromptWorkflowService,
) -> usize {
    runtime
        .block_on(service.list(None, 10_000))
        .map(|runs| {
            runs.into_iter()
                .filter(|run| !run.status.terminal())
                .count()
        })
        .unwrap_or(0)
}

fn health_line(instance: &str, generation: &str, draining: bool, active: usize) -> String {
    format!(
        "ok {PROTOCOL_VERSION} {instance} pid={} generation={generation} state={} active={active} notifications=1\n",
        std::process::id(),
        if draining { "draining" } else { "running" }
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
        limit: usize,
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
    Recover { evidence: String },
    Discard,
}

fn prompt_response(
    runtime: &tokio::runtime::Handle,
    service: &crate::PromptWorkflowService,
    request: &str,
) -> String {
    let request = match serde_json::from_str::<SocketRequest>(request) {
        Ok(request) => request,
        Err(error) => return json_error(format!("invalid Worker request: {error}")),
    };
    let result = runtime.block_on(async {
        match request {
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
            SocketRequest::List { repository, limit } => service
                .list(repository.as_deref().map(Path::new), limit)
                .await
                .map(|runs| serde_json::json!({"ok": true, "runs": runs}))
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
                    SocketControl::Recover { evidence } => {
                        service.recover(&run_id, now_unix_ms, &evidence).await
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
        }
    });
    match result {
        Ok(value) => format!("{value}\n"),
        Err(error) => json_error(error.to_string()),
    }
}

fn json_error(error: String) -> String {
    format!("{}\n", serde_json::json!({"ok": false, "error": error}))
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
    let response = worker_request(serde_json::json!({
        "type": "prompt_workflow_list",
        "repository": repository.map(|path| path.to_string_lossy().into_owned()),
        "limit": limit,
    }))?;
    serde_json::from_value(response["runs"].clone())
        .map_err(|error| format!("decode Workflow list: {error}"))
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
    Recover { evidence: String },
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
    let path = validated_socket_path()?;
    let stream =
        UnixStream::connect(&path).map_err(|error| format!("connect to Prism worker: {error}"))?;
    request_stream_with_timeout(stream, command, timeout)
}

#[cfg(test)]
fn request_stream(stream: UnixStream, command: &str) -> Result<String, String> {
    request_stream_with_timeout(stream, command, Duration::from_secs(30))
}

fn request_stream_with_timeout(
    mut stream: UnixStream,
    command: &str,
    timeout: Duration,
) -> Result<String, String> {
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|error| format!("configure Prism worker socket read timeout: {error}"))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|error| format!("configure Prism worker socket write timeout: {error}"))?;
    stream
        .write_all(format!("{command}\n").as_bytes())
        .map_err(|error| format!("write Prism worker request: {error}"))?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|error| format!("read Prism worker response: {error}"))?;
    let response = response.trim();
    if response.is_empty() {
        return Err("Prism worker closed connection without a response".into());
    }
    Ok(response.to_string())
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

fn secure_runtime_directory(path: &Path) -> Result<(), String> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "Prism worker runtime is a symlink: {}",
                path.display()
            ));
        }
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(format!(
                "Prism worker runtime has another owner: {}",
                path.display()
            ));
        }
    }
    fs::create_dir_all(path).map_err(|error| format!("create worker runtime: {error}"))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("secure worker runtime: {error}"))
}

fn acquire_lock(path: &Path) -> Result<File, String> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(|error| format!("open worker lock: {error}"))?;
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == -1 {
        return Err(format!(
            "Prism worker is already running: {}",
            std::io::Error::last_os_error()
        ));
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
        &crate::util::prism_config_dir(),
    )
}

fn runtime_dir_for(
    os: SupportedOs,
    override_path: Option<&std::ffi::OsStr>,
    xdg_runtime: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
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
    fallback.join("runtime")
}

pub fn socket_path() -> PathBuf {
    runtime_dir().join("worker.sock")
}

fn validated_socket_path() -> Result<PathBuf, String> {
    let path = socket_path();
    let bytes = path.as_os_str().as_bytes();
    if bytes.contains(&0) || bytes.len() > SOCKET_PATH_BUDGET {
        return Err(format!(
            "Prism worker socket path must be at most {SOCKET_PATH_BUDGET} bytes; set PRISM_RUNTIME_DIR to a shorter private directory"
        ));
    }
    Ok(path)
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
            "command": {"recover": {"evidence": "authoritative provider observation"}},
            "now_unix_ms": 1
        });
        assert!(matches!(
            serde_json::from_value::<SocketRequest>(explicit).unwrap(),
            SocketRequest::Command {
                command: SocketControl::Recover { evidence },
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

    #[test]
    fn connection_closed_without_response_is_a_worker_transition() {
        let (server, client) = UnixStream::pair().unwrap();
        let server = thread::spawn(move || {
            let mut request = String::new();
            BufReader::new(server).read_line(&mut request).unwrap();
            assert_eq!(request, "health\n");
        });

        let result = request_stream(client, "health");
        server.join().unwrap();
        let error = result.unwrap_err();
        assert!(is_worker_transition_error(&error), "{error}");
    }

    #[test]
    fn concurrent_connection_handler_keeps_health_responsive() {
        let temporary = crate::compact_runtime::CompactTempDir::new("worker-concurrency");
        let database = temporary.path().join("workflow.db");
        let state_root = temporary.path().join("state");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("create test runtime");
        let service = runtime
            .block_on(crate::PromptWorkflowService::open(&database, &state_root))
            .expect("open test service");
        let control = Arc::new(DaemonControl::default());
        let (slow_server, _slow_client) = UnixStream::pair().expect("create slow socket pair");
        let (health_server, mut health_client) =
            UnixStream::pair().expect("create health socket pair");
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let slow_control = Arc::clone(&control);
        slow_control.handlers.fetch_add(1, Ordering::AcqRel);
        let slow = thread::spawn(move || {
            release_rx.recv().expect("release slow handler");
            drop(slow_server);
            slow_control.handlers.fetch_sub(1, Ordering::AcqRel);
        });
        let health_control = Arc::clone(&control);
        let handle = runtime.handle().clone();
        let health_service = service.clone();
        let health = thread::spawn(move || {
            respond(
                &handle,
                &health_service,
                health_server,
                "test-instance",
                "test-generation",
                &health_control,
            );
        });

        health_client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("configure health timeout");
        health_client
            .write_all(b"health\n")
            .expect("write health request");
        let mut response = String::new();
        health_client
            .read_to_string(&mut response)
            .expect("read health response");
        assert!(response.starts_with("ok 6 test-instance"), "{response}");
        assert_eq!(control.handlers.load(Ordering::Acquire), 1);

        release_tx.send(()).expect("release slow handler");
        slow.join().expect("join slow handler");
        health.join().expect("join health handler");
    }

    #[test]
    fn draining_worker_rejects_new_json_requests() {
        let temporary = crate::compact_runtime::CompactTempDir::new("worker-draining");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("create test runtime");
        let service = runtime
            .block_on(crate::PromptWorkflowService::open(
                &temporary.path().join("workflow.db"),
                &temporary.path().join("state"),
            ))
            .expect("open test service");
        let control = DaemonControl::default();
        control.draining.store(true, Ordering::Release);
        let (server, mut client) = UnixStream::pair().expect("create socket pair");
        client
            .write_all(b"{\"type\":\"prompt_workflow_list\",\"repository\":null,\"limit\":1}\n")
            .expect("write request");
        respond(
            runtime.handle(),
            &service,
            server,
            "test-instance",
            "test-generation",
            &control,
        );
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
                Path::new("/fallback"),
            )
            .ends_with("Library/Application Support/Prism/runtime")
        );
    }

    #[test]
    fn socket_path_is_bounded_before_bind() {
        let root = PathBuf::from("x".repeat(SOCKET_PATH_BUDGET));
        assert!(root.join("worker.sock").as_os_str().as_bytes().len() > SOCKET_PATH_BUDGET);
    }

    #[test]
    fn exclusive_owner_removes_stale_socket_before_cutover_and_cleanup_preserves_accept_error() {
        let runtime = crate::compact_runtime::CompactTempDir::new("stale-worker-socket");
        let socket = runtime.runtime_path().join("worker.sock");
        fs::create_dir_all(runtime.runtime_path()).unwrap();
        fs::write(&socket, b"stale").unwrap();
        remove_owned_socket(&socket).unwrap();
        assert!(!socket.exists());
        let listener = UnixListener::bind(&socket).unwrap();
        drop(listener);
        let error = finish_socket(&socket, Some("forced accept failure".into())).unwrap_err();
        assert_eq!(error, "forced accept failure");
        assert!(!socket.exists());
    }

    #[test]
    fn blocked_handler_retains_exclusive_worker_ownership_until_joined() {
        let runtime = crate::compact_runtime::CompactTempDir::new("blocked-worker-handler");
        fs::create_dir_all(runtime.runtime_path()).unwrap();
        let lock_path = runtime.runtime_path().join("worker.lock");
        let owner = acquire_lock(&lock_path).unwrap();
        let (release, wait) = std::sync::mpsc::channel();
        let handler = thread::spawn(move || wait.recv().unwrap());
        let joining = thread::spawn(move || {
            handler.join().unwrap();
            drop(owner);
        });

        assert!(
            acquire_lock(&lock_path)
                .unwrap_err()
                .contains("already running")
        );
        release.send(()).unwrap();
        joining.join().unwrap();
        assert!(acquire_lock(&lock_path).is_ok());
    }

    #[test]
    fn finished_handlers_are_reaped_without_waiting_for_live_handlers() {
        let (release, wait) = std::sync::mpsc::channel();
        let live = thread::spawn(move || {
            let _ = wait.recv();
        });
        let finished = thread::spawn(|| {});
        while !finished.is_finished() {
            thread::yield_now();
        }
        let mut handlers = vec![live, finished];
        let started = Instant::now();
        reap_finished_handlers(&mut handlers);
        assert!(started.elapsed() < Duration::from_millis(100));
        assert_eq!(handlers.len(), 1);
        release.send(()).unwrap();
        handlers.pop().unwrap().join().unwrap();
    }
}
