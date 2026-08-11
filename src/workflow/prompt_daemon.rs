//! On-demand user-wide Worker and socket transport for prompt Workflows.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Write as _};
#[cfg(unix)]
use std::os::fd::AsRawFd as _;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use sha2::{Digest as _, Sha256};

use crate::platform::SupportedOs;
use crate::workflow::worker_ipc::{self, WorkerEndpoint, WorkerStream};

const PROTOCOL_VERSION: u32 = 4;
const TRANSITION_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(not(test))]
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(test)]
const REQUEST_READ_TIMEOUT: Duration = Duration::from_millis(250);
const RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REQUEST_BYTES: usize = 1024 * 1024;

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
    let secret = worker_ipc::read_secret(&endpoint)?;
    parse_health(&request_stream(
        stream,
        &authenticated_command(&secret, "health"),
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
    wait_stopped(TRANSITION_TIMEOUT)
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

fn serve_socket(
    runtime: &tokio::runtime::Runtime,
    service: &crate::PromptWorkflowService,
    generation: &str,
) -> Result<(), String> {
    let directory = runtime_dir();
    worker_ipc::prepare_runtime(&directory)?;
    let _lock = acquire_lock(&directory.join("worker.lock"))?;
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
    let mut draining = false;
    loop {
        match worker_ipc::accept(&listener) {
            Ok(mut stream) => {
                worker_ipc::set_write_timeout(&stream, RESPONSE_WRITE_TIMEOUT)
                    .map_err(|error| format!("configure worker response timeout: {error}"))?;
                draining |= respond(
                    runtime,
                    service,
                    &mut stream,
                    &secret,
                    &instance,
                    generation,
                    draining,
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(format!("accept Prism worker connection: {error}")),
        }
        if draining {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    drop(listener);
    endpoint
        .remove_stale_address()
        .map_err(|error| format!("remove Prism worker endpoint: {error}"))
}

fn respond(
    runtime: &tokio::runtime::Runtime,
    service: &crate::PromptWorkflowService,
    stream: &mut WorkerStream,
    secret: &str,
    instance: &str,
    generation: &str,
    draining: bool,
) -> bool {
    let Ok(request) = read_request_line(stream) else {
        let _ = stream.write_all(b"error invalid-request\n");
        return false;
    };
    let request = request.trim();
    let Some(request) = authenticate_command(secret, request) else {
        let _ = stream.write_all(b"error authentication-failed\n");
        return false;
    };
    let active = runtime
        .block_on(service.list(None, 10_000))
        .map(|runs| {
            runs.into_iter()
                .filter(|run| !run.status.terminal())
                .count()
        })
        .unwrap_or(0);
    let response = match request {
        "health" | "wake" => health_line(instance, generation, draining, active),
        "shutdown" => health_line(instance, generation, true, active),
        request if request.starts_with('{') => prompt_response(runtime, service, request),
        _ => "error unknown-command\n".into(),
    };
    let _ = stream.write_all(response.as_bytes());
    request == "shutdown"
}

fn read_request_line(stream: &mut WorkerStream) -> io::Result<String> {
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
        worker_ipc::set_read_timeout(stream, remaining)?;
        let available = MAX_REQUEST_BYTES + 1 - request.len();
        if available == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "worker request exceeds the size limit",
            ));
        }
        let read_len = available.min(buffer.len());
        let count = stream.read(&mut buffer[..read_len])?;
        if count == 0 {
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
    runtime: &tokio::runtime::Runtime,
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
    ensure_running()?;
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
    ensure_running()?;
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
    let endpoint = validated_socket_path()?;
    let stream = endpoint
        .connect()
        .map_err(|error| format!("connect to Prism worker: {error}"))?;
    let secret = worker_ipc::read_secret(&endpoint)?;
    request_stream(stream, &authenticated_command(&secret, command))
}

fn authenticated_command(secret: &str, command: &str) -> String {
    format!("auth {secret} {command}")
}

fn authenticate_command<'a>(secret: &str, command: &'a str) -> Option<&'a str> {
    command.strip_prefix(&format!("auth {secret} "))
}

fn request_stream(mut stream: WorkerStream, command: &str) -> Result<String, String> {
    worker_ipc::set_read_timeout(&stream, Duration::from_secs(30))
        .map_err(|error| format!("configure Prism worker endpoint: {error}"))?;
    worker_ipc::set_write_timeout(&stream, Duration::from_secs(30))
        .map_err(|error| format!("configure Prism worker endpoint: {error}"))?;
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
    #[test]
    fn socket_path_is_bounded_before_bind() {
        let root = PathBuf::from("x".repeat(worker_ipc::UNIX_SOCKET_PATH_BUDGET));
        assert!(WorkerEndpoint::for_runtime(&root).is_err());
    }
}
