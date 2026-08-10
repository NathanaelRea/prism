use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::notification::{NotificationCoordinator, NotificationObservation, PendingNotification};
use crate::platform::SupportedOs;
use crate::process::DetachedProcessPolicy;
use crate::repo::Repository;
use crate::{observability, workspace};
use sha2::{Digest, Sha256};

// Version 3 makes Workflow launch a worker-owned operation and adds executable generation
// identity. Older workers must be drained before they can observe the current ledger schema.
const PROTOCOL_VERSION: u32 = 3;
const NOTIFICATION_POLL_INTERVAL: Duration = Duration::from_secs(5);
const NOTIFICATION_RETRY_INTERVAL: Duration = Duration::from_secs(10);
const DAEMON_TRANSITION_TIMEOUT: Duration = Duration::from_secs(3);
const SOCKET_PATH_BUDGET: usize = 103;

#[derive(Clone, Debug, PartialEq, Eq)]
struct WorkerSocketPath(PathBuf);

impl WorkerSocketPath {
    fn for_runtime(runtime: &Path) -> Result<Self, String> {
        let path = runtime.join("worker.sock");
        let bytes = socket_path_bytes(&path);
        if bytes.contains(&0) {
            return Err(format!(
                "Prism worker runtime directory {} produces a socket path containing a NUL byte; set PRISM_RUNTIME_DIR to a shorter valid private directory",
                runtime.display()
            ));
        }
        if bytes.len() > SOCKET_PATH_BUDGET {
            return Err(format!(
                "Prism worker runtime directory {} produces a {}-byte socket path, exceeding the supported maximum of {SOCKET_PATH_BUDGET} bytes; set PRISM_RUNTIME_DIR to a shorter private directory such as /tmp/prism-$UID",
                runtime.display(),
                bytes.len()
            ));
        }
        Ok(Self(path))
    }

    fn as_path(&self) -> &Path {
        &self.0
    }
}

fn socket_path_bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
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
    probe_health_at(&validated_socket_path()?)
}

fn probe_health_at(path: &WorkerSocketPath) -> Result<DaemonHealth, String> {
    let stream = match UnixStream::connect(path.as_path()) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) =>
        {
            return Ok(DaemonHealth::stopped());
        }
        Err(error) => return Err(format!("connect to Prism worker: {error}")),
    };
    let response = match request_on_stream_raw(stream, "health") {
        Ok(response) => response,
        Err((_, error))
            if matches!(
                error.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::NotConnected
            ) =>
        {
            return Ok(DaemonHealth::stopped());
        }
        Err(error) => return Err(format_request_error(error)),
    };
    parse_health_response(&response)
}

fn parse_health_response(response: &str) -> Result<DaemonHealth, String> {
    let mut fields = response.split_whitespace();
    if fields.next() != Some("ok") {
        return Err(format!("invalid Prism daemon response: {response}"));
    }
    let version = fields
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(|| format!("invalid Prism daemon protocol: {response}"))?;
    let instance_id = fields
        .next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("missing Prism daemon instance ID: {response}"))?;
    let mut pid: Option<u32> = None;
    let mut state = None;
    let mut binary_generation = None;
    let mut active = None;
    let mut notifications = false;
    for field in fields {
        if let Some(value) = field.strip_prefix("pid=") {
            pid = value.parse().ok();
        } else if let Some(value) = field.strip_prefix("generation=") {
            binary_generation = Some(value.to_string());
        } else if let Some(value) = field.strip_prefix("state=") {
            state = Some(match value {
                "running" => DaemonState::Running,
                "draining" => DaemonState::Draining,
                _ => return Err(format!("unknown Prism daemon state: {value}")),
            });
        } else if let Some(value) = field.strip_prefix("active=") {
            active = value.parse().ok();
        } else if let Some(value) = field.strip_prefix("notifications=") {
            notifications = value == "1";
        }
    }
    Ok(DaemonHealth {
        state: state.ok_or_else(|| format!("missing Prism daemon state: {response}"))?,
        protocol_version: Some(version),
        instance_id: Some(instance_id.to_string()),
        pid: Some(pid.ok_or_else(|| format!("missing Prism daemon PID: {response}"))?),
        binary_generation,
        active: active.ok_or_else(|| format!("missing Prism daemon active count: {response}"))?,
        notifications,
    })
}

fn binary_generation() -> Result<String, String> {
    static GENERATION: OnceLock<Result<String, String>> = OnceLock::new();
    GENERATION
        .get_or_init(|| {
            let executable = std::env::current_exe()
                .map_err(|error| format!("resolve Prism executable generation: {error}"))?;
            let mut executable = File::open(&executable)
                .map_err(|error| format!("open Prism executable generation: {error}"))?;
            let mut digest = Sha256::new();
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let read = executable
                    .read(&mut buffer)
                    .map_err(|error| format!("read Prism executable generation: {error}"))?;
                if read == 0 {
                    break;
                }
                digest.update(&buffer[..read]);
            }
            Ok(format!("{:x}", digest.finalize()))
        })
        .clone()
}

fn daemon_is_current(health: &DaemonHealth, generation: &str) -> bool {
    health.protocol_version == Some(PROTOCOL_VERSION)
        && health.binary_generation.as_deref() == Some(generation)
        && health.notifications
}

pub fn ensure_running() -> Result<(), String> {
    let socket = validated_socket_path()?;
    let generation = binary_generation()?;
    if std::env::var_os("PRISM_WAIT_FOR_WORKER_DRAIN").is_some() {
        loop {
            match probe_health_at(&socket)? {
                DaemonHealth {
                    state: DaemonState::Stopped,
                    ..
                } => break,
                health
                    if health.state == DaemonState::Running
                        && daemon_is_current(&health, &generation) =>
                {
                    return Ok(());
                }
                _ => thread::sleep(Duration::from_millis(250)),
            }
        }
    }
    if wait_for_existing_daemon(DAEMON_TRANSITION_TIMEOUT, || probe_health_at(&socket))? {
        let health = probe_health_at(&socket)?;
        if daemon_is_current(&health, &generation) {
            return Ok(());
        }
        let shutdown_health = parse_health_response(&request_at(&socket, "shutdown")?)?;
        if shutdown_health.active > 0 {
            spawn_worker_replacement()?;
            return Err(format!(
                "Prism worker replacement is waiting for {} active Attempt(s) to drain; retry when daemon replacement completes",
                shutdown_health.active
            ));
        }
        wait_for_socket_to_close(&socket, DAEMON_TRANSITION_TIMEOUT)?;
    }
    let executable = std::env::current_exe()
        .map_err(|error| format!("resolve Prism worker executable: {error}"))?;
    let mut command = Command::new(executable);
    command
        .args(["worker", "serve"])
        .env("PRISM_WORKER_GENERATION", &generation);
    crate::process::spawn_detached_named(
        &mut command,
        DetachedProcessPolicy::WorkerDaemon,
        crate::process::ProcessDescriptor::new("prism.worker.serve"),
    )
    .map_err(|error| format!("start Prism worker daemon: {error}"))?;

    let deadline = Instant::now() + DAEMON_TRANSITION_TIMEOUT;
    let mut last_error = "worker did not become ready".to_string();
    while Instant::now() < deadline {
        match probe_health_at(&socket) {
            Ok(health)
                if health.state == DaemonState::Running
                    && daemon_is_current(&health, &generation) =>
            {
                return Ok(());
            }
            Ok(health) => {
                last_error = format!(
                    "worker did not become ready: state={:?}, protocol={:?}, generation={:?}",
                    health.state, health.protocol_version, health.binary_generation
                )
            }
            Err(error) => last_error = error,
        }
        thread::sleep(Duration::from_millis(25));
    }
    Err(last_error)
}

