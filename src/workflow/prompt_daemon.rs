//! On-demand user-wide Worker and socket transport for prompt Workflows.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
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

const PROTOCOL_VERSION: u32 = 5;
const TRANSITION_TIMEOUT: Duration = Duration::from_secs(3);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(6);
const MAX_CONNECTION_HANDLERS: usize = 32;
const SOCKET_PATH_BUDGET: usize = 103;
const AUTHENTICATION_FAILED_RESPONSE: &str = "error authentication-failed";
const AUTHENTICATION_SECRET_BYTES: usize = 32;

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
            return Ok(DaemonHealth::stopped());
        }
        Err(error) => return Err(format!("connect to Prism worker: {error}")),
    };
    parse_health(&request_with_authentication_fallback(
        &path, stream, "health",
    )?)
}

pub fn ensure_running() -> Result<(), String> {
    let generation = binary_generation()?;
    let health = probe_health()?;
    if health.state == DaemonState::Running
        && health.protocol_version == Some(PROTOCOL_VERSION)
        && health.binary_generation.as_deref() == Some(generation.as_str())
    {
        return Ok(());
    }
    if health.state != DaemonState::Stopped {
        let _ = request("shutdown");
        wait_stopped(TRANSITION_TIMEOUT)?;
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
            Ok(health)
                if health.state == DaemonState::Running
                    && health.protocol_version == Some(PROTOCOL_VERSION)
                    && health.binary_generation.as_deref() == Some(generation.as_str()) =>
            {
                return Ok(());
            }
            Ok(health) => last = format!("worker state is {:?}", health.state),
            Err(error) => last = error,
        }
        thread::sleep(Duration::from_millis(25));
    }
    Err(last)
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
        if probe_health()?.state == DaemonState::Stopped {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(25));
    }
    Err("timed out waiting for Prism worker daemon to stop".into())
}

pub fn serve() -> Result<(), String> {
    let generation = std::env::var("PRISM_WORKER_GENERATION")
        .ok()
        .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map_or_else(binary_generation, Ok)?;
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
    let result = serve_socket(&runtime, &service, &generation);
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
    requests: AtomicUsize,
}

fn serve_socket(
    runtime: &tokio::runtime::Runtime,
    service: &crate::PromptWorkflowService,
    generation: &str,
) -> Result<(), String> {
    let directory = runtime_dir();
    secure_runtime_directory(&directory)?;
    let _lock = acquire_lock(&directory.join("worker.lock"))?;
    let socket = validated_socket_path()?;
    if socket.exists() {
        match UnixStream::connect(&socket) {
            Ok(_) => return Err("a live Prism worker already owns the socket".into()),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                ) =>
            {
                fs::remove_file(&socket)
                    .map_err(|error| format!("remove stale worker socket: {error}"))?;
            }
            Err(error) => return Err(format!("inspect existing worker socket: {error}")),
        }
    }
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
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                if control.handlers.load(Ordering::Acquire) >= MAX_CONNECTION_HANDLERS {
                    let _ = stream.write_all(b"error worker-busy\n");
                    continue;
                }
                control.handlers.fetch_add(1, Ordering::AcqRel);
                let handle = runtime.handle().clone();
                let service = service.clone();
                let instance = instance.clone();
                let generation = generation.to_string();
                let handler_control = Arc::clone(&control);
                if let Err(error) = thread::Builder::new()
                    .name("prism-worker-connection".to_string())
                    .spawn(move || {
                        respond(
                            &handle,
                            &service,
                            &mut stream,
                            &instance,
                            &generation,
                            &handler_control,
                        );
                        handler_control.handlers.fetch_sub(1, Ordering::AcqRel);
                    })
                {
                    control.handlers.fetch_sub(1, Ordering::AcqRel);
                    return Err(format!("spawn Prism worker connection handler: {error}"));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(format!("accept Prism worker connection: {error}")),
        }
        if control.draining.load(Ordering::Acquire)
            && control.handlers.load(Ordering::Acquire) == 0
            && control.requests.load(Ordering::Acquire) == 0
        {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    drop(listener);
    fs::remove_file(&socket).map_err(|error| format!("remove Prism worker socket: {error}"))
}

