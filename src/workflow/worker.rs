use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
#[cfg(any(target_os = "macos", test))]
use std::io::{BufRead, BufReader};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::execution::{self, DispatchState, ExecutionClaim, WorkflowIdentity, WorkflowKind};
use crate::notification::{NotificationCoordinator, NotificationObservation, PendingNotification};
use crate::platform::SupportedOs;
use crate::process::DetachedProcessPolicy;
use crate::repo::Repository;
use crate::util::stable_hash;
use crate::{observability, workspace};

const PROTOCOL_VERSION: u32 = 1;
const POLL_INTERVAL: Duration = Duration::from_secs(1);
const NOTIFICATION_POLL_INTERVAL: Duration = Duration::from_secs(5);
const NOTIFICATION_RETRY_INTERVAL: Duration = Duration::from_secs(10);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const DAEMON_TRANSITION_TIMEOUT: Duration = Duration::from_secs(3);
const GLOBAL_CONCURRENCY: usize = 4;
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
    pub executable_identity: Option<String>,
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
            executable_identity: None,
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
    let response = request_on_stream(stream, "health")?;
    if response.is_empty() {
        return Ok(DaemonHealth::stopped());
    }
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
    let mut executable_identity = None;
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
        } else if let Some(value) = field.strip_prefix("exe=") {
            executable_identity = Some(value.to_string());
        } else if let Some(value) = field.strip_prefix("notifications=") {
            notifications = value == "1";
        }
    }
    Ok(DaemonHealth {
        state: state.ok_or_else(|| format!("missing Prism daemon state: {response}"))?,
        protocol_version: Some(version),
        instance_id: Some(instance_id.to_string()),
        pid: Some(pid.ok_or_else(|| format!("missing Prism daemon PID: {response}"))?),
        executable_identity,
        active: active.ok_or_else(|| format!("missing Prism daemon active count: {response}"))?,
        notifications,
    })
}