fn spawn_worker_replacement() -> Result<(), String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("resolve replacement Prism worker executable: {error}"))?;
    let mut command = Command::new(executable);
    command
        .args(["worker", "ensure"])
        .env("PRISM_WAIT_FOR_WORKER_DRAIN", "1");
    crate::process::spawn_detached_named(
        &mut command,
        DetachedProcessPolicy::WorkerDaemon,
        crate::process::ProcessDescriptor::new("prism.worker.replace"),
    )
    .map(|_| ())
    .map_err(|error| format!("schedule replacement Prism worker: {error}"))
}

fn wait_for_existing_daemon(
    timeout: Duration,
    mut probe: impl FnMut() -> Result<DaemonHealth, String>,
) -> Result<bool, String> {
    let deadline = Instant::now() + timeout;
    loop {
        match probe() {
            Ok(DaemonHealth {
                state: DaemonState::Running,
                ..
            }) => return Ok(true),
            Ok(DaemonHealth {
                state: DaemonState::Draining,
                active,
                ..
            }) => {
                if Instant::now() >= deadline {
                    return Err(format!(
                        "timed out waiting for Prism worker daemon to finish draining ({active} active)"
                    ));
                }
                thread::sleep(Duration::from_millis(25));
            }
            Ok(DaemonHealth {
                state: DaemonState::Stopped,
                ..
            }) => return Ok(false),
            Err(error) => return Err(error),
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) struct NotificationSubscription {
    stop: Arc<AtomicBool>,
    listener: Option<thread::JoinHandle<()>>,
}

#[cfg(target_os = "macos")]
pub(crate) fn subscribe_notifications() -> Result<NotificationSubscription, String> {
    let socket = validated_socket_path()?;
    let stop = Arc::new(AtomicBool::new(false));
    let listener_stop = Arc::clone(&stop);
    let listener = thread::Builder::new()
        .name("prism-notification-subscription".to_string())
        .spawn(move || notification_subscription_loop(&socket, &listener_stop))
        .map_err(|error| format!("start notification subscription: {error}"))?;
    Ok(NotificationSubscription {
        stop,
        listener: Some(listener),
    })
}

#[cfg(target_os = "macos")]
impl Drop for NotificationSubscription {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let Some(listener) = self.listener.take() else {
            return;
        };
        let deadline = Instant::now() + Duration::from_secs(1);
        while !listener.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        if listener.is_finished() {
            let _ = listener.join();
        }
    }
}

#[cfg(target_os = "macos")]
fn notification_subscription_loop(socket: &WorkerSocketPath, stop: &AtomicBool) {
    notification_subscription_loop_with_delivery(socket, stop, |title, body| {
        crate::desktop_notification::deliver_terminal_notification(title, body)
    });
}

#[cfg(any(target_os = "macos", test))]
fn notification_subscription_loop_with_delivery(
    socket: &WorkerSocketPath,
    stop: &AtomicBool,
    mut deliver: impl FnMut(&str, &str) -> Result<(), &'static str>,
) {
    while !stop.load(Ordering::Acquire) {
        if let Ok(mut stream) = UnixStream::connect(socket.as_path()) {
            let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
            if stream.write_all(b"subscribe-notifications\n").is_ok() {
                let mut reader = BufReader::new(stream);
                loop {
                    if stop.load(Ordering::Acquire) {
                        return;
                    }
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        Ok(0) => break,
                        Ok(_) if line.starts_with("ok ") => continue,
                        Ok(_) if line.starts_with("error ") => break,
                        Ok(_) => {
                            let Ok(message) = serde_json::from_str::<serde_json::Value>(&line)
                            else {
                                continue;
                            };
                            let (Some(id), Some(title), Some(body)) = (
                                message.get("id").and_then(serde_json::Value::as_i64),
                                message.get("title").and_then(serde_json::Value::as_str),
                                message.get("body").and_then(serde_json::Value::as_str),
                            ) else {
                                continue;
                            };
                            let acknowledgement = match deliver(title, body) {
                                Ok(()) => format!("accepted {id}\n"),
                                Err(category) => format!("failed {id} {category}\n"),
                            };
                            if reader
                                .get_mut()
                                .write_all(acknowledgement.as_bytes())
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::WouldBlock
                                    | std::io::ErrorKind::TimedOut
                                    | std::io::ErrorKind::Interrupted
                            ) => {}
                        Err(_) => break,
                    }
                }
            }
        }
        for _ in 0..5 {
            if stop.load(Ordering::Acquire) {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

pub fn health_response() -> Result<String, String> {
    request("health")
}

pub fn shutdown() -> Result<(), String> {
    let socket = validated_socket_path()?;
    let response = request_at(&socket, "shutdown")?;
    let health = parse_health_response(&response)
        .map_err(|_| format!("Prism worker rejected shutdown: {response}"))?;
    if health.state != DaemonState::Draining {
        return Err(format!("Prism worker rejected shutdown: {response}"));
    }
    wait_for_socket_to_close(&socket, DAEMON_TRANSITION_TIMEOUT)
}

fn wait_for_socket_to_close(path: &WorkerSocketPath, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        match fs::symlink_metadata(path.as_path()) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(format!("inspect Prism worker socket: {error}")),
            Ok(_) => {}
        }
        if Instant::now() >= deadline {
            return Err("timed out waiting for Prism worker daemon to stop".to_string());
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn request(command: &str) -> Result<String, String> {
    request_at(&validated_socket_path()?, command)
}

fn workflow_request(request_value: serde_json::Value) -> Result<serde_json::Value, String> {
    let response = request(&request_value.to_string())?;
    let response: serde_json::Value = serde_json::from_str(&response)
        .map_err(|error| format!("decode workflow worker response: {error}"))?;
    if response["ok"] == true {
        Ok(response)
    } else {
        Err(response["error"]
            .as_str()
            .unwrap_or("workflow operation failed")
            .to_string())
    }
}

/// Read the authoritative global run ledger through the worker projection API.
pub fn list_workflows(
    repository: Option<&Path>,
    limit: usize,
) -> Result<Vec<crate::WorkflowProjection>, String> {
    let repository = repository.map(|path| path.display().to_string());
    let response = workflow_request(serde_json::json!({
        "type": "workflow_list",
        "repository": repository,
        "limit": limit,
    }))?;
    serde_json::from_value(response["runs"].clone())
        .map_err(|error| format!("decode workflow list projection: {error}"))
}

pub fn launch_workflow(
    catalog: &crate::workflow::definition::DefinitionCatalog,
    run: crate::LaunchWorkflow<'_>,
) -> Result<String, String> {
    ensure_running()?;
    for definition in catalog.list() {
        let snapshot = catalog
            .compile(&definition.id)
            .map_err(|error| error.to_string())?;
        let body = serde_json::to_string(&snapshot)
            .map_err(|error| format!("serialize Workflow snapshot: {error}"))?;
        workflow_request(serde_json::json!({
            "type": "workflow_register_definition",
            "definition": {
                "id": snapshot.digest,
                "name": snapshot.definition.name,
                "revision": definition.revision,
                "source": definition.path,
                "trusted": snapshot.trusted,
                "body_json": body,
                "digest": snapshot.digest,
                "now_unix_ms": run.now_unix_ms,
            }
        }))?;
    }
    let response = workflow_request(serde_json::json!({
        "type": "workflow_launch",
        "run": {
            "run_id": run.run_id,
            "definition_snapshot_id": run.definition_snapshot_id,
            "repository": run.repository,
            "idempotency_key": run.idempotency_key,
            "input_json": run.input_json,
            "now_unix_ms": run.now_unix_ms,
        }
    }))?;
    let launched = response["run_id"]
        .as_str()
        .ok_or_else(|| "Workflow worker launch response omitted the Run ID".to_string())?
        .to_string();
    confirm_queued_worker(launched, request("wake"))
}

fn confirm_queued_worker(run_id: String, wake: Result<String, String>) -> Result<String, String> {
    match wake {
        Ok(_) => Ok(run_id),
        Err(error) => Err(format!(
            "Workflow Run {run_id} is durably queued, but the Prism worker became unavailable: {error}"
        )),
    }
}

pub fn inspect_workflows(run_ids: &[String]) -> Result<Vec<crate::WorkflowProjection>, String> {
    if run_ids.is_empty() {
        return Ok(Vec::new());
    }
    match workflow_request(serde_json::json!({
        "type": "workflow_inspect_many",
        "run_ids": run_ids,
    })) {
        Ok(response) => serde_json::from_value(response["runs"].clone())
            .map_err(|error| format!("decode workflow inspection projections: {error}")),
        // Protocol 2 workers from an earlier build already support singular inspection. Keep a
        // rolling-upgrade fallback so a running worker need not be interrupted for this read path.
        Err(_) => run_ids
            .iter()
            .filter_map(|run_id| match inspect_workflow(run_id) {
                Ok(Some(run)) => Some(Ok(run)),
                Ok(None) => None,
                Err(error) => Some(Err(error)),
            })
            .collect(),
    }
}

fn inspect_workflow(run_id: &str) -> Result<Option<crate::WorkflowProjection>, String> {
    let response = workflow_request(serde_json::json!({
        "type": "workflow_inspect",
        "run_id": run_id,
    }))?;
    serde_json::from_value(response["run"].clone())
        .map_err(|error| format!("decode workflow inspection projection: {error}"))
}

pub fn command_workflow(run_id: &str, command: crate::WorkflowCommand) -> Result<(), String> {
    ensure_running()?;
    workflow_request(serde_json::json!({
        "type": "workflow_command",
        "run_id": run_id,
        "command": command,
        "now_unix_ms": current_unix_ms(),
    }))?;
    Ok(())
}

fn request_at(path: &WorkerSocketPath, command: &str) -> Result<String, String> {
    let stream = UnixStream::connect(path.as_path())
        .map_err(|error| format!("connect to Prism worker: {error}"))?;
    request_on_stream(stream, command)
}

fn request_on_stream(stream: UnixStream, command: &str) -> Result<String, String> {
    request_on_stream_raw(stream, command).map_err(format_request_error)
}

fn request_on_stream_raw(
    mut stream: UnixStream,
    command: &str,
) -> Result<String, (&'static str, std::io::Error)> {
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .map_err(|error| ("configure Prism worker socket", error))?;
    stream
        .write_all(format!("{command}\n").as_bytes())
        .map_err(|error| ("write Prism worker request", error))?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|error| ("read Prism worker response", error))?;
    Ok(response.trim().to_string())
}

fn format_request_error((action, error): (&str, std::io::Error)) -> String {
    format!("{action}: {error}")
}

pub fn serve() -> Result<(), String> {
    // The launcher captures this before spawning so an atomic installation cannot make an old
    // process report the replacement file's digest during the narrow exec/startup window.
    let generation = std::env::var("PRISM_WORKER_GENERATION")
        .ok()
        .filter(|generation| {
            generation.len() == 64 && generation.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
        .map_or_else(binary_generation, Ok)?;
    // One long-lived runtime supervises the generalized async control plane. The blocking Unix
    // socket adapter only translates requests; it does not run a second scheduling engine.
    let async_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_io()
        .enable_time()
        .build()
        .map_err(|error| format!("create Prism worker runtime: {error}"))?;
    async_runtime.block_on(async {
        let mut worker = crate::workflow::engine::WorkflowWorker::open_default(
            new_instance_id("workflow-worker"),
            crate::workflow::engine::WorkerConfig::default(),
        )
        .await
        .map_err(|error| format!("open workflow control plane: {error}"))?;
        worker
            .register_builtins()
            .map_err(|error| format!("register workflow implementations: {error}"))?;
        worker
            .register_standard_reconcilers()
            .map_err(|error| format!("register Standard effect reconcilers: {error}"))?;
        let standard_extension = crate::package::locate_standard_extension()
            .map_err(|error| format!("locate Standard Extension: {error}"))?;
        let standard_dispatcher = worker.standard_production_dispatcher();
        // The Standard Extension may invoke provider CLIs that share credential/config files.
        // Serialize one executable revision until those adapters provide a concurrency-safe
        // transport; the generic Worker still schedules unrelated implementations concurrently.
        let standard_limits = crate::extension::HostLimits {
            max_concurrent_calls_per_revision: 1,
            ..crate::extension::HostLimits::default()
        };
        worker
            .register_extension(standard_extension, standard_dispatcher, standard_limits)
            .await
            .map_err(|error| format!("register Standard Extension: {error}"))?;
        let operations = worker.operations();
        let (shutdown, shutdown_receiver) = tokio::sync::watch::channel(false);
        let control_plane_failure = Arc::new(Mutex::new(None::<String>));
        let failure = Arc::clone(&control_plane_failure);
        let control_plane = tokio::spawn(async move {
            if let Err(error) = worker.run(shutdown_receiver).await
                && let Ok(mut current) = failure.lock()
            {
                *current = Some(error.to_string());
            }
        });
        // Socket polling is blocking, so isolate the protocol adapter from runtime worker threads.
        let socket_failure = Arc::clone(&control_plane_failure);
        let socket = tokio::task::spawn_blocking(move || {
            serve_socket(&socket_failure, &operations, &generation)
        });
        let socket_result = socket
            .await
            .map_err(|error| format!("join workflow socket adapter: {error}"))?;
        let _ = shutdown.send(true);
        control_plane
            .await
            .map_err(|error| format!("join workflow control plane: {error}"))?;
        socket_result
    })
}

fn serve_socket(
    control_plane_failure: &Arc<Mutex<Option<String>>>,
    operations: &crate::WorkflowOperations,
    generation: &str,
) -> Result<(), String> {
    let runtime = runtime_dir();
    let socket = WorkerSocketPath::for_runtime(&runtime)?;
    if let Ok(metadata) = fs::symlink_metadata(&runtime) {
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "Prism worker runtime directory is a symlink: {}",
                runtime.display()
            ));
        }
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(format!(
                "Prism worker runtime directory is owned by another user: {}",
                runtime.display()
            ));
        }
    }
    fs::create_dir_all(&runtime).map_err(|error| format!("create worker runtime dir: {error}"))?;
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("secure worker runtime dir: {error}"))?;
    let _lock = acquire_lock(&runtime.join("worker.lock"))?;
    if socket.as_path().exists() {
        match UnixStream::connect(socket.as_path()) {
            Ok(_) => {
                return Err(
                    "a live Prism worker endpoint already owns the runtime socket".to_string(),
                );
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                ) => {}
            Err(error) => {
                return Err(format!(
                    "cannot safely classify existing Prism worker socket: {error}"
                ));
            }
        }
        fs::remove_file(socket.as_path())
            .map_err(|error| format!("remove stale worker socket: {error}"))?;
    }

    let instance_id = new_instance_id("daemon");
    log_daemon_lifecycle("daemon_start", &instance_id);
    let listener = UnixListener::bind(socket.as_path()).map_err(|error| {
        format!(
            "bind Prism worker socket {}: {error}",
            socket.as_path().display()
        )
    })?;
    fs::set_permissions(socket.as_path(), fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("secure Prism worker socket: {error}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("configure Prism worker listener: {error}"))?;

    let notification_subscriber = Arc::new(Mutex::new(Vec::<UnixStream>::new()));
    let notification_stop = Arc::new(AtomicBool::new(false));
    let observer_stop = Arc::clone(&notification_stop);
    let observer_subscriber = Arc::clone(&notification_subscriber);
    thread::Builder::new()
        .name("prism-notification-observer".to_string())
        .spawn(move || notification_loop(observer_stop, observer_subscriber))
        .map_err(|error| format!("start notification observer: {error}"))?;
    let mut draining = false;
    loop {
        if let Some(error) = control_plane_failure
            .lock()
            .map_err(|_| "workflow control-plane supervisor state is poisoned".to_string())?
            .clone()
        {
            notification_stop.store(true, Ordering::Release);
            return Err(format!("workflow control plane failed: {error}"));
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                if respond(
                    &mut stream,
                    &instance_id,
                    generation,
                    &notification_subscriber,
                    Some(operations),
                    draining,
                ) {
                    draining = true;
                    notification_stop.store(true, Ordering::Release);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(format!("accept Prism worker connection: {error}")),
        }
        if draining && operations.active_attempt_count() == 0 {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    notification_stop.store(true, Ordering::Release);
    log_daemon_lifecycle("daemon_stop", &instance_id);
    fs::remove_file(socket.as_path()).map_err(|error| format!("remove worker socket: {error}"))
}

fn respond(
    stream: &mut UnixStream,
    instance_id: &str,
    generation: &str,
    notification_subscriber: &Arc<Mutex<Vec<UnixStream>>>,
    operations: Option<&crate::WorkflowOperations>,
    draining: bool,
) -> bool {
    const MAX_REQUEST_BYTES: usize = 1024 * 1024;
    let mut command = String::new();
    let read = BufReader::new(&mut *stream)
        .take((MAX_REQUEST_BYTES + 1) as u64)
        .read_line(&mut command);
    if read.is_err() || command.len() > MAX_REQUEST_BYTES {
        let _ = stream.write_all(b"error invalid-request\n");
        return false;
    }
    let command = command.trim();
    let active = operations.map_or(0, crate::WorkflowOperations::active_attempt_count);
    let mut new_notification_subscriber = None;
    let response = if command.starts_with('{') {
        operations.map_or_else(
            || {
                format!(
                    "{}\n",
                    serde_json::json!({"ok": false, "error": "workflow operations unavailable"})
                )
            },
            |operations| workflow_socket_response(operations, command),
        )
    } else {
        match command {
            "health" | "wake" => format!(
                "ok {PROTOCOL_VERSION} {instance_id} pid={} generation={generation} state={} active={active} notifications=1\n",
                std::process::id(),
                if draining { "draining" } else { "running" }
            ),
            "shutdown" => {
                if let Some(operations) = operations {
                    operations.begin_draining();
                }
                format!(
                    "ok {PROTOCOL_VERSION} {instance_id} pid={} generation={generation} state=draining active={} notifications=1\n",
                    std::process::id(),
                    operations.map_or(0, crate::WorkflowOperations::active_attempt_count)
                )
            }
            "subscribe-notifications" if !draining => match stream.try_clone() {
                Ok(subscriber) => {
                    let _ = subscriber.set_read_timeout(Some(Duration::from_secs(1)));
                    let _ = subscriber.set_write_timeout(Some(Duration::from_secs(1)));
                    new_notification_subscriber = Some(subscriber);
                    format!("ok {PROTOCOL_VERSION} subscribed\n")
                }
                Err(_) => "error subscribe-failed\n".to_string(),
            },
            _ => "error unknown-command\n".to_string(),
        }
    };
    if stream.write_all(response.as_bytes()).is_ok()
        && let Some(subscriber) = new_notification_subscriber
        && let Ok(mut current) = notification_subscriber.lock()
    {
        current.push(subscriber);
    }
    command == "shutdown"
}

#[derive(serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(
    clippy::enum_variant_names,
    reason = "the stable wire tags are deliberately namespaced with workflow_"
)]
enum WorkflowSocketRequest {
    WorkflowRegisterDefinition {
        definition: SocketDefinition,
    },
    WorkflowLaunch {
        run: SocketRun,
        #[serde(default)]
        steps: Vec<SocketStep>,
    },
    WorkflowList {
        repository: Option<String>,
        limit: usize,
    },
    WorkflowInspect {
        run_id: String,
    },
    WorkflowInspectMany {
        run_ids: Vec<String>,
    },
    WorkflowCommand {
        run_id: String,
        command: SocketWorkflowCommand,
        now_unix_ms: i64,
    },
    WorkflowRequestApproval {
        id: String,
        run_id: String,
        step_id: String,
        now_unix_ms: i64,
    },
    WorkflowDecideApproval {
        id: String,
        decision: SocketApprovalDecision,
        decided_by: String,
        note: Option<String>,
        now_unix_ms: i64,
    },
    WorkflowGrantAuthority {
        id: String,
        run_id: String,
        scope: String,
        granted_by: String,
        now_unix_ms: i64,
        expires_unix_ms: Option<i64>,
    },
    WorkflowRegisterTrigger {
        id: String,
        definition_snapshot_id: String,
        overlap_policy: String,
        config_json: String,
        enabled: bool,
    },
    WorkflowRecordTriggerOccurrence {
        id: String,
        trigger_id: String,
        deduplication_key: String,
        due_unix_ms: i64,
    },
    WorkflowCompleteTrigger {
        occurrence_id: String,
        run_id: String,
        checkpoint_json: String,
        now_unix_ms: i64,
    },
    WorkflowWaitOnGate {
        step_id: String,
        gate_kind: String,
        due_unix_ms: i64,
        checkpoint_json: String,
        now_unix_ms: i64,
    },
}

#[derive(serde::Deserialize)]
struct SocketDefinition {
    id: String,
    name: String,
    revision: String,
    source: String,
    trusted: bool,
    body_json: String,
    digest: String,
    now_unix_ms: i64,
}

#[derive(serde::Deserialize)]
struct SocketRun {
    run_id: String,
    definition_snapshot_id: String,
    repository: Option<String>,
    idempotency_key: String,
    #[serde(default = "empty_json_object")]
    input_json: String,
    now_unix_ms: i64,
}

fn empty_json_object() -> String {
    "{}".into()
}

#[derive(serde::Deserialize)]
struct SocketStep {
    id: String,
    key: String,
    implementation: String,
    target_id: String,
    input_json: String,
    #[serde(default)]
    dependencies: Vec<String>,
    #[serde(default)]
    resources: Vec<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum SocketWorkflowCommand {
    Pause,
    Resume,
    Cancel,
    Retry,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum SocketApprovalDecision {
    Approve,
    Reject,
}

fn workflow_socket_response(operations: &crate::WorkflowOperations, request: &str) -> String {
    let request = match serde_json::from_str::<WorkflowSocketRequest>(request) {
        Ok(request) => request,
        Err(error) => {
            return format!(
                "{}\n",
                serde_json::json!({"ok": false, "error": format!("invalid workflow request: {error}")})
            );
        }
    };
    let result = crate::async_runtime::block_on(async {
        match request {
            WorkflowSocketRequest::WorkflowRegisterDefinition { definition } => operations
                .register_definition(crate::DefinitionSnapshot {
                    id: &definition.id,
                    name: &definition.name,
                    revision: &definition.revision,
                    source: &definition.source,
                    trusted: definition.trusted,
                    body_json: &definition.body_json,
                    digest: &definition.digest,
                    now_unix_ms: definition.now_unix_ms,
                })
                .await
                .map(|()| serde_json::json!({"ok": true})),
            WorkflowSocketRequest::WorkflowLaunch { run, steps } => {
                let command = crate::LaunchWorkflow {
                    run_id: &run.run_id,
                    definition_snapshot_id: &run.definition_snapshot_id,
                    repository: run.repository.as_deref(),
                    idempotency_key: &run.idempotency_key,
                    input_json: &run.input_json,
                    now_unix_ms: run.now_unix_ms,
                };
                if steps.is_empty() {
                    operations.launch(command).await
                } else {
                    // Materialized launches remain available to low-level worker contract tests;
                    // application launches send no Steps and materialize the pinned snapshot.
                    operations
                        .launch_definition(
                            command,
                            steps
                                .into_iter()
                                .map(|step| crate::WorkflowStep {
                                    id: step.id,
                                    key: step.key,
                                    implementation: step.implementation,
                                    target_id: step.target_id,
                                    input_json: step.input_json,
                                    dependencies: step.dependencies,
                                    resources: step.resources,
                                })
                                .collect(),
                        )
                        .await
                }
                .map(|run_id| serde_json::json!({"ok": true, "run_id": run_id}))
            }
            WorkflowSocketRequest::WorkflowList { repository, limit } => operations
                .list(repository.as_deref(), limit)
                .await
                .map(|runs| serde_json::json!({"ok": true, "runs": runs})),
            WorkflowSocketRequest::WorkflowInspect { run_id } => operations
                .inspect(&run_id)
                .await
                .map(|run| serde_json::json!({"ok": true, "run": run})),
            WorkflowSocketRequest::WorkflowInspectMany { run_ids } => {
                let mut runs = Vec::with_capacity(run_ids.len());
                for run_id in run_ids {
                    if let Some(run) = operations.inspect(&run_id).await? {
                        runs.push(run);
                    }
                }
                Ok(serde_json::json!({"ok": true, "runs": runs}))
            }
            WorkflowSocketRequest::WorkflowCommand {
                run_id,
                command,
                now_unix_ms,
            } => operations
                .command(
                    &run_id,
                    match command {
                        SocketWorkflowCommand::Pause => crate::WorkflowCommand::Pause,
                        SocketWorkflowCommand::Resume => crate::WorkflowCommand::Resume,
                        SocketWorkflowCommand::Cancel => crate::WorkflowCommand::Cancel,
                        SocketWorkflowCommand::Retry => crate::WorkflowCommand::Retry,
                    },
                    now_unix_ms,
                )
                .await
                .map(|()| serde_json::json!({"ok": true})),
            WorkflowSocketRequest::WorkflowRequestApproval {
                id,
                run_id,
                step_id,
                now_unix_ms,
            } => operations
                .request_approval(&id, &run_id, &step_id, now_unix_ms)
                .await
                .map(|()| serde_json::json!({"ok": true})),
            WorkflowSocketRequest::WorkflowDecideApproval {
                id,
                decision,
                decided_by,
                note,
                now_unix_ms,
            } => operations
                .decide_approval(
                    &id,
                    match decision {
                        SocketApprovalDecision::Approve => crate::ApprovalDecision::Approve,
                        SocketApprovalDecision::Reject => crate::ApprovalDecision::Reject,
                    },
                    &decided_by,
                    note.as_deref(),
                    now_unix_ms,
                )
                .await
                .map(|()| serde_json::json!({"ok": true})),
            WorkflowSocketRequest::WorkflowGrantAuthority {
                id,
                run_id,
                scope,
                granted_by,
                now_unix_ms,
                expires_unix_ms,
            } => operations
                .grant_authority(
                    &id,
                    &run_id,
                    &scope,
                    &granted_by,
                    now_unix_ms,
                    expires_unix_ms,
                )
                .await
                .map(|()| serde_json::json!({"ok": true})),
            WorkflowSocketRequest::WorkflowRegisterTrigger {
                id,
                definition_snapshot_id,
                overlap_policy,
                config_json,
                enabled,
            } => operations
                .register_trigger(
                    &id,
                    &definition_snapshot_id,
                    &overlap_policy,
                    &config_json,
                    enabled,
                )
                .await
                .map(|()| serde_json::json!({"ok": true})),
            WorkflowSocketRequest::WorkflowRecordTriggerOccurrence {
                id,
                trigger_id,
                deduplication_key,
                due_unix_ms,
            } => operations
                .record_trigger_occurrence(&id, &trigger_id, &deduplication_key, due_unix_ms)
                .await
                .map(|inserted| serde_json::json!({"ok": true, "inserted": inserted})),
            WorkflowSocketRequest::WorkflowCompleteTrigger {
                occurrence_id,
                run_id,
                checkpoint_json,
                now_unix_ms,
            } => operations
                .complete_trigger(&occurrence_id, &run_id, &checkpoint_json, now_unix_ms)
                .await
                .map(|()| serde_json::json!({"ok": true})),
            WorkflowSocketRequest::WorkflowWaitOnGate {
                step_id,
                gate_kind,
                due_unix_ms,
                checkpoint_json,
                now_unix_ms,
            } => operations
                .wait_on_gate(
                    &step_id,
                    &gate_kind,
                    due_unix_ms,
                    &checkpoint_json,
                    now_unix_ms,
                )
                .await
                .map(|()| serde_json::json!({"ok": true})),
        }
    });
    match result {
        Ok(Ok(value)) => format!("{value}\n"),
        Ok(Err(error)) => format!(
            "{}\n",
            serde_json::json!({"ok": false, "error": error.to_string()})
        ),
        Err(error) => format!(
            "{}\n",
            serde_json::json!({"ok": false, "error": error.to_string()})
        ),
    }
}

fn notification_loop(stop: Arc<AtomicBool>, subscriber: Arc<Mutex<Vec<UnixStream>>>) {
    while !stop.load(Ordering::Acquire) {
        if let Err(error) = observe_and_deliver_notifications(&subscriber) {
            eprintln!("Prism notification observer failed: {error}");
        }
        let deadline = Instant::now() + NOTIFICATION_POLL_INTERVAL;
        while !stop.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(100));
        }
    }
}

fn observe_and_deliver_notifications(
    subscriber: &Arc<Mutex<Vec<UnixStream>>>,
) -> Result<(), String> {
    let entries = workspace::load_entries()?;
    for entry in workspace::discover_valid_entries(entries) {
        let repo = entry.repo;
        if let Err(error) = observability::attach_run_repo(&repo) {
            eprintln!(
                "Prism notification observer cannot attach {}: {error}",
                repo.root.display()
            );
            continue;
        }
        let config = Config::load(&repo);
        if !config.config_errors.is_empty() {
            eprintln!(
                "Prism notification observer skipped {} because configuration is invalid",
                repo.root.display()
            );
            continue;
        }
        let sessions = match crate::session::discover_sessions(&repo, &config) {
            Ok(sessions) => sessions,
            Err(error) => {
                eprintln!(
                    "Prism notification observer cannot discover {}: {error}",
                    repo.root.display()
                );
                continue;
            }
        };
        let repository = crate::session::WorktreeRepositoryKey::new(repo.root.clone());
        let repo_label = workspace::label_for_root(&repo.root);
        let observed_unix_ms = current_unix_ms();
        let mut observations = Vec::new();
        let mut live = Vec::new();
        for session in sessions
            .iter()
            .filter(|session| !session.hidden && session.path.exists())
        {
            let identity = session.identity_key(&repository);
            live.push(identity.clone());
            match observe_interactive_agent(&repo, &config, session) {
                Ok(state) => observations.push((identity, state)),
                Err(error) => eprintln!(
                    "Prism notification observer cannot inspect {}: {error}",
                    session.branch
                ),
            }
        }
        let result = (|| {
            let coordinator = NotificationCoordinator::open(&observability::db_path(&repo))?;
            coordinator.abandon_uncertain(observed_unix_ms)?;
            coordinator.retain(live.iter(), observed_unix_ms)?;
            for (session, state) in &observations {
                let state = resolve_observed_state(*state, coordinator.last_state(session)?);
                let Some(state) = state else { continue };
                coordinator.observe(NotificationObservation {
                    session,
                    repo_label: &repo_label,
                    state,
                    config: config.notifications,
                    observed_unix_ms,
                })?;
            }
            dispatch_pending_notifications(&coordinator, subscriber, observed_unix_ms)
        })();
        if let Err(error) = result {
            eprintln!(
                "Prism notification observer cannot update {}: {error}",
                repo.root.display()
            );
        }
    }
    Ok(())
}

fn observe_interactive_agent(
    repo: &Repository,
    config: &Config,
    session: &crate::session::Session,
) -> Result<Option<crate::agent::AgentState>, String> {
    let association = crate::session::worktree_harness(repo, session)?;
    let effective_config = config.for_harness(&association.harness_id)?;
    let generation = crate::tmux::latest_agent_session_generation_result(
        repo,
        &effective_config,
        &session.branch,
    )?;
    let running = match generation {
        Some(generation) => {
            crate::tmux::agent_session_running_result(repo, &effective_config, session, generation)?
        }
        None => false,
    };
    if effective_config.selected_adapter_is("opencode")
        && let Some(runtime) = crate::opencode::load_runtime_snapshot(
            repo,
            &association.harness_id,
            &session.branch,
            &session.path,
        )?
    {
        return normalize_opencode_observation(
            running,
            crate::opencode::poll_status_authoritative(&runtime)
                .map(|status| status.state.agent_state()),
        );
    }
    Ok(generation.map(|_| normalize_interactive_state(running, None)))
}

fn normalize_opencode_observation(
    running: bool,
    observed: Result<crate::agent::AgentState, String>,
) -> Result<Option<crate::agent::AgentState>, String> {
    match observed {
        Ok(state) => Ok(Some(normalize_interactive_state(running, Some(state)))),
        Err(error) => Err(error),
    }
}

fn normalize_interactive_state(
    running: bool,
    rich_state: Option<crate::agent::AgentState>,
) -> crate::agent::AgentState {
    match rich_state {
        Some(crate::agent::AgentState::NeedsRestart) if running => {
            crate::agent::AgentState::Running
        }
        Some(state) => state,
        None if running => crate::agent::AgentState::Running,
        None => crate::agent::AgentState::ExitedOk,
    }
}

fn resolve_observed_state(
    observed: Option<crate::agent::AgentState>,
    previous: Option<crate::agent::AgentState>,
) -> Option<crate::agent::AgentState> {
    match (observed, previous) {
        (Some(state), _) => Some(state),
        (None, Some(crate::agent::AgentState::Attached | crate::agent::AgentState::Running)) => {
            Some(crate::agent::AgentState::ExitedOk)
        }
        (None, _) => None,
    }
}

fn dispatch_pending_notifications(
    coordinator: &NotificationCoordinator,
    subscriber: &Arc<Mutex<Vec<UnixStream>>>,
    now_unix_ms: i64,
) -> Result<(), String> {
    coordinator.expire_pending(now_unix_ms)?;
    loop {
        #[cfg(target_os = "macos")]
        if subscriber
            .lock()
            .map(|subscriber| subscriber.is_empty())
            .unwrap_or(true)
        {
            return Ok(());
        }
        let Some(notification) = coordinator.claim_next(now_unix_ms)? else {
            return Ok(());
        };
        match deliver_worker_notification(&notification, subscriber) {
            DeliveryOutcome::Accepted => {
                coordinator.mark_accepted(notification.id, current_unix_ms())?
            }
            DeliveryOutcome::Retry(category) => coordinator.retry(
                notification.id,
                current_unix_ms().saturating_add(
                    NOTIFICATION_RETRY_INTERVAL
                        .as_millis()
                        .min(i64::MAX as u128) as i64,
                ),
                category,
            )?,
            #[cfg(target_os = "macos")]
            DeliveryOutcome::Uncertain(category) => {
                coordinator.mark_uncertain(notification.id, current_unix_ms(), category)?
            }
        }
    }
}

enum DeliveryOutcome {
    Accepted,
    Retry(&'static str),
    #[cfg(target_os = "macos")]
    Uncertain(&'static str),
}

#[cfg(target_os = "linux")]
fn deliver_worker_notification(
    notification: &PendingNotification,
    _subscriber: &Arc<Mutex<Vec<UnixStream>>>,
) -> DeliveryOutcome {
    match crate::desktop_notification::deliver_native_notification(
        &notification.title,
        &notification.body,
    ) {
        Ok(()) => DeliveryOutcome::Accepted,
        Err(category) => DeliveryOutcome::Retry(category),
    }
}

#[cfg(target_os = "macos")]
fn deliver_worker_notification(
    notification: &PendingNotification,
    subscriber: &Arc<Mutex<Vec<UnixStream>>>,
) -> DeliveryOutcome {
    let message = serde_json::json!({
        "id": notification.id,
        "title": notification.title,
        "body": notification.body,
    });
    let Ok(mut subscribers) = subscriber.lock() else {
        return DeliveryOutcome::Retry("subscriber_lock");
    };
    while let Some(stream) = subscribers.last_mut() {
        if stream.write_all(format!("{message}\n").as_bytes()).is_err() {
            subscribers.pop();
            continue;
        }
        let response = read_notification_ack(stream);
        let accepted = format!("accepted {}", notification.id);
        let failed = format!("failed {}", notification.id);
        return match response.as_deref() {
            Ok(response) if response == accepted => DeliveryOutcome::Accepted,
            Ok(response) if response.starts_with(&failed) => {
                subscribers.pop();
                continue;
            }
            _ => {
                subscribers.pop();
                DeliveryOutcome::Uncertain("subscriber_ack")
            }
        };
    }
    DeliveryOutcome::Retry("subscriber_unavailable")
}

#[cfg(target_os = "macos")]
fn read_notification_ack(stream: &mut UnixStream) -> Result<String, std::io::Error> {
    let mut response = Vec::new();
    let mut byte = [0_u8; 1];
    while response.len() < 128 {
        let size = stream.read(&mut byte)?;
        if size == 0 || byte[0] == b'\n' {
            break;
        }
        response.push(byte[0]);
    }
    Ok(String::from_utf8_lossy(&response).to_string())
}

fn current_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn log_daemon_lifecycle(action: &str, instance_id: &str) {
    let entries = match workspace::load_entries() {
        Ok(entries) => entries,
        Err(error) => {
            eprintln!("Prism worker cannot load repositories: {error}");
            return;
        }
    };
    for entry in workspace::discover_valid_entries(entries) {
        let data = format!("{{\"daemon_instance_id\":\"{instance_id}\"}}");
        log_worker_event(
            &entry.repo,
            action,
            "Prism worker daemon lifecycle",
            Some(&data),
        );
    }
}

fn log_worker_event(repo: &Repository, action: &str, message: &str, data_json: Option<&str>) {
    let suffix = data_json.map_or_else(String::new, |data| format!(" {data}"));
    if let Err(error) = observability::append_runtime_message(
        repo,
        &format!("info worker.{action} {message}{suffix}"),
    ) {
        eprintln!(
            "Prism worker cannot persist lifecycle event for {}: {error}",
            repo.root.display()
        );
    }
}

fn new_instance_id(prefix: &str) -> String {
    format!("{prefix}-{}-{}", std::process::id(), current_unix_ms())
}

fn acquire_lock(path: &Path) -> Result<File, String> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| format!("open Prism worker lock: {error}"))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("secure Prism worker lock: {error}"))?;
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
    let override_path = std::env::var_os("PRISM_RUNTIME_DIR").filter(|path| !path.is_empty());
    let xdg_runtime = std::env::var_os("XDG_RUNTIME_DIR").filter(|path| !path.is_empty());
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty());
    runtime_dir_for(
        crate::platform::current_os(),
        override_path.as_deref(),
        xdg_runtime.as_deref(),
        home.as_deref(),
        &crate::util::prism_config_dir(),
    )
}

fn runtime_dir_for(
    os: SupportedOs,
    override_path: Option<&std::ffi::OsStr>,
    xdg_runtime: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
    fallback_config: &Path,
) -> PathBuf {
    if let Some(path) = override_path {
        return PathBuf::from(path);
    }
    if os == SupportedOs::Linux
        && let Some(path) = xdg_runtime
    {
        return PathBuf::from(path).join("prism");
    }
    if os == SupportedOs::MacOs
        && let Some(home) = home
    {
        return PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("Prism")
            .join("runtime");
    }
    fallback_config.join("runtime")
}

pub fn socket_path() -> PathBuf {
    runtime_dir().join("worker.sock")
}

fn validated_socket_path() -> Result<WorkerSocketPath, String> {
    WorkerSocketPath::for_runtime(&runtime_dir())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn test_socket_path() -> (crate::compact_runtime::CompactTempDir, WorkerSocketPath) {
        let runtime = crate::compact_runtime::CompactTempDir::new("worker-socket");
        fs::create_dir(runtime.runtime_path()).unwrap();
        let socket = WorkerSocketPath::for_runtime(runtime.runtime_path()).unwrap();
        (runtime, socket)
    }

    #[test]
    fn notification_observation_prefers_rich_state_without_false_offline_restart() {
        use crate::agent::AgentState;

        assert_eq!(
            normalize_interactive_state(true, Some(AgentState::NeedsRestart)),
            AgentState::Running
        );
        assert_eq!(
            normalize_interactive_state(true, Some(AgentState::NeedsInput)),
            AgentState::NeedsInput
        );
        assert_eq!(
            normalize_interactive_state(false, Some(AgentState::ExitedError)),
            AgentState::ExitedError
        );
        assert_eq!(
            normalize_interactive_state(false, None),
            AgentState::ExitedOk
        );
        assert_eq!(
            resolve_observed_state(None, Some(AgentState::Running)),
            Some(AgentState::ExitedOk)
        );
        assert_eq!(resolve_observed_state(None, None), None);
    }

    #[test]
    fn transient_opencode_poll_failure_does_not_repeat_a_completion_notification() {
        use crate::agent::AgentState;
        use crate::config::NotificationConfig;
        use crate::notification::{NotificationCoordinator, NotificationObservation};
        use crate::session::{WorktreeRepositoryKey, WorktreeSessionKey};

        let temp = crate::compact_runtime::CompactTempDir::new("notification-poll");
        let coordinator =
            NotificationCoordinator::open(&temp.runtime_path().join("state.db")).unwrap();
        let session = WorktreeSessionKey {
            repository: WorktreeRepositoryKey::new("/tmp/repo".into()),
            path: "/tmp/repo/feature".into(),
            branch: "feature".to_string(),
            incarnation: "one".to_string(),
        };
        let config = NotificationConfig {
            enabled: true,
            completed: true,
            ..NotificationConfig::default()
        };
        let observe = |state, at| {
            coordinator
                .observe(NotificationObservation {
                    session: &session,
                    repo_label: "repo",
                    state,
                    config,
                    observed_unix_ms: at,
                })
                .unwrap()
                .event_id
        };

        assert_eq!(observe(AgentState::Running, 1_000), None);
        assert!(observe(AgentState::ExitedOk, 2_000).is_some());
        let transient = normalize_opencode_observation(true, Err("timeout".to_string()));
        if let Ok(Some(state)) = transient.as_ref() {
            observe(*state, 3_000);
        }
        assert!(transient.is_err());
        assert_eq!(observe(AgentState::ExitedOk, 4_000), None);
        assert_eq!(coordinator.pending().unwrap().len(), 1);
    }

    #[test]
    fn notification_subscription_keeps_a_worker_to_tui_stream() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let subscriber = Arc::new(Mutex::new(Vec::new()));
        client.write_all(b"subscribe-notifications\n").unwrap();

        assert!(!respond(
            &mut server,
            "daemon-test",
            "generation-test",
            &subscriber,
            None,
            false,
        ));
        let mut acknowledgement = [0_u8; 64];
        let size = client.read(&mut acknowledgement).unwrap();
        assert_eq!(
            std::str::from_utf8(&acknowledgement[..size]).unwrap(),
            format!("ok {PROTOCOL_VERSION} subscribed\n")
        );

        subscriber
            .lock()
            .unwrap()
            .last_mut()
            .unwrap()
            .write_all(b"{\"title\":\"Prism\",\"body\":\"ready\"}\n")
            .unwrap();
        let mut message = [0_u8; 64];
        let size = client.read(&mut message).unwrap();
        assert_eq!(
            std::str::from_utf8(&message[..size]).unwrap(),
            "{\"title\":\"Prism\",\"body\":\"ready\"}\n"
        );
    }

    #[test]
    fn notification_subscription_reconnects_after_a_protocol_error() {
        let (_runtime, socket) = test_socket_path();
        let listener = UnixListener::bind(socket.as_path()).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let listener_stop = Arc::clone(&stop);
        let subscription_socket = socket.clone();
        let subscription = thread::spawn(move || {
            notification_subscription_loop_with_delivery(
                &subscription_socket,
                &listener_stop,
                |_, _| Ok(()),
            );
        });

        let (mut rejected, _) = listener.accept().unwrap();
        let mut request = [0_u8; 64];
        let size = rejected.read(&mut request).unwrap();
        assert_eq!(
            std::str::from_utf8(&request[..size]).unwrap(),
            "subscribe-notifications\n"
        );
        rejected.write_all(b"error unknown-command\n").unwrap();

        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let reconnected = loop {
            match listener.accept() {
                Ok((_stream, _)) => break true,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        break false;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("accept replacement subscription: {error}"),
            }
        };

        stop.store(true, Ordering::Release);
        subscription.join().unwrap();
        assert!(
            reconnected,
            "subscription did not reconnect after protocol error"
        );
    }

    #[test]
    fn socket_and_lock_share_a_private_runtime_directory() {
        let socket = socket_path();
        assert_eq!(socket.parent(), Some(runtime_dir().as_path()));
        assert_eq!(
            socket.file_name().and_then(|name| name.to_str()),
            Some("worker.sock")
        );
    }

    #[test]
    fn platform_contract_runtime_paths_cover_linux_and_macos() {
        assert_eq!(
            runtime_dir_for(
                SupportedOs::Linux,
                None,
                Some(OsStr::new("/run/user/1000")),
                Some(OsStr::new("/home/user")),
                Path::new("/fallback"),
            ),
            PathBuf::from("/run/user/1000/prism")
        );
        assert_eq!(
            runtime_dir_for(
                SupportedOs::MacOs,
                None,
                None,
                Some(OsStr::new("/Users/user")),
                Path::new("/fallback"),
            ),
            PathBuf::from("/Users/user/Library/Application Support/Prism/runtime")
        );
        assert_eq!(
            runtime_dir_for(
                SupportedOs::Linux,
                Some(OsStr::new("/override")),
                Some(OsStr::new("/ignored")),
                None,
                Path::new("/fallback"),
            ),
            PathBuf::from("/override")
        );
    }

    #[test]
    fn socket_path_budget_accepts_the_boundary_and_rejects_the_next_byte() {
        for byte_len in [SOCKET_PATH_BUDGET - 1, SOCKET_PATH_BUDGET] {
            let runtime = runtime_with_socket_path_len(byte_len);
            let socket = WorkerSocketPath::for_runtime(&runtime).unwrap();
            assert_eq!(socket_path_bytes(socket.as_path()).len(), byte_len);
        }

        let runtime = runtime_with_socket_path_len(SOCKET_PATH_BUDGET + 1);
        let error = WorkerSocketPath::for_runtime(&runtime).unwrap_err();
        assert!(error.contains("103 bytes"), "{error}");
        assert!(error.contains("PRISM_RUNTIME_DIR"), "{error}");
    }

    #[test]
    fn socket_path_rejects_nul_before_dispatch() {
        use std::os::unix::ffi::OsStringExt;

        let runtime = PathBuf::from(std::ffi::OsString::from_vec(b"/tmp/prism\0bad".to_vec()));
        let error = WorkerSocketPath::for_runtime(&runtime).unwrap_err();

        assert!(error.contains("NUL byte"), "{error}");
        assert!(error.contains("PRISM_RUNTIME_DIR"), "{error}");
    }

    #[test]
    fn socket_path_accepts_non_utf8_unix_paths() {
        use std::os::unix::ffi::OsStringExt;

        let runtime = PathBuf::from(std::ffi::OsString::from_vec(b"/tmp/prism-\xff".to_vec()));
        let socket = WorkerSocketPath::for_runtime(&runtime).unwrap();

        assert_eq!(socket.as_path(), runtime.join("worker.sock"));
    }

    #[test]
    fn long_runtime_policy_inputs_are_rejected_by_the_socket_invariant() {
        let long = OsStr::new(
            "/runtime/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        );
        for runtime in [
            runtime_dir_for(
                SupportedOs::Linux,
                Some(long),
                None,
                None,
                Path::new("/fallback"),
            ),
            runtime_dir_for(
                SupportedOs::Linux,
                None,
                Some(long),
                None,
                Path::new("/fallback"),
            ),
            runtime_dir_for(
                SupportedOs::MacOs,
                None,
                None,
                Some(long),
                Path::new("/fallback"),
            ),
            runtime_dir_for(SupportedOs::Linux, None, None, None, Path::new(long)),
        ] {
            assert!(WorkerSocketPath::for_runtime(&runtime).is_err());
        }
    }

    #[test]
    fn platform_smoke_native_worker_socket_bind_and_connect() {
        let (_runtime, socket) = test_socket_path();
        let listener = UnixListener::bind(socket.as_path()).unwrap();

        let client = UnixStream::connect(socket.as_path()).unwrap();
        let (_server, _) = listener.accept().unwrap();

        drop(client);
        drop(listener);
    }

    #[test]
    fn platform_smoke_native_worker_socket_binds_at_portable_path_boundary() {
        let runtime_len = SOCKET_PATH_BUDGET - b"/worker.sock".len();
        let mut runtime = format!("/tmp/pb-{:x}-", std::process::id());
        runtime.extend(std::iter::repeat_n('x', runtime_len - runtime.len()));
        let runtime = PathBuf::from(runtime);
        fs::create_dir(&runtime).unwrap();
        let socket = WorkerSocketPath::for_runtime(&runtime).unwrap();
        assert_eq!(
            socket_path_bytes(socket.as_path()).len(),
            SOCKET_PATH_BUDGET
        );

        let listener = UnixListener::bind(socket.as_path()).unwrap();
        let client = UnixStream::connect(socket.as_path()).unwrap();
        let (_server, _) = listener.accept().unwrap();

        drop(client);
        drop(listener);
        fs::remove_file(socket.as_path()).unwrap();
        fs::remove_dir(runtime).unwrap();
    }

    #[test]
    fn platform_smoke_native_probe_health_treats_a_stale_socket_as_stopped() {
        let (_runtime, socket) = test_socket_path();
        let listener = UnixListener::bind(socket.as_path()).unwrap();
        drop(listener);

        assert_eq!(probe_health_at(&socket).unwrap(), DaemonHealth::stopped());
    }

    #[test]
    fn platform_smoke_native_waiting_for_a_live_socket_to_close_times_out() {
        let (_runtime, socket) = test_socket_path();
        let _listener = UnixListener::bind(socket.as_path()).unwrap();

        assert_eq!(
            wait_for_socket_to_close(&socket, Duration::ZERO),
            Err("timed out waiting for Prism worker daemon to stop".to_string())
        );
    }

    #[test]
    fn socket_launch_preserves_typed_run_inputs() {
        use prism_extension_protocol::{
            ArtifactSchemaDescriptor, ExtensionDescriptor, ImplementationDescriptor,
            PortDescriptor, StepClass,
        };

        let mut registry = crate::extension::DescriptorRegistry::default();
        registry
            .register(&ExtensionDescriptor {
                artifact_schemas: vec![ArtifactSchemaDescriptor {
                    id: "acme.test/text".into(),
                    schema: serde_json::json!({"type":"string"}),
                }],
                implementations: vec![ImplementationDescriptor {
                    id: "acme.test/action".into(),
                    class: StepClass::Action,
                    inputs: vec![PortDescriptor {
                        name: "subject".into(),
                        schema: "acme.test/text".into(),
                        required: true,
                    }],
                    outputs: vec![],
                    capabilities: vec![],
                    targets: vec!["local".into()],
                    effect_boundary: Default::default(),
                }],
                ..ExtensionDescriptor::default()
            })
            .unwrap();
        let catalog = crate::workflow::definition::DefinitionCatalog::from_sources(
            [(
                "workflow.toml".into(),
                "schema_version=2\nid='acme.test/runtime'\nname='runtime'\nlaunch=['manual']\n[inputs.subject]\ntype='acme.test/text'\nrequired=true\n[[steps]]\nid='action'\nclass='action'\nuse='acme.test/action'\nskippable=false\n[steps.inputs]\nsubject='inputs.subject'\n".into(),
            )],
            registry,
        )
        .unwrap();
        let snapshot = catalog.compile("acme.test/runtime").unwrap();
        let body = serde_json::to_string(&snapshot).unwrap();
        let temp = crate::compact_runtime::CompactTempDir::new("worker-launch");
        let database_path = temp.runtime_path().join("workflow.db");
        let operations =
            crate::async_runtime::block_on(crate::WorkflowOperations::open(&database_path))
                .unwrap()
                .unwrap();
        crate::async_runtime::block_on(operations.register_definition(crate::DefinitionSnapshot {
            id: &snapshot.digest,
            name: "definition",
            revision: "1",
            source: "test",
            trusted: true,
            body_json: &body,
            digest: &snapshot.digest,
            now_unix_ms: 1,
        }))
        .unwrap()
        .unwrap();

        let response = workflow_socket_response(
            &operations,
            &serde_json::json!({
                "type": "workflow_launch",
                "run": {
                    "run_id": "run",
                    "definition_snapshot_id": snapshot.digest,
                    "repository": "/repo",
                    "idempotency_key": "run",
                    "input_json": r#"{"subject":"typed"}"#,
                    "now_unix_ms": 2
                }
            })
            .to_string(),
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&response).unwrap()["ok"],
            true
        );

        let database = crate::async_runtime::block_on(
            crate::persistence::pools::WorkflowDatabase::open(&database_path),
        )
        .unwrap()
        .unwrap();
        let input: String = crate::async_runtime::block_on(
            sqlx::query_scalar("select input_json from workflow_run where id = 'run'")
                .fetch_one(database.readers()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(input, r#"{"subject":"typed"}"#);
    }

    #[test]
    fn queued_launch_failure_names_the_durable_run() {
        assert_eq!(
            confirm_queued_worker("run-123".into(), Err("connection reset".into())),
            Err("Workflow Run run-123 is durably queued, but the Prism worker became unavailable: connection reset".into())
        );
        assert_eq!(
            confirm_queued_worker("run-123".into(), Ok("awake".into())),
            Ok("run-123".into())
        );
    }

    #[test]
    fn waiting_for_a_draining_daemon_times_out() {
        assert_eq!(
            wait_for_existing_daemon(Duration::ZERO, || Ok(DaemonHealth {
                state: DaemonState::Draining,
                protocol_version: Some(PROTOCOL_VERSION),
                instance_id: Some("test".to_string()),
                pid: Some(std::process::id()),
                binary_generation: Some("generation-test".into()),
                active: 2,
                notifications: false,
            })),
            Err(
                "timed out waiting for Prism worker daemon to finish draining (2 active)"
                    .to_string()
            )
        );
    }

    #[test]
    fn daemon_probe_errors_are_not_treated_as_a_stopped_daemon() {
        assert_eq!(
            wait_for_existing_daemon(Duration::ZERO, || Err("permission denied".to_string())),
            Err("permission denied".to_string())
        );
    }

    #[test]
    fn stale_protocol_or_executable_generation_is_not_reused() {
        let legacy = parse_health_response(
            "ok 1 legacy-worker pid=123 state=running active=0 notifications=1",
        )
        .unwrap();
        let stale_build = parse_health_response(&format!(
            "ok {PROTOCOL_VERSION} stale-worker pid=123 generation=old state=running active=0 notifications=1"
        ))
        .unwrap();
        let current = parse_health_response(&format!(
            "ok {PROTOCOL_VERSION} current-worker pid=123 generation=current state=running active=0 notifications=1"
        ))
        .unwrap();

        assert!(!daemon_is_current(&legacy, "current"));
        assert!(!daemon_is_current(&stale_build, "current"));
        assert!(daemon_is_current(&current, "current"));
    }

    fn runtime_with_socket_path_len(byte_len: usize) -> PathBuf {
        use std::os::unix::ffi::OsStringExt;

        const SOCKET_SUFFIX_LEN: usize = b"/worker.sock".len();
        let mut bytes = vec![b'/'];
        bytes.extend(std::iter::repeat_n(b'a', byte_len - SOCKET_SUFFIX_LEN - 1));
        PathBuf::from(std::ffi::OsString::from_vec(bytes))
    }
}