fn respond(
    runtime: &tokio::runtime::Handle,
    service: &crate::PromptWorkflowService,
    stream: &mut UnixStream,
    instance: &str,
    generation: &str,
    control: &DaemonControl,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut request = String::new();
    if BufReader::new(&mut *stream)
        .take(1024 * 1024 + 1)
        .read_line(&mut request)
        .is_err()
        || request.len() > 1024 * 1024
    {
        let _ = stream.write_all(b"error invalid-request\n");
        return;
    }
    let request = request.trim();
    let active = runtime
        .block_on(service.list(None, 10_000))
        .map(|runs| {
            runs.into_iter()
                .filter(|run| !run.status.terminal())
                .count()
        })
        .unwrap_or(0);
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
                let response = prompt_response(runtime, service, request);
                control.requests.fetch_sub(1, Ordering::AcqRel);
                response
            }
        }
        _ => "error unknown-command\n".into(),
    };
    let _ = stream.write_all(response.as_bytes());
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
        operation: String,
        subject: String,
        payload: serde_json::Value,
    },
    RemoteMutate {
        repository: PathBuf,
        worktree: PathBuf,
        request_id: String,
        operation: String,
        subject: String,
        payload: serde_json::Value,
    },
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum SocketControl {
    Pause,
    Resume,
    Cancel,
    Retry,
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
                payload,
            } => service
                .remote_observe(&repository, &worktree, operation, subject, payload)
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
                payload,
            } => service
                .remote_mutate(
                    &repository,
                    &worktree,
                    request_id,
                    operation,
                    subject,
                    payload,
                )
                .await
                .and_then(|result| match result {
                    crate::remote::request_coordinator::RemoteMutationResult::Applied(value) => {
                        Ok(serde_json::json!({"ok": true, "state": "applied", "value": value}))
                    }
                    crate::remote::request_coordinator::RemoteMutationResult::Pending(wait) => {
                        Ok(serde_json::json!({"ok": true, "state": "pending", "wait": wait}))
                    }
                    crate::remote::request_coordinator::RemoteMutationResult::Failed(reason) => {
                        Err(reason)
                    }
                }),
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

fn worker_request(value: serde_json::Value) -> Result<serde_json::Value, String> {
    let response = request(&value.to_string())?;
    let response: serde_json::Value = serde_json::from_str(&response)
        .map_err(|error| format!("decode Worker response: {error}"))?;
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

#[derive(Clone, Copy, Debug, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptWorkflowControl {
    Pause,
    Resume,
    Cancel,
    Retry,
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
    operation: &str,
    subject: &str,
    payload: impl serde::Serialize,
) -> Result<T, String> {
    observe_remote_with_progress(
        repository,
        worktree,
        operation,
        subject,
        payload,
        |_| {},
        || false,
    )
}