pub fn ensure_running() -> Result<(), String> {
    let socket = validated_socket_path()?;
    let executable_identity = current_executable_identity()?;
    if std::env::var_os("PRISM_WAIT_FOR_WORKER_DRAIN").is_some() {
        loop {
            match probe_health_at(&socket)? {
                DaemonHealth {
                    state: DaemonState::Stopped,
                    ..
                } => break,
                health @ DaemonHealth {
                    state: DaemonState::Running,
                    notifications: true,
                    ..
                } if daemon_uses_executable(&health, &executable_identity) => return Ok(()),
                _ => thread::sleep(Duration::from_millis(250)),
            }
        }
    }
    let mut health = probe_health_at(&socket)?;
    loop {
        match health {
            current @ DaemonHealth {
                state: DaemonState::Running,
                ..
            } if daemon_uses_executable(&current, &executable_identity)
                && current.notifications =>
            {
                return Ok(());
            }
            DaemonHealth {
                state: DaemonState::Running,
                ..
            } => {
                let response = request_at(&socket, "replace")?;
                if response.starts_with(&format!("ok {PROTOCOL_VERSION} ")) {
                    return wait_for_replacement(
                        &socket,
                        &executable_identity,
                        DAEMON_TRANSITION_TIMEOUT,
                    );
                }
                let shutdown_health = parse_health_response(&request_at(&socket, "shutdown")?)?;
                if shutdown_health.active > 0 {
                    spawn_worker_replacement()?;
                    return Ok(());
                }
                wait_for_socket_to_close(&socket, DAEMON_TRANSITION_TIMEOUT)?;
                break;
            }
            DaemonHealth {
                state: DaemonState::Draining,
                ..
            } => {
                health = wait_for_drain_transition(DAEMON_TRANSITION_TIMEOUT, || {
                    probe_health_at(&socket)
                })?;
            }
            DaemonHealth {
                state: DaemonState::Stopped,
                ..
            } => break,
        }
    }
    let executable = std::env::current_exe()
        .map_err(|error| format!("resolve Prism worker executable: {error}"))?;
    start_worker(installed_executable_path(&executable))?;

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

fn start_worker(executable: PathBuf) -> Result<(), String> {
    let mut command = Command::new(executable);
    command.args(["worker", "serve"]);
    crate::process::spawn_detached_named(
        &mut command,
        DetachedProcessPolicy::WorkerDaemon,
        crate::process::ProcessDescriptor::new("prism.worker.serve"),
    )
    .map_err(|error| format!("start Prism worker daemon: {error}"))?;
    Ok(())
}

fn daemon_uses_executable(health: &DaemonHealth, executable_identity: &str) -> bool {
    health.executable_identity.as_deref() == Some(executable_identity)
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

fn wait_for_drain_transition(
    timeout: Duration,
    mut probe: impl FnMut() -> Result<DaemonHealth, String>,
) -> Result<DaemonHealth, String> {
    let deadline = Instant::now() + timeout;
    loop {
        let health = probe()?;
        if health.state != DaemonState::Draining {
            return Ok(health);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for Prism worker daemon to finish draining ({} active)",
                health.active
            ));
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn wait_for_replacement(
    socket: &WorkerSocketPath,
    executable_identity: &str,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let mut active = 0;
    while Instant::now() < deadline {
        match probe_health_at(socket) {
            Ok(health) if daemon_uses_executable(&health, executable_identity) => return Ok(()),
            Ok(health) => active = health.active,
            Err(_) => {}
        }
        thread::sleep(Duration::from_millis(25));
    }
    Err(format!(
        "Prism worker is draining before replacement ({active} active)"
    ))
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
    if !response.starts_with(&format!("ok {PROTOCOL_VERSION} ")) {
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

fn request_at(path: &WorkerSocketPath, command: &str) -> Result<String, String> {
    let stream = UnixStream::connect(path.as_path())
        .map_err(|error| format!("connect to Prism worker: {error}"))?;
    request_on_stream(stream, command)
}

fn request_on_stream(mut stream: UnixStream, command: &str) -> Result<String, String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .map_err(|error| format!("configure Prism worker socket: {error}"))?;
    stream
        .write_all(format!("{command}\n").as_bytes())
        .map_err(|error| format!("write Prism worker request: {error}"))?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|error| format!("read Prism worker response: {error}"))?;
    Ok(response.trim().to_string())
}

pub fn serve() -> Result<(), String> {
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
    let worker_lock = acquire_lock(&runtime.join("worker.lock"))?;
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

    let instance_id = execution::new_instance_id("daemon");
    let executable = std::env::current_exe()
        .map_err(|error| format!("resolve Prism worker executable: {error}"))?;
    let executable = installed_executable_path(&executable);
    let started_executable_identity = executable_identity(&executable)?;
    classify_abandoned(&instance_id)?;
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

    let active = Arc::new(Mutex::new(BTreeSet::<PathBuf>::new()));
    let notification_subscriber = Arc::new(Mutex::new(Vec::<UnixStream>::new()));
    let notification_stop = Arc::new(AtomicBool::new(false));
    let observer_stop = Arc::clone(&notification_stop);
    let observer_subscriber = Arc::clone(&notification_subscriber);
    thread::Builder::new()
        .name("prism-notification-observer".to_string())
        .spawn(move || notification_loop(observer_stop, observer_subscriber))
        .map_err(|error| format!("start notification observer: {error}"))?;
    let mut next_poll = Instant::now();
    let mut draining = false;
    let mut restart_after_drain = false;
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let control = respond(
                    &mut stream,
                    &instance_id,
                    &started_executable_identity,
                    &active,
                    &notification_subscriber,
                    draining,
                );
                if matches!(control, WorkerControl::Drain | WorkerControl::Replace) {
                    notification_stop.store(true, Ordering::Release);
                }
                control.apply(&mut next_poll, &mut draining, &mut restart_after_drain);
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
        if !draining && Instant::now() >= next_poll {
            if executable_identity(&executable)
                .is_ok_and(|identity| identity != started_executable_identity)
            {
                draining = true;
                restart_after_drain = true;
                continue;
            }
            schedule_queued(&instance_id, Arc::clone(&active));
            next_poll = Instant::now() + POLL_INTERVAL;
        }
        thread::sleep(Duration::from_millis(50));
    }
    notification_stop.store(true, Ordering::Release);
    log_daemon_lifecycle("daemon_stop", &instance_id);
    fs::remove_file(socket.as_path()).map_err(|error| format!("remove worker socket: {error}"))?;
    drop(listener);
    drop(worker_lock);
    if restart_after_drain {
        start_worker(executable)?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkerControl {
    Continue,
    Wake,
    Drain,
    Replace,
}

impl WorkerControl {
    fn apply(self, next_poll: &mut Instant, draining: &mut bool, restart_after_drain: &mut bool) {
        match self {
            Self::Continue => {}
            Self::Wake => *next_poll = Instant::now(),
            Self::Drain => *draining = true,
            Self::Replace => {
                *draining = true;
                *restart_after_drain = true;
            }
        }
    }
}

fn respond(
    stream: &mut UnixStream,
    instance_id: &str,
    executable_identity: &str,
    active: &Arc<Mutex<BTreeSet<PathBuf>>>,
    notification_subscriber: &Arc<Mutex<Vec<UnixStream>>>,
    draining: bool,
) -> WorkerControl {
    let mut request = [0_u8; 64];
    let size = stream.read(&mut request).unwrap_or(0);
    let command = String::from_utf8_lossy(&request[..size]);
    let active = active
        .lock()
        .map(|active| active.len())
        .unwrap_or(usize::MAX);
    let mut new_notification_subscriber = None;
    let response = match command.trim() {
        "health" | "wake" => format!(
            "ok {PROTOCOL_VERSION} {instance_id} pid={} state={} active={active} exe={executable_identity} notifications=1\n",
            std::process::id(),
            if draining { "draining" } else { "running" }
        ),
        "shutdown" => format!(
            "ok {PROTOCOL_VERSION} {instance_id} pid={} state=draining active={active} exe={executable_identity} notifications=1\n",
            std::process::id()
        ),
        "replace" => format!(
            "ok {PROTOCOL_VERSION} {instance_id} pid={} state=draining active={active} exe={executable_identity} notifications=1\n",
            std::process::id()
        ),
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
    };
    if stream.write_all(response.as_bytes()).is_ok()
        && let Some(subscriber) = new_notification_subscriber
        && let Ok(mut current) = notification_subscriber.lock()
    {
        current.push(subscriber);
    }
    match command.trim() {
        "wake" if !draining => WorkerControl::Wake,
        "shutdown" => WorkerControl::Drain,
        "replace" => WorkerControl::Replace,
        _ => WorkerControl::Continue,
    }
}

fn current_executable_identity() -> Result<String, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("resolve Prism executable identity: {error}"))?;
    executable_identity(&executable)
}

fn executable_identity(executable: &Path) -> Result<String, String> {
    let executable = installed_executable_path(executable);
    let metadata = fs::metadata(&executable).map_err(|error| {
        format!(
            "inspect Prism executable identity {}: {error}",
            executable.display()
        )
    })?;
    Ok(format!("{}:{}", metadata.dev(), metadata.ino()))
}

fn installed_executable_path(executable: &Path) -> PathBuf {
    if executable.exists() {
        return executable.to_path_buf();
    }
    #[cfg(target_os = "linux")]
    if let Some(path) = executable
        .as_os_str()
        .as_bytes()
        .strip_suffix(b" (deleted)")
    {
        return PathBuf::from(std::ffi::OsStr::from_bytes(path));
    }
    executable.to_path_buf()
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
        let result = observability::with_writable_db_mut(&repo, |conn| {
            let mut coordinator = NotificationCoordinator::new(conn);
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
            dispatch_pending_notifications(&mut coordinator, subscriber, observed_unix_ms)
        });
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
        return match crate::opencode::poll_status_authoritative(&runtime) {
            Ok(status) => Ok(Some(normalize_interactive_state(
                running,
                Some(status.state.agent_state()),
            ))),
            Err(_) if running => Ok(Some(crate::agent::AgentState::Running)),
            Err(error) => Err(error),
        };
    }
    Ok(generation.map(|_| normalize_interactive_state(running, None)))
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
    coordinator: &mut NotificationCoordinator<'_>,
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

fn classify_abandoned(instance_id: &str) -> Result<(), String> {
    for entry in workspace::discover_valid_entries(workspace::load_entries()?) {
        observability::attach_run_repo(&entry.repo)?;
        observability::with_writable_db(&entry.repo, |conn| {
            execution::mark_abandoned(conn, instance_id).map(|_| ())
        })?;
    }
    Ok(())
}

fn schedule_queued(instance_id: &str, active: Arc<Mutex<BTreeSet<PathBuf>>>) {
    let active_count = active
        .lock()
        .map(|active| active.len())
        .unwrap_or(usize::MAX);
    if active_count >= GLOBAL_CONCURRENCY {
        return;
    }
    let entries = match workspace::load_entries() {
        Ok(entries) => entries,
        Err(error) => {
            eprintln!("Prism worker cannot load repositories: {error}");
            return;
        }
    };
    for entry in workspace::discover_valid_entries(entries) {
        let repo = entry.repo;
        if let Err(error) = observability::attach_run_repo(&repo) {
            eprintln!(
                "Prism worker cannot attach repository {}: {error}",
                repo.root.display()
            );
            continue;
        }
        let _ = observability::with_writable_db(&repo, |conn| {
            execution::mark_abandoned(conn, instance_id).map(|_| ())
        });
        let queued = observability::with_writable_db(&repo, |conn| execution::queued(conn, 16));
        let Ok(queued) = queued else {
            continue;
        };
        for workflow in queued {
            if active
                .lock()
                .map(|active| active.len())
                .unwrap_or(usize::MAX)
                >= GLOBAL_CONCURRENCY
            {
                return;
            }
            let Ok(worktree) = workflow_worktree(&repo, &workflow) else {
                continue;
            };
            let config = Config::load(&repo);
            if !matches!(legacy_worker_running(&repo, &config, &workflow), Ok(false)) {
                continue;
            }
            let inserted = active
                .lock()
                .map(|mut active| active.insert(worktree.clone()))
                .unwrap_or(false);
            if !inserted {
                continue;
            }
            let worker_id = execution::new_instance_id("executor");
            let claim = observability::with_writable_db_mut(&repo, |conn| {
                execution::claim(conn, &workflow, instance_id, &worker_id)
            });
            let Ok(Some(claim)) = claim else {
                if let Ok(mut active) = active.lock() {
                    active.remove(&worktree);
                }
                continue;
            };
            log_claim_lifecycle(&repo, "claim", &claim, "workflow claimed");
            let active = Arc::clone(&active);
            let executor_repo = repo.clone();
            thread::spawn(move || {
                execute_claim(&executor_repo, &claim);
                if let Ok(mut active) = active.lock() {
                    active.remove(&worktree);
                }
            });
        }
    }
}

pub fn legacy_worker_running(
    repo: &Repository,
    config: &Config,
    workflow: &WorkflowIdentity,
) -> Result<bool, String> {
    let expected = format!(
        "prism-{:016x}-worker-{}-{:016x}",
        stable_hash(&repo.root),
        workflow.kind.label(),
        stable_hash(Path::new(&workflow.run_id))
    );
    crate::tmux::named_session_exists(config, &expected)
        .map_err(|error| format!("inspect legacy tmux workers: {error}"))
}

fn workflow_worktree(repo: &Repository, workflow: &WorkflowIdentity) -> Result<PathBuf, String> {
    observability::with_writable_db(repo, |conn| {
        let (table, column) = match workflow.kind {
            WorkflowKind::Auto => ("auto_run", "worktree_path"),
            WorkflowKind::Plan => ("plan_run", "scope_path"),
        };
        conn.query_row(
            &format!("select {column} from {table} where id = ?1"),
            [&workflow.run_id],
            |row| row.get::<_, String>(0),
        )
        .map(PathBuf::from)
        .map_err(|error| format!("load workflow worktree: {error}"))
    })
}

fn execute_claim(repo: &Repository, claim: &ExecutionClaim) {
    log_claim_lifecycle(repo, "executor_start", claim, "workflow executor started");
    let heartbeat_stop = Arc::new(AtomicBool::new(false));
    let ownership_lost = Arc::new(AtomicBool::new(false));
    let heartbeat = spawn_heartbeat(
        repo.clone(),
        claim.clone(),
        Arc::clone(&heartbeat_stop),
        Arc::clone(&ownership_lost),
    );
    let config = Config::load(repo);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        observability::with_writable_db(repo, |conn| execution::validate_claim(conn, claim))
            .and_then(|()| match claim.workflow.kind {
                WorkflowKind::Auto => execute_auto(repo, &config, claim),
                WorkflowKind::Plan => execute_plan(repo, &config, claim),
            })
    }))
    .unwrap_or_else(|_| Err("workflow executor panicked".to_string()));
    heartbeat_stop.store(true, Ordering::Release);
    let _ = heartbeat.join();

    let state = match result {
        Ok(()) => workflow_release_state(repo, &claim.workflow).unwrap_or(DispatchState::Terminal),
        Err(error) => {
            if !ownership_lost.load(Ordering::Acquire) {
                mark_domain_failed(repo, claim, &error);
            }
            DispatchState::Terminal
        }
    };
    match observability::with_writable_db(repo, |conn| execution::release(conn, claim, state)) {
        Ok(()) => log_claim_lifecycle(repo, "release", claim, state.label()),
        Err(error) => log_claim_lifecycle(repo, "release_failed", claim, &error),
    }
    log_claim_lifecycle(repo, "executor_stop", claim, "workflow executor stopped");
}

