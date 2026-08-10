use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::execution;
use crate::notification::{NotificationCoordinator, NotificationObservation, PendingNotification};
use crate::platform::SupportedOs;
use crate::process::DetachedProcessPolicy;
use crate::repo::Repository;
use crate::workflow::worker_ipc::{self, WorkerEndpoint, WorkerStream};
use crate::{observability, workspace};

const PROTOCOL_VERSION: u32 = 1;
const NOTIFICATION_POLL_INTERVAL: Duration = Duration::from_secs(5);
const NOTIFICATION_RETRY_INTERVAL: Duration = Duration::from_secs(10);
const DAEMON_TRANSITION_TIMEOUT: Duration = Duration::from_secs(3);

const fn notification_backend_available() -> bool {
    !matches!(
        crate::platform::desktop_notification_policy(crate::platform::current_os()),
        crate::platform::DesktopNotificationPolicy::Unavailable
    )
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
            active: 0,
            notifications: false,
        }
    }
}

pub fn probe_health() -> Result<DaemonHealth, String> {
    probe_health_at(&validated_socket_path()?)
}

fn probe_health_at(endpoint: &WorkerEndpoint) -> Result<DaemonHealth, String> {
    let stream = match endpoint.connect() {
        Ok(stream) => stream,
        Err(error) if worker_ipc::endpoint_unavailable(&error) => {
            return Ok(DaemonHealth::stopped());
        }
        Err(error) => return Err(format!("connect to Prism worker: {error}")),
    };
    let secret = worker_ipc::read_secret(endpoint)?;
    let response = match request_on_stream_raw(stream, &authenticated_command(&secret, "health")) {
        Ok(response) => response,
        Err((_, error)) if worker_ipc::connection_closed(&error) => {
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
    if version != PROTOCOL_VERSION {
        return Err(format!("incompatible Prism daemon protocol {version}"));
    }
    let instance_id = fields
        .next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("missing Prism daemon instance ID: {response}"))?;
    let mut pid: Option<u32> = None;
    let mut state = None;
    let mut active = None;
    let mut notifications = false;
    for field in fields {
        if let Some(value) = field.strip_prefix("pid=") {
            pid = value.parse().ok();
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
        active: active.ok_or_else(|| format!("missing Prism daemon active count: {response}"))?,
        notifications,
    })
}

pub fn ensure_running() -> Result<(), String> {
    let socket = validated_socket_path()?;
    if std::env::var_os("PRISM_WAIT_FOR_WORKER_DRAIN").is_some() {
        loop {
            match probe_health_at(&socket)? {
                DaemonHealth {
                    state: DaemonState::Stopped,
                    ..
                } => break,
                DaemonHealth {
                    state: DaemonState::Running,
                    notifications: true,
                    ..
                } => return Ok(()),
                _ => thread::sleep(Duration::from_millis(250)),
            }
        }
    }
    if wait_for_existing_daemon(DAEMON_TRANSITION_TIMEOUT, || probe_health_at(&socket))? {
        let health = probe_health_at(&socket)?;
        if health.notifications || !notification_backend_available() {
            return Ok(());
        }
        let shutdown_health = parse_health_response(&request_at(&socket, "shutdown")?)?;
        if shutdown_health.active > 0 {
            spawn_worker_replacement()?;
            return Ok(());
        }
        wait_for_socket_to_close(&socket, DAEMON_TRANSITION_TIMEOUT)?;
    }
    let executable = std::env::current_exe()
        .map_err(|error| format!("resolve Prism worker executable: {error}"))?;
    let mut command = Command::new(executable);
    command.args(["worker", "serve"]);
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
            Ok(DaemonHealth {
                state: DaemonState::Running,
                ..
            }) => return Ok(()),
            Ok(health) => {
                last_error = format!("worker did not become ready: state={:?}", health.state)
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

pub fn wake() -> Result<(), String> {
    request("wake").map(|_| ())
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
fn notification_subscription_loop(socket: &WorkerEndpoint, stop: &AtomicBool) {
    notification_subscription_loop_with_delivery(socket, stop, |title, body| {
        crate::desktop_notification::deliver_terminal_notification(title, body)
    });
}

#[cfg(any(target_os = "macos", all(test, unix)))]
fn notification_subscription_loop_with_delivery(
    socket: &WorkerEndpoint,
    stop: &AtomicBool,
    mut deliver: impl FnMut(&str, &str) -> Result<(), &'static str>,
) {
    while !stop.load(Ordering::Acquire) {
        if let Ok(mut stream) = socket.connect() {
            let _ = worker_ipc::set_read_timeout(&stream, Duration::from_millis(250));
            let secret = worker_ipc::read_secret(socket).unwrap_or_default();
            let request = authenticated_command(&secret, "subscribe-notifications");
            if stream.write_all(format!("{request}\n").as_bytes()).is_ok() {
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
    if !response.starts_with(&format!("ok {PROTOCOL_VERSION} ")) {
        return Err(format!("Prism worker rejected shutdown: {response}"));
    }
    wait_for_socket_to_close(&socket, DAEMON_TRANSITION_TIMEOUT)
}

fn wait_for_socket_to_close(endpoint: &WorkerEndpoint, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        if !endpoint
            .address_exists()
            .map_err(|error| format!("inspect Prism worker endpoint: {error}"))?
        {
            return Ok(());
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

pub fn launch_bundled_plan(
    launch: crate::workflow::bundled::BundledPlanLaunch,
) -> Result<String, String> {
    launch_bundled("bundled_plan_launch", launch)
}

pub fn launch_bundled_coding(
    launch: crate::workflow::bundled::BundledCodingLaunch,
) -> Result<String, String> {
    launch_bundled("bundled_coding_launch", launch)
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

fn launch_bundled<T: serde::Serialize>(kind: &str, launch: T) -> Result<String, String> {
    ensure_running()?;
    let response = workflow_request(serde_json::json!({"type": kind, "launch": launch}))?;
    response["run_id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "workflow worker omitted run id".to_string())
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

pub fn command_workflow(run_id: &str, command: crate::WorkflowCommand) -> Result<(), String> {
    ensure_running()?;
    workflow_request(serde_json::json!({
        "type": "workflow_command",
        "run_id": run_id,
        "command": command,
        "now_unix_ms": execution::now_ms(),
    }))?;
    Ok(())
}

fn request_at(endpoint: &WorkerEndpoint, command: &str) -> Result<String, String> {
    let stream = endpoint
        .connect()
        .map_err(|error| format!("connect to Prism worker: {error}"))?;
    let secret = worker_ipc::read_secret(endpoint)?;
    request_on_stream(stream, &authenticated_command(&secret, command))
}

fn authenticated_command(secret: &str, command: &str) -> String {
    format!("auth {secret} {command}")
}

fn authenticate_command<'a>(secret: &str, command: &'a str) -> Option<&'a str> {
    command.strip_prefix(&format!("auth {secret} "))
}

fn request_on_stream(stream: WorkerStream, command: &str) -> Result<String, String> {
    request_on_stream_raw(stream, command).map_err(format_request_error)
}

fn request_on_stream_raw(
    mut stream: WorkerStream,
    command: &str,
) -> Result<String, (&'static str, std::io::Error)> {
    worker_ipc::set_read_timeout(&stream, Duration::from_secs(1))
        .map_err(|error| ("configure Prism worker endpoint", error))?;
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
            execution::new_instance_id("workflow-worker"),
            crate::workflow::engine::WorkerConfig::default(),
        )
        .await
        .map_err(|error| format!("open workflow control plane: {error}"))?;
        worker
            .register_builtins()
            .map_err(|error| format!("register workflow implementations: {error}"))?;
        let operations = worker.operations();
        crate::workflow::bundled::install(&operations)
            .await
            .map_err(|error| format!("install bundled workflow definitions: {error}"))?;
        import_legacy_repositories(&operations).await?;
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
        let socket =
            tokio::task::spawn_blocking(move || serve_socket(&socket_failure, &operations));
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

async fn import_legacy_repositories(operations: &crate::WorkflowOperations) -> Result<(), String> {
    const IMPORTER_REVISION: &str = "workflow-ledger-v1";
    let now_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0);
    for entry in crate::workspace::load_entries()? {
        let repository = crate::repo::Repository { root: entry.root };
        let source = repository.prism_dir().join("prism.db");
        if !source.exists() {
            continue;
        }
        operations
            .import_legacy_repository(&source, IMPORTER_REVISION, now_unix_ms)
            .await
            .map_err(|error| {
                format!(
                    "import legacy workflow history from {} before worker startup: {error}",
                    source.display()
                )
            })?;
    }
    Ok(())
}

fn serve_socket(
    control_plane_failure: &Arc<Mutex<Option<String>>>,
    operations: &crate::WorkflowOperations,
) -> Result<(), String> {
    let runtime = runtime_dir();
    let endpoint = WorkerEndpoint::for_runtime(&runtime)?;
    worker_ipc::prepare_runtime(&runtime)?;
    let _lock = acquire_lock(&runtime.join("worker.lock"))?;
    match endpoint.connect() {
        Ok(_) => {
            return Err("a live Prism worker endpoint already owns the runtime".to_string());
        }
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
    let secret = worker_ipc::create_secret(&endpoint)?;

    let instance_id = execution::new_instance_id("daemon");
    classify_abandoned(&instance_id)?;
    log_daemon_lifecycle("daemon_start", &instance_id);
    let listener = endpoint
        .bind()
        .map_err(|error| format!("bind Prism worker endpoint {}: {error}", endpoint.display()))?;
    worker_ipc::secure_listener(&endpoint)?;
    worker_ipc::set_listener_nonblocking(&listener)
        .map_err(|error| format!("configure Prism worker listener: {error}"))?;

    let active = Arc::new(Mutex::new(BTreeSet::<PathBuf>::new()));
    let notification_subscriber = Arc::new(Mutex::new(Vec::<WorkerStream>::new()));
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
        match worker_ipc::accept(&listener) {
            Ok(mut stream) => {
                if respond(
                    &mut stream,
                    &secret,
                    &instance_id,
                    &active,
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
        if draining
            && active
                .lock()
                .map(|active| active.is_empty())
                .unwrap_or(false)
        {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    notification_stop.store(true, Ordering::Release);
    log_daemon_lifecycle("daemon_stop", &instance_id);
    endpoint
        .remove_stale_address()
        .map_err(|error| format!("remove worker endpoint: {error}"))?;
    let _ = fs::remove_file(endpoint.secret_path());
    Ok(())
}

fn respond(
    stream: &mut WorkerStream,
    secret: &str,
    instance_id: &str,
    active: &Arc<Mutex<BTreeSet<PathBuf>>>,
    notification_subscriber: &Arc<Mutex<Vec<WorkerStream>>>,
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
    let Some(command) = authenticate_command(secret, command) else {
        let _ = stream.write_all(b"error authentication-failed\n");
        return false;
    };
    let active = active
        .lock()
        .map(|active| active.len())
        .unwrap_or(usize::MAX);
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
                "ok {PROTOCOL_VERSION} {instance_id} pid={} state={} active={active} notifications={}\n",
                std::process::id(),
                if draining { "draining" } else { "running" },
                u8::from(notification_backend_available()),
            ),
            "shutdown" => format!(
                "ok {PROTOCOL_VERSION} {instance_id} pid={} state=draining active={active} notifications={}\n",
                std::process::id(),
                u8::from(notification_backend_available()),
            ),
            "subscribe-notifications" if !draining => match worker_ipc::try_clone_stream(stream) {
                Ok(subscriber) => {
                    let _ = worker_ipc::set_read_timeout(&subscriber, Duration::from_secs(1));
                    let _ = worker_ipc::set_write_timeout(&subscriber, Duration::from_secs(1));
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
        steps: Vec<SocketStep>,
    },
    BundledPlanLaunch {
        launch: crate::workflow::bundled::BundledPlanLaunch,
    },
    BundledCodingLaunch {
        launch: crate::workflow::bundled::BundledCodingLaunch,
    },
    WorkflowList {
        repository: Option<String>,
        limit: usize,
    },
    WorkflowInspect {
        run_id: String,
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
    WorkflowImportLegacy {
        source_path: PathBuf,
        importer_revision: String,
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
    now_unix_ms: i64,
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
            WorkflowSocketRequest::WorkflowLaunch { run, steps } => operations
                .launch_materialized(
                    crate::LaunchWorkflow {
                        run_id: &run.run_id,
                        definition_snapshot_id: &run.definition_snapshot_id,
                        repository: run.repository.as_deref(),
                        idempotency_key: &run.idempotency_key,
                        now_unix_ms: run.now_unix_ms,
                    },
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
                .map(|run_id| serde_json::json!({"ok": true, "run_id": run_id})),
            WorkflowSocketRequest::BundledPlanLaunch { launch } => {
                crate::workflow::bundled::launch_plan(operations, launch)
                    .await
                    .map(|run_id| serde_json::json!({"ok": true, "run_id": run_id}))
            }
            WorkflowSocketRequest::BundledCodingLaunch { launch } => {
                crate::workflow::bundled::launch_coding(operations, launch)
                    .await
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
            WorkflowSocketRequest::WorkflowImportLegacy {
                source_path,
                importer_revision,
                now_unix_ms,
            } => operations
                .import_legacy_repository(&source_path, &importer_revision, now_unix_ms)
                .await
                .map(|summary| {
                    serde_json::json!({
                        "ok": true,
                        "imported": summary.imported,
                        "already_imported": summary.already_imported,
                    })
                }),
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

fn notification_loop(stop: Arc<AtomicBool>, subscriber: Arc<Mutex<Vec<WorkerStream>>>) {
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
    subscriber: &Arc<Mutex<Vec<WorkerStream>>>,
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
    subscriber: &Arc<Mutex<Vec<WorkerStream>>>,
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

#[cfg_attr(windows, allow(dead_code))]
enum DeliveryOutcome {
    Accepted,
    Retry(&'static str),
    #[cfg(target_os = "macos")]
    Uncertain(&'static str),
}

#[cfg(target_os = "linux")]
fn deliver_worker_notification(
    notification: &PendingNotification,
    _subscriber: &Arc<Mutex<Vec<WorkerStream>>>,
) -> DeliveryOutcome {
    match crate::desktop_notification::deliver_native_notification(
        &notification.title,
        &notification.body,
    ) {
        Ok(()) => DeliveryOutcome::Accepted,
        Err(category) => DeliveryOutcome::Retry(category),
    }
}

#[cfg(windows)]
fn deliver_worker_notification(
    _notification: &PendingNotification,
    _subscriber: &Arc<Mutex<Vec<WorkerStream>>>,
) -> DeliveryOutcome {
    // Native Windows toast attribution is a phase 6 capability. Notification absence is
    // explicitly non-fatal and pending rows are consumed instead of causing daemon churn.
    DeliveryOutcome::Accepted
}

#[cfg(target_os = "macos")]
fn deliver_worker_notification(
    notification: &PendingNotification,
    subscriber: &Arc<Mutex<Vec<WorkerStream>>>,
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
fn read_notification_ack(stream: &mut WorkerStream) -> Result<String, std::io::Error> {
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

fn classify_abandoned(instance_id: &str) -> Result<(), String> {
    for entry in workspace::discover_valid_entries(workspace::load_entries()?) {
        observability::attach_run_repo(&entry.repo)?;
        execution::mark_abandoned(&observability::db_path(&entry.repo), instance_id).map(|_| ())?;
    }
    Ok(())
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
    let repo_path = repo.root.display().to_string();
    let _ = execution::persistence::WorkflowStore::open(&observability::db_path(repo)).and_then(
        |store| {
            store.insert_worker_event(execution::persistence::WorkerEvent {
                time: execution::now_ms(),
                action,
                repo: &repo_path,
                message,
                data_json,
            })
        },
    );
}

fn acquire_lock(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    let file = options
        .open(path)
        .map_err(|error| format!("open Prism worker lock: {error}"))?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("secure Prism worker lock: {error}"))?;
    #[cfg(windows)]
    crate::system::windows_security::secure_path(path, false)
        .map_err(|error| format!("secure Prism worker lock: {error}"))?;
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
    let override_path = std::env::var_os("PRISM_RUNTIME_DIR").filter(|path| !path.is_empty());
    let xdg_runtime = std::env::var_os("XDG_RUNTIME_DIR").filter(|path| !path.is_empty());
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty());
    let local_app_data = std::env::var_os("LOCALAPPDATA").filter(|path| !path.is_empty());
    runtime_dir_for(
        crate::platform::current_os(),
        override_path.as_deref(),
        xdg_runtime.as_deref(),
        home.as_deref(),
        local_app_data.as_deref(),
        &crate::util::prism_config_dir(),
    )
}

fn runtime_dir_for(
    os: SupportedOs,
    override_path: Option<&std::ffi::OsStr>,
    xdg_runtime: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
    local_app_data: Option<&std::ffi::OsStr>,
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
    if os == SupportedOs::Windows
        && let Some(local_app_data) = local_app_data
    {
        return PathBuf::from(local_app_data).join("Prism").join("runtime");
    }
    fallback_config.join("runtime")
}

pub fn socket_path() -> PathBuf {
    #[cfg(unix)]
    return runtime_dir().join("worker.sock");
    #[cfg(windows)]
    return runtime_dir().join("worker.endpoint");
}

fn validated_socket_path() -> Result<WorkerEndpoint, String> {
    WorkerEndpoint::for_runtime(&runtime_dir())
}

#[cfg(test)]
mod authentication_tests {
    use super::{authenticate_command, authenticated_command};

    #[test]
    fn worker_commands_require_the_exact_run_secret() {
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
    fn windows_worker_lock_is_exclusive_released_and_permanent() {
        let runtime = std::env::temp_dir().join(format!(
            "prism-windows-worker-lock-{}-{}",
            std::process::id(),
            crate::util::timestamp_nanos()
        ));
        std::fs::create_dir_all(&runtime).unwrap();
        let path = runtime.join("worker.lock");
        let first = super::acquire_lock(&path).unwrap();
        assert!(super::acquire_lock(&path).is_err());
        drop(first);
        let second = super::acquire_lock(&path).unwrap();
        assert!(path.exists());
        drop(second);
        std::fs::remove_dir_all(runtime).unwrap();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::net::{UnixListener, UnixStream};

    type WorkerSocketPath = WorkerEndpoint;
    const SOCKET_PATH_BUDGET: usize = worker_ipc::UNIX_SOCKET_PATH_BUDGET;

    fn socket_path_bytes(path: &Path) -> &[u8] {
        path.as_os_str().as_bytes()
    }

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
        let active = Arc::new(Mutex::new(BTreeSet::new()));
        let subscriber = Arc::new(Mutex::new(Vec::new()));
        client
            .write_all(b"auth test-secret subscribe-notifications\n")
            .unwrap();

        assert!(!respond(
            &mut server,
            "test-secret",
            "daemon-test",
            &active,
            &subscriber,
            None,
            false,
        ));
        let mut acknowledgement = [0_u8; 64];
        let size = client.read(&mut acknowledgement).unwrap();
        assert_eq!(
            std::str::from_utf8(&acknowledgement[..size]).unwrap(),
            "ok 1 subscribed\n"
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
        let secret = worker_ipc::create_secret(&socket).unwrap();
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
        let mut request = [0_u8; 128];
        let size = rejected.read(&mut request).unwrap();
        assert_eq!(
            std::str::from_utf8(&request[..size]).unwrap(),
            format!("auth {secret} subscribe-notifications\n")
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
    fn platform_contract_runtime_paths_cover_all_supported_hosts() {
        assert_eq!(
            runtime_dir_for(
                SupportedOs::Linux,
                None,
                Some(OsStr::new("/run/user/1000")),
                Some(OsStr::new("/home/user")),
                None,
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
                None,
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
                None,
                Path::new("/fallback"),
            ),
            PathBuf::from("/override")
        );
        assert_eq!(
            runtime_dir_for(
                SupportedOs::Windows,
                None,
                None,
                None,
                Some(OsStr::new("C:/Users/test/AppData/Local")),
                Path::new("C:/fallback"),
            ),
            PathBuf::from("C:/Users/test/AppData/Local/Prism/runtime")
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
                None,
                Path::new("/fallback"),
            ),
            runtime_dir_for(
                SupportedOs::Linux,
                None,
                Some(long),
                None,
                None,
                Path::new("/fallback"),
            ),
            runtime_dir_for(
                SupportedOs::MacOs,
                None,
                None,
                Some(long),
                None,
                Path::new("/fallback"),
            ),
            runtime_dir_for(SupportedOs::Linux, None, None, None, None, Path::new(long)),
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
    fn waiting_for_a_draining_daemon_times_out() {
        assert_eq!(
            wait_for_existing_daemon(Duration::ZERO, || Ok(DaemonHealth {
                state: DaemonState::Draining,
                protocol_version: Some(PROTOCOL_VERSION),
                instance_id: Some("test".to_string()),
                pid: Some(std::process::id()),
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

    fn runtime_with_socket_path_len(byte_len: usize) -> PathBuf {
        use std::os::unix::ffi::OsStringExt;

        const SOCKET_SUFFIX_LEN: usize = b"/worker.sock".len();
        let mut bytes = vec![b'/'];
        bytes.extend(std::iter::repeat_n(b'a', byte_len - SOCKET_SUFFIX_LEN - 1));
        PathBuf::from(std::ffi::OsString::from_vec(bytes))
    }
}