pub(crate) fn observe_remote_with_progress<T, F, C>(
    repository: &Path,
    worktree: &Path,
    operation: &str,
    subject: &str,
    payload: impl serde::Serialize,
    on_wait: F,
    is_cancelled: C,
) -> Result<T, String>
where
    T: serde::de::DeserializeOwned,
    F: FnMut(crate::remote::request_coordinator::RemoteWait),
    C: Fn() -> bool,
{
    ensure_compatible_running()?;
    let payload = serde_json::to_value(payload)
        .map_err(|error| format!("encode remote observation: {error}"))?;
    coordinated_remote_request(
        serde_json::json!({
            "type": "remote_observe",
            "repository": repository,
            "worktree": worktree,
            "operation": operation,
            "subject": subject,
            "payload": payload,
        }),
        "fresh",
        on_wait,
        is_cancelled,
    )
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

pub(crate) fn mutate_remote_with_progress<T, F, C>(
    repository: &Path,
    worktree: &Path,
    request_id: &str,
    operation: &str,
    subject: &str,
    payload: impl serde::Serialize,
    progress: RemoteRequestProgress<F, C>,
) -> Result<T, String>
where
    T: serde::de::DeserializeOwned,
    F: FnMut(crate::remote::request_coordinator::RemoteWait),
    C: Fn() -> bool,
{
    ensure_compatible_running()?;
    let payload = serde_json::to_value(payload)
        .map_err(|error| format!("encode remote mutation: {error}"))?;
    coordinated_remote_request(
        serde_json::json!({
            "type": "remote_mutate",
            "repository": repository,
            "worktree": worktree,
            "request_id": request_id,
            "operation": operation,
            "subject": subject,
            "payload": payload,
        }),
        "applied",
        progress.on_wait,
        progress.is_cancelled,
    )
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
    loop {
        if is_cancelled() {
            return Err("remote request cancelled while queued".into());
        }
        let response = worker_request(request_value.clone())?;
        if response["state"] == completed_state {
            return serde_json::from_value(response["value"].clone())
                .map_err(|error| format!("decode coordinated remote response: {error}"));
        }
        let wait: crate::remote::request_coordinator::RemoteWait =
            serde_json::from_value(response["wait"].clone())
                .map_err(|error| format!("decode coordinated remote Wait: {error}"))?;
        on_wait(wait.clone());
        if Instant::now() >= deadline {
            return Err(wait.summary);
        }
        let wake = wait.wake_at_unix_ms;
        let delay = wake
            .saturating_sub(crate::workflow::prompt_worker::now_unix_ms())
            .clamp(25, 250);
        thread::sleep(Duration::from_millis(u64::try_from(delay).unwrap_or(250)));
    }
}

fn request(command: &str) -> Result<String, String> {
    let path = validated_socket_path()?;
    let stream =
        UnixStream::connect(&path).map_err(|error| format!("connect to Prism worker: {error}"))?;
    request_with_authentication_fallback(&path, stream, command)
}

fn request_with_authentication_fallback(
    socket_path: &Path,
    stream: UnixStream,
    command: &str,
) -> Result<String, String> {
    let response = request_stream(stream, command)?;
    if response != AUTHENTICATION_FAILED_RESPONSE {
        return Ok(response);
    }

    let secret = read_authentication_secret(socket_path)?;
    let stream = UnixStream::connect(socket_path)
        .map_err(|error| format!("reconnect to authenticated Prism worker: {error}"))?;
    request_stream(stream, &format!("auth {secret} {command}"))
}

fn read_authentication_secret(socket_path: &Path) -> Result<String, String> {
    let secret_path = socket_path.with_file_name("worker.secret");
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&secret_path)
        .map_err(|error| format!("read worker authentication secret: {error}"))?;
    let mut secret = String::new();
    file.read_to_string(&mut secret)
        .map_err(|error| format!("read worker authentication secret: {error}"))?;
    let secret = secret.trim();
    if secret.len() != AUTHENTICATION_SECRET_BYTES * 2
        || !secret.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("worker authentication secret is invalid".to_string());
    }
    Ok(secret.to_string())
}

fn request_stream(mut stream: UnixStream, command: &str) -> Result<String, String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
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
        let (mut health_server, mut health_client) =
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
                &mut health_server,
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
        assert!(response.starts_with("ok 5 test-instance"), "{response}");
        assert_eq!(control.handlers.load(Ordering::Acquire), 1);

        release_tx.send(()).expect("release slow handler");
        slow.join().expect("join slow handler");
        health.join().expect("join health handler");
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
    fn authenticated_daemon_response_is_retried_with_runtime_secret() {
        let runtime = crate::compact_runtime::CompactTempDir::new("auth-fallback");
        let root = runtime.runtime_path();
        fs::create_dir_all(root).expect("create test runtime");
        let socket = root.join("worker.sock");
        let secret = "ab".repeat(AUTHENTICATION_SECRET_BYTES);
        fs::write(root.join("worker.secret"), &secret).expect("write test worker secret");
        let listener = UnixListener::bind(&socket).expect("bind test worker socket");
        let expected_authenticated = format!("auth {secret} health");

        let server = thread::spawn(move || {
            for (expected, response) in [
                ("health".to_string(), AUTHENTICATION_FAILED_RESPONSE),
                (expected_authenticated, "ok authenticated"),
            ] {
                let (mut stream, _) = listener.accept().expect("accept test client");
                let mut request = String::new();
                BufReader::new(&mut stream)
                    .read_line(&mut request)
                    .expect("read test request");
                assert_eq!(request.trim(), expected);
                stream
                    .write_all(format!("{response}\n").as_bytes())
                    .expect("write test response");
            }
        });

        let stream = UnixStream::connect(&socket).expect("connect test client");
        let response = request_with_authentication_fallback(&socket, stream, "health")
            .expect("retry authenticated request");
        assert_eq!(response, "ok authenticated");
        server.join().expect("join test server");
    }
}