fn spawn_heartbeat(
    repo: Repository,
    claim: ExecutionClaim,
    stop: Arc<AtomicBool>,
    ownership_lost: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut next_heartbeat = Instant::now() + HEARTBEAT_INTERVAL;
        while !stop.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(100));
            if stop.load(Ordering::Acquire) {
                break;
            }
            if Instant::now() < next_heartbeat {
                continue;
            }
            if observability::with_writable_db(&repo, |conn| execution::heartbeat(conn, &claim))
                .is_err()
            {
                let validation = observability::with_writable_db(&repo, |conn| {
                    execution::validate_claim(conn, &claim)
                });
                if matches!(
                    validation,
                    Err(ref error) if execution::is_stale_claim_error(error)
                ) {
                    ownership_lost.store(true, Ordering::Release);
                    log_claim_lifecycle(
                        &repo,
                        "heartbeat_lost",
                        &claim,
                        "execution ownership lost",
                    );
                    break;
                }
            }
            next_heartbeat = Instant::now() + HEARTBEAT_INTERVAL;
        }
    })
}

fn execute_auto(repo: &Repository, config: &Config, claim: &ExecutionClaim) -> Result<(), String> {
    let run_id = &claim.workflow.run_id;
    let mut persisted = observability::with_writable_db(repo, |conn| {
        crate::auto_flow::load_auto_run(conn, run_id)
    })?
    .ok_or_else(|| format!("auto flow run not found: {run_id}"))?;
    let harness_config = config
        .harness_config(&persisted.run.harness_id)
        .map_err(|_| {
            format!(
                "auto run harness '{}' is no longer configured",
                persisted.run.harness_id
            )
        })?;
    if harness_config.adapter != persisted.run.adapter_id {
        return Err(format!(
            "auto run harness '{}' was recorded with adapter '{}', but it is now configured as '{}'",
            persisted.run.harness_id, persisted.run.adapter_id, harness_config.adapter
        ));
    }
    let runtime = crate::harness::Harness::new(&persisted.run.harness_id, &harness_config)
        .prepare_server(
            repo,
            config,
            &persisted.run.branch,
            &persisted.run.worktree_path,
        )?
        .map(|runtime| runtime.server_url);
    let executor = crate::auto_flow::AutoExecutorConfig::for_harness(
        persisted.run.harness_id.clone(),
        harness_config,
        runtime,
        persisted.run.worktree_path.clone(),
        format!("Auto Flow {}", persisted.run.prompt_summary),
    );
    observability::with_writable_db(repo, |conn| {
        execution::install_claim_guards(conn, claim)?;
        crate::auto_flow::execute_auto_initial_step(
            conn,
            repo,
            config,
            &mut persisted,
            &executor,
            &mut std::io::sink(),
        )
    })
}

fn execute_plan(repo: &Repository, config: &Config, claim: &ExecutionClaim) -> Result<(), String> {
    let run_id = &claim.workflow.run_id;
    let mut persisted =
        observability::with_writable_db(repo, |conn| crate::plan_run::load_plan_run(conn, run_id))?
            .ok_or_else(|| format!("plan run not found: {run_id}"))?;
    let harness_config = config
        .harness_config(&persisted.run.harness_id)
        .map_err(|_| {
            format!(
                "plan run harness '{}' is no longer configured",
                persisted.run.harness_id
            )
        })?;
    if harness_config.adapter != persisted.run.adapter_id {
        return Err(format!(
            "plan run harness '{}' was recorded with adapter '{}', but it is now configured as '{}'",
            persisted.run.harness_id, persisted.run.adapter_id, harness_config.adapter
        ));
    }
    let server_url = crate::harness::Harness::new(&persisted.run.harness_id, &harness_config)
        .prepare_server(repo, config, "plan", &persisted.run.scope_path)?
        .map(|runtime| runtime.server_url);
    let mut executor = crate::plan_run::PlanExecutorConfig::for_harness(
        persisted.run.harness_id.clone(),
        harness_config.clone(),
        server_url,
        persisted.run.scope_path.clone(),
        persisted.run.plan_display.clone(),
    );
    if harness_config.adapter == "opencode"
        && config.opencode_plan_plugin
        && let Ok(plugin) = crate::plan_run::prepare_plan_plugin_config(&repo.prism_dir())
    {
        executor = executor.with_plugin_config(plugin);
    }
    observability::with_writable_db(repo, |conn| {
        execution::install_claim_guards(conn, claim)?;
        match persisted.run.mode {
            crate::plan_run::PlanRunMode::Sequential => crate::plan_run::execute_plan_sequential(
                conn,
                &mut persisted,
                &executor,
                &mut std::io::sink(),
            ),
            crate::plan_run::PlanRunMode::Parallel => crate::plan_run::execute_plan_parallel(
                conn,
                &mut persisted,
                &executor,
                &mut std::io::sink(),
            ),
        }
    })
}

fn workflow_release_state(
    repo: &Repository,
    workflow: &WorkflowIdentity,
) -> Result<DispatchState, String> {
    observability::with_writable_db(repo, |conn| {
        let table = match workflow.kind {
            WorkflowKind::Auto => "auto_run",
            WorkflowKind::Plan => "plan_run",
        };
        let status = conn
            .query_row(
                &format!("select status from {table} where id = ?1"),
                [&workflow.run_id],
                |row| row.get::<_, String>(0),
            )
            .map_err(|error| format!("load completed workflow status: {error}"))?;
        Ok(if status == "paused" {
            DispatchState::Paused
        } else {
            DispatchState::Terminal
        })
    })
}

fn mark_domain_failed(repo: &Repository, claim: &ExecutionClaim, error: &str) {
    let _ = observability::with_writable_db(repo, |conn| {
        execution::install_claim_guards(conn, claim)?;
        match claim.workflow.kind {
            WorkflowKind::Auto => {
                if let Some(mut persisted) =
                    crate::auto_flow::load_auto_run(conn, &claim.workflow.run_id)?
                {
                    crate::auto_flow::fail_auto_run(conn, &mut persisted, error.to_string())?;
                }
            }
            WorkflowKind::Plan => {
                conn.execute(
                    "update plan_run set status = 'failed', updated_unix_ms = ?1
                     where id = ?2 and status not in ('aborted', 'done')",
                    rusqlite::params![execution::now_ms(), claim.workflow.run_id],
                )
                .map_err(|db_error| format!("mark plan run failed: {db_error}"))?;
            }
        }
        Ok(())
    });
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

fn log_claim_lifecycle(repo: &Repository, action: &str, claim: &ExecutionClaim, message: &str) {
    let data = format!(
        "{{\"workflow_kind\":\"{}\",\"run_id\":{},\"worker_id\":{},\"daemon_instance_id\":{},\"fencing_token\":{}}}",
        claim.workflow.kind.label(),
        serde_json::to_string(&claim.workflow.run_id).unwrap_or_else(|_| "null".to_string()),
        serde_json::to_string(&claim.worker_id).unwrap_or_else(|_| "null".to_string()),
        serde_json::to_string(&claim.daemon_instance_id).unwrap_or_else(|_| "null".to_string()),
        claim.fencing_token,
    );
    log_worker_event(repo, action, message, Some(&data));
}

fn log_worker_event(repo: &Repository, action: &str, message: &str, data_json: Option<&str>) {
    let _ = observability::with_writable_db(repo, |conn| {
        conn.execute(
            "insert into event (
               time_unix_ms, level, target, action, repo, message, data_json
             ) values (?1, 'info', 'worker', ?2, ?3, ?4, ?5)",
            rusqlite::params![
                execution::now_ms(),
                action,
                repo.root.display().to_string(),
                message,
                data_json,
            ],
        )
        .map(|_| ())
        .map_err(|error| format!("record worker lifecycle event: {error}"))
    });
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
    fn notification_subscription_keeps_a_worker_to_tui_stream() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let active = Arc::new(Mutex::new(BTreeSet::new()));
        let subscriber = Arc::new(Mutex::new(Vec::new()));
        client.write_all(b"subscribe-notifications\n").unwrap();

        assert_eq!(
            respond(
                &mut server,
                "daemon-test",
                "exe",
                &active,
                &subscriber,
                false,
            ),
            WorkerControl::Continue
        );
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
    fn same_version_worker_from_replaced_executable_is_not_reused() {
        let current =
            parse_health_response("ok 1 daemon pid=42 state=running active=0 exe=8:100").unwrap();
        let replaced =
            parse_health_response("ok 1 daemon pid=42 state=running active=0 exe=8:99").unwrap();
        let legacy = parse_health_response("ok 1 daemon pid=42 state=running active=0").unwrap();

        assert!(daemon_uses_executable(&current, "8:100"));
        assert!(!daemon_uses_executable(&replaced, "8:100"));
        assert!(!daemon_uses_executable(&legacy, "8:100"));
    }

    #[test]
    fn draining_daemon_is_waited_through_socket_removal() {
        let mut probes = [
            DaemonHealth {
                state: DaemonState::Draining,
                protocol_version: Some(PROTOCOL_VERSION),
                instance_id: Some("old".to_string()),
                pid: Some(42),
                executable_identity: Some("8:99".to_string()),
                active: 0,
                notifications: false,
            },
            DaemonHealth::stopped(),
        ]
        .into_iter();

        assert_eq!(
            wait_for_drain_transition(Duration::from_secs(1), || Ok(probes.next().unwrap()))
                .unwrap(),
            DaemonHealth::stopped()
        );
    }

    #[test]
    fn draining_daemon_is_waited_through_concurrent_replacement() {
        let replacement = DaemonHealth {
            state: DaemonState::Running,
            protocol_version: Some(PROTOCOL_VERSION),
            instance_id: Some("new".to_string()),
            pid: Some(43),
            executable_identity: Some("8:100".to_string()),
            active: 0,
            notifications: true,
        };
        let mut probes = [
            DaemonHealth {
                state: DaemonState::Draining,
                protocol_version: Some(PROTOCOL_VERSION),
                instance_id: Some("old".to_string()),
                pid: Some(42),
                executable_identity: Some("8:99".to_string()),
                active: 1,
                notifications: false,
            },
            replacement.clone(),
        ]
        .into_iter();

        assert_eq!(
            wait_for_drain_transition(Duration::from_secs(1), || Ok(probes.next().unwrap()))
                .unwrap(),
            replacement
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn deleted_executable_path_uses_installed_replacement_identity() {
        let temp = crate::compact_runtime::CompactTempDir::new("worker-deleted-executable");
        let installed = temp.path.join("prism");
        fs::write(&installed, "replacement").unwrap();
        let deleted = PathBuf::from(format!("{} (deleted)", installed.display()));

        assert_eq!(
            executable_identity(&deleted).unwrap(),
            executable_identity(&installed).unwrap()
        );
    }

    #[test]
    fn wake_requests_force_running_scheduler_poll_only() {
        let active = Arc::new(Mutex::new(BTreeSet::new()));
        let notification_subscriber = Arc::new(Mutex::new(Vec::new()));
        for (draining, expected) in [
            (false, WorkerControl::Wake),
            (true, WorkerControl::Continue),
        ] {
            let (mut server, mut client) = UnixStream::pair().unwrap();
            client.write_all(b"wake\n").unwrap();

            let control = respond(
                &mut server,
                "daemon",
                "exe",
                &active,
                &notification_subscriber,
                draining,
            );
            assert_eq!(control, expected);
            let future = Instant::now() + POLL_INTERVAL;
            let mut next_poll = future;
            let mut applied_draining = draining;
            let mut restart = false;
            control.apply(&mut next_poll, &mut applied_draining, &mut restart);
            assert_eq!(next_poll < future, !draining);
        }
    }

    fn runtime_with_socket_path_len(byte_len: usize) -> PathBuf {
        use std::os::unix::ffi::OsStringExt;

        const SOCKET_SUFFIX_LEN: usize = b"/worker.sock".len();
        let mut bytes = vec![b'/'];
        bytes.extend(std::iter::repeat_n(b'a', byte_len - SOCKET_SUFFIX_LEN - 1));
        PathBuf::from(std::ffi::OsString::from_vec(bytes))
    }
}
