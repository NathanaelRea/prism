#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::agent::AgentState;
use crate::config::Config;
use crate::json::json_escape;
use crate::observability;
use crate::repo::Repository;
use serde_json::Value;

const HEALTH_TIMEOUT: Duration = Duration::from_millis(250);
const API_TIMEOUT: Duration = Duration::from_secs(5);
const SSE_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const SSE_READ_TIMEOUT: Duration = Duration::from_secs(60);
const SSE_CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(200);
const SERVER_START_TIMEOUT: Duration = Duration::from_secs(5);
const SERVER_START_POLL: Duration = Duration::from_millis(100);

static OWNED_SERVER_PROCESSES: OnceLock<Mutex<BTreeMap<u32, OwnedServerProcess>>> = OnceLock::new();

struct OwnedServerProcess {
    child: crate::process::SupervisedChild,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpencodeRuntime {
    pub repo_root: String,
    pub harness_id: String,
    pub branch: String,
    pub worktree_path: String,
    pub server_port: u16,
    pub server_url: String,
    pub server_pid: Option<u32>,
    pub server_process_identity: Option<u64>,
    pub opencode_session_id: Option<String>,
    pub generation: u64,
    pub updated_unix_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortStatus {
    Free,
    OpenCode,
    Occupied,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpencodeSession {
    pub id: String,
    pub directory: Option<String>,
    pub title: Option<String>,
    pub time_updated: Option<String>,
    pub parent_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpencodeState {
    Unknown,
    Starting,
    Idle,
    Done,
    Busy,
    Retry,
    NeedsInput,
    Error,
    Offline,
}

impl OpencodeState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Starting => "starting",
            Self::Idle => "idle",
            Self::Done => "done",
            Self::Busy => "busy",
            Self::Retry => "retry",
            Self::NeedsInput => "needs input",
            Self::Error => "error",
            Self::Offline => "offline",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "unknown" => Some(Self::Unknown),
            "starting" | "loading" => Some(Self::Starting),
            "idle" | "ready" => Some(Self::Idle),
            "done" | "completed" => Some(Self::Done),
            "busy" | "running" | "working" => Some(Self::Busy),
            "retry" | "retrying" => Some(Self::Retry),
            "needs input" | "needs-input" | "permission" => Some(Self::NeedsInput),
            "error" | "failed" => Some(Self::Error),
            "offline" | "disconnected" => Some(Self::Offline),
            _ => None,
        }
    }

    pub fn agent_state(self) -> AgentState {
        match self {
            Self::Unknown => AgentState::NeedsRestart,
            Self::Starting => AgentState::Running,
            Self::Idle => AgentState::Idle,
            Self::Done => AgentState::ExitedOk,
            Self::Busy | Self::Retry => AgentState::Running,
            Self::NeedsInput => AgentState::NeedsInput,
            Self::Error => AgentState::ExitedError,
            Self::Offline => AgentState::NeedsRestart,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpencodeTodo {
    pub text: String,
    pub status: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpencodeStatus {
    pub server_url: Option<String>,
    pub session_id: Option<String>,
    pub title: Option<String>,
    pub state: OpencodeState,
    pub detail: Option<String>,
    pub latest_message: Option<String>,
    pub latest_user_message: Option<String>,
    pub recent_messages: Vec<String>,
    pub active_tool: Option<String>,
    pub todos: Vec<OpencodeTodo>,
    pub last_updated_unix_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpencodeEvent {
    pub session_id: Option<String>,
    pub title: Option<String>,
    pub state: Option<OpencodeState>,
    pub detail: Option<String>,
    pub latest_message: Option<String>,
    pub active_tool: Option<String>,
    pub todos: Option<Vec<OpencodeTodo>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OpencodeSnapshotFacet {
    Status,
    Message,
}

impl OpencodeStatus {
    pub fn offline(server_url: Option<String>, session_id: Option<String>) -> Self {
        Self {
            server_url,
            session_id,
            title: None,
            state: OpencodeState::Offline,
            detail: None,
            latest_message: None,
            latest_user_message: None,
            recent_messages: Vec::new(),
            active_tool: None,
            todos: Vec::new(),
            last_updated_unix_ms: Some(unix_ms()),
        }
    }
}

pub fn ensure_opencode_server(
    repo: &Repository,
    config: &Config,
    branch: &str,
    worktree: &Path,
) -> Result<OpencodeRuntime, String> {
    ensure_opencode_server_with_program(
        repo,
        config,
        &config.default_harness,
        branch,
        worktree,
        &config.tool("opencode"),
    )
}

pub fn ensure_opencode_server_with_program(
    repo: &Repository,
    config: &Config,
    harness_id: &str,
    branch: &str,
    worktree: &Path,
    program: &str,
) -> Result<OpencodeRuntime, String> {
    let _server_lock = lock_repository_server(repo)?;
    ensure_opencode_server_locked(repo, config, harness_id, branch, worktree, program)
}

fn ensure_opencode_server_locked(
    repo: &Repository,
    config: &Config,
    harness_id: &str,
    branch: &str,
    worktree: &Path,
    program: &str,
) -> Result<OpencodeRuntime, String> {
    let existing = load_runtime(repo, harness_id, branch, worktree)?;
    let runtimes = load_runtimes_for_harness(repo, harness_id)?;
    if let Some(shared) = healthy_shared_runtime(&runtimes) {
        let runtime = runtime_for_worktree(repo, harness_id, branch, worktree, &shared, &existing);
        save_shared_server_runtime(repo, &runtime)?;
        return Ok(runtime);
    }

    let runtime_identity = format!("{}:{harness_id}", repo.root.display());
    let stored_port = runtimes
        .iter()
        .filter(|runtime| runtime.server_pid.is_some() && stored_server_identity_is_valid(runtime))
        .min_by_key(|runtime| (runtime.server_port, runtime.server_url.as_str()))
        .map(|runtime| runtime.server_port);
    let port = allocate_port(
        &runtime_identity,
        "",
        stored_port,
        config.opencode_port_base,
        config.opencode_port_span,
        port_status,
    )?;
    let server_url = server_url(port);
    let mut started_server = None;
    let server_pid = if check_health(&server_url) {
        existing.as_ref().and_then(|runtime| runtime.server_pid)
    } else {
        let mut command = Command::new(program);
        command
            .arg("serve")
            .args(["--hostname", "127.0.0.1"])
            .args(["--port", &port.to_string()])
            .current_dir(&repo.root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = crate::process::SupervisedChild::spawn_named(
            &mut command,
            None,
            None,
            crate::process::ProcessDescriptor::new("opencode.server.serve"),
        )
        .map_err(|error| format!("start opencode server: {error}"))?;
        if let Err(error) = wait_for_health(&server_url) {
            let _ = child.terminate();
            return Err(error);
        }
        let pid = child.id();
        started_server = Some(child);
        Some(pid)
    };

    let server_process_identity = if started_server.is_some() {
        server_pid.and_then(stored_process_identity)
    } else {
        existing
            .as_ref()
            .and_then(|runtime| runtime.server_process_identity)
    };
    let runtime = OpencodeRuntime {
        repo_root: repo.root.display().to_string(),
        harness_id: harness_id.to_string(),
        branch: branch.to_string(),
        worktree_path: worktree.display().to_string(),
        server_port: port,
        server_url,
        server_pid,
        server_process_identity,
        opencode_session_id: existing.and_then(|runtime| runtime.opencode_session_id),
        generation: 0,
        updated_unix_ms: unix_ms(),
    };
    if let Err(error) = save_shared_server_runtime(repo, &runtime) {
        if let Some(mut child) = started_server {
            let _ = child.terminate();
        }
        return Err(error);
    }
    if let Some(child) = started_server {
        record_owned_server_process(child);
    }
    Ok(runtime)
}

pub(crate) fn lock_repository_server(repo: &Repository) -> Result<File, String> {
    let state_dir = repo.prism_dir();
    fs::create_dir_all(&state_dir).map_err(|error| {
        format!(
            "create OpenCode server state directory {}: {error}",
            state_dir.display()
        )
    })?;
    let lock_path = state_dir.join("opencode-server.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|error| format!("open OpenCode server lock {}: {error}", lock_path.display()))?;
    lock.lock().map_err(|error| {
        format!(
            "acquire OpenCode server lock {}: {error}",
            lock_path.display()
        )
    })?;
    Ok(lock)
}

fn healthy_shared_runtime(runtimes: &[OpencodeRuntime]) -> Option<OpencodeRuntime> {
    let mut servers = BTreeMap::new();
    for runtime in runtimes {
        servers
            .entry((runtime.server_port, runtime.server_url.as_str()))
            .or_insert(runtime);
    }
    servers
        .into_values()
        .find(|runtime| {
            check_health(&runtime.server_url)
                && (stored_server_identity_is_valid(runtime) || runtime.server_pid.is_none())
        })
        .cloned()
}

fn stored_server_identity_is_valid(runtime: &OpencodeRuntime) -> bool {
    if parse_localhost_url(&runtime.server_url)
        .ok()
        .map(|(_, port)| port)
        != Some(runtime.server_port)
    {
        return false;
    }
    if let Some(pid) = runtime.server_pid {
        if !stored_server_process_matches(pid, runtime.server_port).unwrap_or(false) {
            return false;
        }
        return match runtime.server_process_identity {
            Some(identity) => crate::process::observe_process(
                crate::process::RecordedProcess::from_stored(pid, Some(identity)),
            )
            .is_ok_and(|observation| {
                observation == crate::process::ProcessObservation::RunningSameProcess
            }),
            None => stored_runtime_session_matches(runtime),
        };
    }
    stored_runtime_session_matches(runtime)
}

fn stored_runtime_session_matches(runtime: &OpencodeRuntime) -> bool {
    runtime
        .opencode_session_id
        .as_deref()
        .and_then(|session_id| {
            get_session_for_worktree(
                &runtime.server_url,
                session_id,
                Path::new(&runtime.worktree_path),
            )
            .ok()
            .flatten()
        })
        .is_some_and(|session| session.directory.as_deref() == Some(runtime.worktree_path.as_str()))
}

fn runtime_for_worktree(
    repo: &Repository,
    harness_id: &str,
    branch: &str,
    worktree: &Path,
    shared: &OpencodeRuntime,
    existing: &Option<OpencodeRuntime>,
) -> OpencodeRuntime {
    let unchanged_server = existing.as_ref().is_some_and(|runtime| {
        runtime.server_url == shared.server_url
            && runtime.server_pid == shared.server_pid
            && runtime.server_process_identity == shared.server_process_identity
    });
    OpencodeRuntime {
        repo_root: repo.root.display().to_string(),
        harness_id: harness_id.to_string(),
        branch: branch.to_string(),
        worktree_path: worktree.display().to_string(),
        server_port: shared.server_port,
        server_url: shared.server_url.clone(),
        server_pid: shared.server_pid,
        server_process_identity: shared.server_process_identity,
        opencode_session_id: existing
            .as_ref()
            .and_then(|runtime| runtime.opencode_session_id.clone()),
        generation: existing
            .as_ref()
            .map(|runtime| runtime.generation)
            .unwrap_or_default(),
        updated_unix_ms: if unchanged_server {
            existing
                .as_ref()
                .map(|runtime| runtime.updated_unix_ms)
                .unwrap_or_else(unix_ms)
        } else {
            unix_ms()
        },
    }
}

fn server_reference_count(repo: &Repository, runtime: &OpencodeRuntime) -> Result<i64, String> {
    crate::persistence::session::count_server_references(
        &observability::db_path(repo),
        &runtime.repo_root,
        &runtime.server_url,
    )
    .map_err(|error| format!("count OpenCode server references: {error}"))
}

pub fn ensure_opencode_session(
    repo: &Repository,
    config: &Config,
    branch: &str,
    worktree: &Path,
) -> Result<OpencodeRuntime, String> {
    ensure_opencode_session_with_program(
        repo,
        config,
        &config.default_harness,
        branch,
        worktree,
        &config.tool("opencode"),
    )
}

pub fn ensure_opencode_session_with_program(
    repo: &Repository,
    config: &Config,
    harness_id: &str,
    branch: &str,
    worktree: &Path,
    program: &str,
) -> Result<OpencodeRuntime, String> {
    let _server_lock = lock_repository_server(repo)?;
    let mut runtime =
        ensure_opencode_server_locked(repo, config, harness_id, branch, worktree, program)?;
    let session = resolve_session(&runtime, worktree)?;
    save_runtime_session(repo, &mut runtime, session.id)?;
    Ok(runtime)
}

pub fn refresh_opencode_session(
    repo: &Repository,
    mut runtime: OpencodeRuntime,
    worktree: &Path,
) -> Result<OpencodeRuntime, String> {
    let _server_lock = lock_repository_server(repo)?;
    let Some(current) = load_runtime(
        repo,
        &runtime.harness_id,
        &runtime.branch,
        Path::new(&runtime.worktree_path),
    )?
    else {
        return Ok(runtime);
    };
    runtime = current;
    let Some(session) = newest_listed_session_for_worktree(&runtime, worktree).unwrap_or(None)
    else {
        return Ok(runtime);
    };
    save_runtime_session(repo, &mut runtime, session.id)?;
    Ok(runtime)
}

pub fn list_sessions(server_url: &str) -> Result<Vec<OpencodeSession>, String> {
    let response = get("opencode.session.list", server_url, "/session", API_TIMEOUT)?;
    if response.status_code != 200 {
        return Err(format!(
            "list opencode sessions failed with HTTP {}",
            response.status_code
        ));
    }
    Ok(parse_sessions(&response.body))
}

pub(crate) fn list_sessions_for_directory(
    server_url: &str,
    directory: &Path,
) -> Result<Vec<OpencodeSession>, String> {
    list_sessions_for_worktree(server_url, &directory.display().to_string())
}

pub fn get_session(server_url: &str, session_id: &str) -> Result<Option<OpencodeSession>, String> {
    get_session_in_directory(server_url, session_id, None)
}

fn get_session_for_worktree(
    server_url: &str,
    session_id: &str,
    worktree: &Path,
) -> Result<Option<OpencodeSession>, String> {
    get_session_in_directory(server_url, session_id, Some(worktree))
}

fn get_session_in_directory(
    server_url: &str,
    session_id: &str,
    directory: Option<&Path>,
) -> Result<Option<OpencodeSession>, String> {
    let path = request_path(
        &format!("/session/{}", url_path_segment(session_id)),
        directory,
    );
    let response = get("opencode.session.get", server_url, &path, API_TIMEOUT)?;
    match response.status_code {
        200 => Ok(parse_session(&response.body)),
        404 => Ok(None),
        status => Err(format!(
            "get opencode session {session_id} failed with HTTP {status}"
        )),
    }
}

pub fn create_session(
    server_url: &str,
    worktree: &Path,
    title: &str,
) -> Result<OpencodeSession, String> {
    let directory = worktree.display().to_string();
    let path = format!("/session?directory={}", url_path_segment(&directory));
    let body = format!(r#"{{"title":"{}"}}"#, json_escape(title));
    match post(
        "opencode.session.create",
        server_url,
        &path,
        &body,
        API_TIMEOUT,
    ) {
        Ok(response) if response.status_code == 200 || response.status_code == 201 => {
            parse_session(&response.body).ok_or_else(|| "created opencode session had no id".into())
        }
        Ok(response) if response.status_code == 400 || response.status_code == 415 => {
            let mut fallback = post(
                "opencode.session.create",
                server_url,
                &path,
                "{}",
                API_TIMEOUT,
            )?;
            if fallback.status_code == 400 || fallback.status_code == 415 {
                fallback = post(
                    "opencode.session.create",
                    server_url,
                    "/session",
                    "{}",
                    API_TIMEOUT,
                )?;
            }
            if fallback.status_code != 200 && fallback.status_code != 201 {
                return Err(format!(
                    "create opencode session failed with HTTP {}",
                    fallback.status_code
                ));
            }
            parse_session(&fallback.body).ok_or_else(|| "created opencode session had no id".into())
        }
        Ok(response) => Err(format!(
            "create opencode session failed with HTTP {}",
            response.status_code
        )),
        Err(error) => Err(error),
    }
}

pub fn submit_prompt(server_url: &str, session_id: &str, prompt: &str) -> Result<(), String> {
    let directory = get_session(server_url, session_id)
        .ok()
        .flatten()
        .and_then(|session| session.directory);
    submit_prompt_in_directory(
        server_url,
        session_id,
        prompt,
        directory.as_deref().map(Path::new),
    )
}

pub(crate) fn submit_prompt_for_worktree(
    server_url: &str,
    session_id: &str,
    prompt: &str,
    worktree: &Path,
) -> Result<(), String> {
    submit_prompt_in_directory(server_url, session_id, prompt, Some(worktree))
}

fn submit_prompt_in_directory(
    server_url: &str,
    session_id: &str,
    prompt: &str,
    directory: Option<&Path>,
) -> Result<(), String> {
    let body = prompt_async_body(prompt);
    let path = request_path(
        &format!("/session/{}/prompt_async", url_path_segment(session_id)),
        directory,
    );
    let response = post(
        "opencode.session.prompt",
        server_url,
        &path,
        &body,
        API_TIMEOUT,
    )?;
    if success_status(response.status_code) {
        Ok(())
    } else {
        Err(http_error_message(
            "submit opencode prompt",
            response.status_code,
            &response.body,
        ))
    }
}

pub fn abort_session(server_url: &str, session_id: &str) -> Result<(), String> {
    let directory = get_session(server_url, session_id)
        .ok()
        .flatten()
        .and_then(|session| session.directory);
    let path = request_path(
        &format!("/session/{}/abort", url_path_segment(session_id)),
        directory.as_deref().map(Path::new),
    );
    let response = post(
        "opencode.session.abort",
        server_url,
        &path,
        "{}",
        API_TIMEOUT,
    )?;
    if success_status(response.status_code) {
        Ok(())
    } else {
        Err(http_error_message(
            "abort opencode session",
            response.status_code,
            &response.body,
        ))
    }
}

pub fn shutdown_owned_server(runtime: &OpencodeRuntime) -> Result<(), String> {
    let Some(pid) = runtime.server_pid else {
        return Ok(());
    };
    let Some(mut owned) = take_owned_server_process(pid) else {
        return Ok(());
    };
    if owned
        .child
        .try_wait()
        .map_err(|error| format!("inspect owned opencode server {pid} before shutdown: {error}"))?
        .is_some()
    {
        return Ok(());
    }
    owned
        .child
        .terminate()
        .map(|_| ())
        .map_err(|error| format!("stop opencode server {pid}: {error}"))
}

pub(crate) fn shutdown_stored_server(runtime: &OpencodeRuntime) -> Result<(), String> {
    shutdown_stored_server_with(runtime, crate::process::process_arguments)
}

fn shutdown_stored_server_with(
    runtime: &OpencodeRuntime,
    inspect_arguments: impl FnOnce(
        u32,
    )
        -> Result<Option<Vec<String>>, crate::process::ProcessLifecycleError>,
) -> Result<(), String> {
    if runtime.server_pid.is_some_and(owned_server_process) {
        return shutdown_owned_server(runtime);
    }
    let Some(pid) = runtime.server_pid else {
        return Ok(());
    };
    if !stored_server_process_matches_with(pid, runtime.server_port, inspect_arguments)
        .map_err(|error| format!("inspect stored opencode server {pid} before shutdown: {error}"))?
    {
        return Ok(());
    }
    let recorded =
        crate::process::RecordedProcess::from_stored(pid, runtime.server_process_identity);
    match crate::process::terminate_recorded_process(recorded, Duration::from_secs(1))
        .map_err(|error| format!("stop opencode server {pid}: {error}"))?
    {
        crate::process::TerminationOutcome::Terminated
        | crate::process::TerminationOutcome::AlreadyExited
        | crate::process::TerminationOutcome::IdentityReused => Ok(()),
        crate::process::TerminationOutcome::Unverifiable => Err(format!(
            "refusing to stop opencode server {pid}: reusable process identity is unavailable"
        )),
    }
}

fn stored_server_process_matches(
    pid: u32,
    port: u16,
) -> Result<bool, crate::process::ProcessLifecycleError> {
    stored_server_process_matches_with(pid, port, crate::process::process_arguments)
}

fn stored_server_process_matches_with(
    pid: u32,
    port: u16,
    inspect_arguments: impl FnOnce(
        u32,
    )
        -> Result<Option<Vec<String>>, crate::process::ProcessLifecycleError>,
) -> Result<bool, crate::process::ProcessLifecycleError> {
    Ok(inspect_arguments(pid)?.is_some_and(|args| {
        let args = args.iter().map(String::as_str).collect::<Vec<_>>();
        stored_server_args_match(&args, port)
    }))
}

fn stored_server_args_match(args: &[&str], port: u16) -> bool {
    let port = port.to_string();
    args.windows(2).any(|window| window[1] == "serve")
        && args
            .windows(2)
            .any(|window| window[0] == "--hostname" && window[1] == "127.0.0.1")
        && args
            .windows(2)
            .any(|window| window[0] == "--port" && window[1] == port)
}

fn owned_server_processes() -> &'static Mutex<BTreeMap<u32, OwnedServerProcess>> {
    OWNED_SERVER_PROCESSES.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn record_owned_server_process(child: crate::process::SupervisedChild) {
    let pid = child.id();
    let process = OwnedServerProcess { child };
    owned_server_processes()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(pid, process);
}

fn owned_server_process(pid: u32) -> bool {
    owned_server_processes()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .contains_key(&pid)
}

fn take_owned_server_process(pid: u32) -> Option<OwnedServerProcess> {
    owned_server_processes()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(&pid)
}

fn stored_process_identity(pid: u32) -> Option<u64> {
    crate::process::record_process(pid)
        .ok()?
        .identity
        .map(crate::process::ProcessIdentity::stored_value)
}

pub fn poll_status(runtime: &OpencodeRuntime) -> Result<OpencodeStatus, String> {
    let Some(session_id) = runtime.opencode_session_id.as_deref() else {
        return Ok(OpencodeStatus {
            server_url: Some(runtime.server_url.clone()),
            session_id: None,
            title: None,
            state: OpencodeState::Starting,
            detail: None,
            latest_message: None,
            latest_user_message: None,
            recent_messages: Vec::new(),
            active_tool: None,
            todos: Vec::new(),
            last_updated_unix_ms: Some(unix_ms()),
        });
    };
    poll_session_status_in_directory(
        &runtime.server_url,
        session_id,
        Some(Path::new(&runtime.worktree_path)),
    )
}

pub fn poll_status_authoritative(runtime: &OpencodeRuntime) -> Result<OpencodeStatus, String> {
    let Some(session_id) = runtime.opencode_session_id.as_deref() else {
        return poll_status(runtime);
    };
    let server_url = &runtime.server_url;
    if !check_health(server_url) {
        return Err("OpenCode server is unavailable".to_string());
    }
    let worktree = Path::new(&runtime.worktree_path);
    let session = get_session_in_directory(server_url, session_id, Some(worktree))?
        .ok_or_else(|| format!("OpenCode session {session_id} is unavailable"))?;
    let directory = session
        .directory
        .as_deref()
        .map(Path::new)
        .or(Some(worktree));
    let mut state = fetch_session_state(server_url, session_id, directory)?;
    if fetch_pending_permission(server_url, session_id, directory)? {
        state = OpencodeState::NeedsInput;
    }
    let mut messages = fetch_message_summary(server_url, session_id, directory)?;
    if state == OpencodeState::Idle
        && let Some(message_state) = messages.latest_turn_state
    {
        state = message_state;
    }
    if state == OpencodeState::NeedsInput {
        messages.active_tool = None;
    }
    let todos = fetch_todos(server_url, session_id, directory)?;
    Ok(OpencodeStatus {
        server_url: Some(server_url.clone()),
        session_id: Some(session_id.to_string()),
        title: session.title,
        state,
        detail: messages.latest_error,
        latest_message: messages.latest_message,
        latest_user_message: messages.latest_user_message,
        recent_messages: messages.recent_messages,
        active_tool: messages.active_tool,
        todos,
        last_updated_unix_ms: Some(unix_ms()),
    })
}

pub fn poll_session_status(server_url: &str, session_id: &str) -> Result<OpencodeStatus, String> {
    poll_session_status_in_directory(server_url, session_id, None)
}

fn poll_session_status_in_directory(
    server_url: &str,
    session_id: &str,
    directory: Option<&Path>,
) -> Result<OpencodeStatus, String> {
    if !check_health(server_url) {
        return Ok(OpencodeStatus::offline(
            Some(server_url.to_string()),
            Some(session_id.to_string()),
        ));
    }

    let session =
        get_session_in_directory(server_url, session_id, directory)?.unwrap_or(OpencodeSession {
            id: session_id.to_string(),
            directory: None,
            title: None,
            time_updated: None,
            parent_id: None,
        });
    let session_directory = session.directory.as_deref().map(Path::new);
    let directory = directory.or(session_directory);
    let mut state =
        fetch_session_state(server_url, session_id, directory).unwrap_or(OpencodeState::Idle);
    if fetch_pending_permission(server_url, session_id, directory).unwrap_or(false) {
        state = OpencodeState::NeedsInput;
    }
    let mut messages = fetch_message_summary(server_url, session_id, directory).unwrap_or_default();
    if state == OpencodeState::Idle
        && let Some(message_state) = messages.latest_turn_state
    {
        state = message_state;
    }
    if state == OpencodeState::NeedsInput {
        messages.active_tool = None;
    }
    let todos = fetch_todos(server_url, session_id, directory).unwrap_or_default();

    Ok(OpencodeStatus {
        server_url: Some(server_url.to_string()),
        session_id: Some(session_id.to_string()),
        title: session.title,
        state,
        detail: messages.latest_error,
        latest_message: messages.latest_message,
        latest_user_message: messages.latest_user_message,
        recent_messages: messages.recent_messages,
        active_tool: messages.active_tool,
        todos,
        last_updated_unix_ms: Some(unix_ms()),
    })
}

pub fn listen_events(
    server_url: &str,
    mut on_event: impl FnMut(OpencodeEvent) -> Result<(), String>,
) -> Result<(), String> {
    listen_event_payloads(server_url, |payload| {
        if let Some(event) = parse_event_payload(&payload) {
            on_event(event)?;
        }
        Ok(())
    })
}

pub fn listen_events_until(
    server_url: &str,
    should_stop: impl FnMut() -> bool,
    mut on_event: impl FnMut(OpencodeEvent) -> Result<(), String>,
) -> Result<(), String> {
    listen_classified_events_until_in_directory(server_url, None, should_stop, |event, _| {
        on_event(event)
    })
}

pub(crate) fn listen_classified_events_until(
    server_url: &str,
    directory: &Path,
    should_stop: impl FnMut() -> bool,
    on_event: impl FnMut(OpencodeEvent, Option<OpencodeSnapshotFacet>) -> Result<(), String>,
) -> Result<(), String> {
    listen_classified_events_until_in_directory(server_url, Some(directory), should_stop, on_event)
}

fn listen_classified_events_until_in_directory(
    server_url: &str,
    directory: Option<&Path>,
    mut should_stop: impl FnMut() -> bool,
    mut on_event: impl FnMut(OpencodeEvent, Option<OpencodeSnapshotFacet>) -> Result<(), String>,
) -> Result<(), String> {
    let path = request_path("/event", directory);
    listen_event_payloads_with_stop_at_path(
        server_url,
        &path,
        SSE_CANCEL_POLL_INTERVAL,
        SSE_CANCEL_POLL_INTERVAL,
        SSE_READ_TIMEOUT,
        &mut should_stop,
        &mut |payload| {
            if let Some((event, facet)) = parse_event_payload_classified(&payload) {
                on_event(event, facet)?;
            }
            Ok(())
        },
    )
}

pub fn listen_event_payloads(
    server_url: &str,
    mut on_payload: impl FnMut(String) -> Result<(), String>,
) -> Result<(), String> {
    listen_event_payloads_with_stop(
        server_url,
        SSE_CONNECT_TIMEOUT,
        SSE_READ_TIMEOUT,
        SSE_READ_TIMEOUT,
        &mut || false,
        &mut on_payload,
    )
}

fn listen_event_payloads_with_stop(
    server_url: &str,
    connect_timeout: Duration,
    read_poll_interval: Duration,
    inactivity_timeout: Duration,
    should_stop: &mut impl FnMut() -> bool,
    on_payload: &mut impl FnMut(String) -> Result<(), String>,
) -> Result<(), String> {
    listen_event_payloads_with_stop_at_path(
        server_url,
        "/event",
        connect_timeout,
        read_poll_interval,
        inactivity_timeout,
        should_stop,
        on_payload,
    )
}

fn listen_event_payloads_with_stop_at_path(
    server_url: &str,
    path: &str,
    connect_timeout: Duration,
    read_poll_interval: Duration,
    inactivity_timeout: Duration,
    should_stop: &mut impl FnMut() -> bool,
    on_payload: &mut impl FnMut(String) -> Result<(), String>,
) -> Result<(), String> {
    let mut trace = crate::flight_recorder::ExternalCallTrace::begin(
        crate::flight_recorder::ExternalCallCategory::Http,
        "opencode.events",
        vec![
            crate::flight_recorder::text("method", "GET"),
            crate::flight_recorder::unsigned("timeout_ms", inactivity_timeout.as_millis()),
        ],
    );
    let mut metrics = SseMetrics::default();
    let result = listen_event_payloads_with_stop_inner(
        server_url,
        SseRequest {
            path,
            connect_timeout,
            read_poll_interval,
            inactivity_timeout,
        },
        should_stop,
        on_payload,
        &mut metrics,
    );
    if let Some(started) = metrics.stream_started {
        metrics.stream_lifetime_us = Some(started.elapsed().as_micros());
    }
    let mut fields = metrics.fields();
    match &result {
        Ok(()) => {
            fields.push(crate::flight_recorder::text(
                "terminal_reason",
                "stop_request",
            ));
            trace.finish(
                crate::flight_recorder::ExternalCallOutcome::Canceled,
                fields,
            );
        }
        Err(failure) => {
            fields.push(crate::flight_recorder::text(
                "terminal_reason",
                failure.kind.terminal_reason(),
            ));
            if let Some(error_kind) = failure.kind.error_kind() {
                fields.push(crate::flight_recorder::text("error_kind", error_kind));
            }
            trace.finish(failure.kind.outcome(), fields);
        }
    }
    result.map_err(|failure| failure.message)
}

#[derive(Default)]
struct SseMetrics {
    resolve_us: Option<u128>,
    connect_us: Option<u128>,
    write_us: Option<u128>,
    handshake_us: Option<u128>,
    stream_started: Option<Instant>,
    stream_lifetime_us: Option<u128>,
    status_code: Option<u16>,
    payload_count: u64,
    payload_bytes: u64,
}

#[derive(Clone, Copy)]
struct SseRequest<'a> {
    path: &'a str,
    connect_timeout: Duration,
    read_poll_interval: Duration,
    inactivity_timeout: Duration,
}

impl SseMetrics {
    fn fields(&self) -> Vec<crate::flight_recorder::Field> {
        let mut fields = vec![
            crate::flight_recorder::unsigned("payload_count", self.payload_count),
            crate::flight_recorder::unsigned("payload_bytes", self.payload_bytes),
        ];
        for (name, value) in [
            ("resolve_us", self.resolve_us),
            ("connect_us", self.connect_us),
            ("write_us", self.write_us),
            ("handshake_us", self.handshake_us),
            ("stream_lifetime_us", self.stream_lifetime_us),
        ] {
            if let Some(value) = value {
                fields.push(crate::flight_recorder::unsigned(name, value));
            }
        }
        if let Some(status_code) = self.status_code {
            fields.push(crate::flight_recorder::unsigned("status_code", status_code));
        }
        fields
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SseFailureKind {
    Resolve,
    Connect,
    Write,
    Read,
    Protocol,
    HttpStatus,
    Timeout,
    Closed,
    Callback,
}

impl SseFailureKind {
    const fn outcome(self) -> crate::flight_recorder::ExternalCallOutcome {
        match self {
            Self::Timeout => crate::flight_recorder::ExternalCallOutcome::TimedOut,
            Self::Closed => crate::flight_recorder::ExternalCallOutcome::Closed,
            _ => crate::flight_recorder::ExternalCallOutcome::Failed,
        }
    }

    const fn terminal_reason(self) -> &'static str {
        match self {
            Self::HttpStatus => "http_status",
            Self::Protocol => "protocol_error",
            Self::Timeout => "timeout",
            Self::Closed => "peer_close",
            Self::Callback => "callback_error",
            Self::Resolve | Self::Connect | Self::Write | Self::Read => "io_error",
        }
    }

    const fn error_kind(self) -> Option<&'static str> {
        match self {
            Self::Resolve => Some("resolve"),
            Self::Connect => Some("connect"),
            Self::Write => Some("write"),
            Self::Read => Some("read"),
            Self::Protocol => Some("parse"),
            Self::HttpStatus => Some("http_status"),
            Self::Timeout => Some("timeout"),
            Self::Closed => Some("closed"),
            Self::Callback => None,
        }
    }
}

struct SseFailure {
    kind: SseFailureKind,
    message: String,
}

impl SseFailure {
    fn new(kind: SseFailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

fn sse_io_failure(
    kind: SseFailureKind,
    context: &'static str,
    error: std::io::Error,
) -> SseFailure {
    let kind = if is_timeout(&error) {
        SseFailureKind::Timeout
    } else if error.kind() == std::io::ErrorKind::InvalidData {
        SseFailureKind::Protocol
    } else {
        kind
    };
    SseFailure::new(kind, format!("{context}: {error}"))
}

fn listen_event_payloads_with_stop_inner(
    server_url: &str,
    request: SseRequest<'_>,
    should_stop: &mut impl FnMut() -> bool,
    on_payload: &mut impl FnMut(String) -> Result<(), String>,
    metrics: &mut SseMetrics,
) -> Result<(), SseFailure> {
    let SseRequest {
        path,
        connect_timeout,
        read_poll_interval,
        inactivity_timeout,
    } = request;
    let resolve_started = Instant::now();
    let (host, port) = parse_localhost_url(server_url)
        .map_err(|message| SseFailure::new(SseFailureKind::Protocol, message))?;
    let address = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|error| sse_io_failure(SseFailureKind::Resolve, "resolve SSE host", error))?
        .next()
        .ok_or_else(|| SseFailure::new(SseFailureKind::Resolve, "resolve SSE host: no address"))?;
    metrics.resolve_us = Some(resolve_started.elapsed().as_micros());
    let connect_started = Instant::now();
    let mut stream = TcpStream::connect_timeout(&address, connect_timeout)
        .map_err(|error| sse_io_failure(SseFailureKind::Connect, "connect SSE stream", error))?;
    metrics.connect_us = Some(connect_started.elapsed().as_micros());
    stream
        .set_read_timeout(Some(read_poll_interval))
        .map_err(|error| {
            sse_io_failure(SseFailureKind::Read, "configure SSE read timeout", error)
        })?;
    stream
        .set_write_timeout(Some(connect_timeout))
        .map_err(|error| {
            sse_io_failure(SseFailureKind::Write, "configure SSE write timeout", error)
        })?;
    let write_started = Instant::now();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nAccept: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"
    )
    .map_err(|error| sse_io_failure(SseFailureKind::Write, "write SSE request", error))?;
    metrics.write_us = Some(write_started.elapsed().as_micros());

    let mut reader = BufReader::new(stream);
    let handshake_started = Instant::now();
    let mut status_line = String::new();
    if read_line_until(
        &mut reader,
        &mut status_line,
        should_stop,
        inactivity_timeout,
    )? == 0
    {
        return if (should_stop)() {
            Ok(())
        } else {
            Err(SseFailure::new(
                SseFailureKind::Closed,
                "opencode event stream closed before status",
            ))
        };
    }
    let status_code = status_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| {
            SseFailure::new(
                SseFailureKind::Protocol,
                format!("invalid SSE status line: {}", status_line.trim_end()),
            )
        })?
        .parse::<u16>()
        .map_err(|error| {
            SseFailure::new(
                SseFailureKind::Protocol,
                format!("parse SSE status: {error}"),
            )
        })?;
    metrics.status_code = Some(status_code);
    if !success_status(status_code) {
        return Err(SseFailure::new(
            SseFailureKind::HttpStatus,
            format!("open opencode event stream failed with HTTP {status_code}"),
        ));
    }

    let mut line = String::new();
    let mut chunked = false;
    loop {
        line.clear();
        let count = read_line_until(&mut reader, &mut line, should_stop, inactivity_timeout)?;
        if (should_stop)() {
            return Ok(());
        }
        if count == 0 {
            return Err(SseFailure::new(
                SseFailureKind::Closed,
                "opencode event stream closed before body",
            ));
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        let header = line.trim_end().to_ascii_lowercase();
        if header.starts_with("transfer-encoding:") && header.contains("chunked") {
            chunked = true;
        }
    }
    metrics.handshake_us = Some(handshake_started.elapsed().as_micros());
    metrics.stream_started = Some(Instant::now());

    if chunked {
        read_sse_payloads_until(
            BufReader::new(ChunkedBodyReader::new(reader)),
            on_payload,
            should_stop,
            metrics,
            inactivity_timeout,
        )
    } else {
        read_sse_payloads_until(reader, on_payload, should_stop, metrics, inactivity_timeout)
    }
}

fn read_sse_payloads_until(
    mut reader: impl BufRead,
    on_payload: &mut impl FnMut(String) -> Result<(), String>,
    should_stop: &mut impl FnMut() -> bool,
    metrics: &mut SseMetrics,
    inactivity_timeout: Duration,
) -> Result<(), SseFailure> {
    let mut line = String::new();
    let mut data = String::new();
    let mut last_activity = Instant::now();
    loop {
        if (should_stop)() {
            return Ok(());
        }
        let count = match reader.read_line(&mut line) {
            Ok(count) => count,
            Err(error) if is_timeout(&error) && last_activity.elapsed() < inactivity_timeout => {
                continue;
            }
            Err(error) if is_timeout(&error) => {
                return Err(SseFailure::new(
                    SseFailureKind::Timeout,
                    "opencode event stream timed out",
                ));
            }
            Err(error) => {
                return Err(sse_io_failure(
                    SseFailureKind::Read,
                    "read opencode event stream",
                    error,
                ));
            }
        };
        if count == 0 {
            return Err(SseFailure::new(
                SseFailureKind::Closed,
                "opencode event stream closed",
            ));
        }
        last_activity = Instant::now();
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            if !data.trim().is_empty() {
                let payload = data.trim().to_string();
                metrics.payload_count = metrics.payload_count.saturating_add(1);
                metrics.payload_bytes = metrics.payload_bytes.saturating_add(payload.len() as u64);
                on_payload(payload)
                    .map_err(|message| SseFailure::new(SseFailureKind::Callback, message))?;
                data.clear();
            }
            line.clear();
            continue;
        }
        if let Some(value) = trimmed.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.trim_start());
        }
        line.clear();
    }
}

fn read_line_until(
    reader: &mut impl BufRead,
    line: &mut String,
    should_stop: &mut impl FnMut() -> bool,
    inactivity_timeout: Duration,
) -> Result<usize, SseFailure> {
    let last_activity = Instant::now();
    loop {
        if (should_stop)() {
            return Ok(0);
        }
        match reader.read_line(line) {
            Ok(count) => return Ok(count),
            Err(error) if is_timeout(&error) && last_activity.elapsed() < inactivity_timeout => {
                continue;
            }
            Err(error) if is_timeout(&error) => {
                return Err(SseFailure::new(
                    SseFailureKind::Timeout,
                    "opencode event stream handshake timed out",
                ));
            }
            Err(error) => {
                return Err(sse_io_failure(
                    SseFailureKind::Read,
                    "read opencode event stream",
                    error,
                ));
            }
        }
    }
}

fn is_timeout(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

struct ChunkedBodyReader<R> {
    inner: R,
    remaining: usize,
    done: bool,
    consume_crlf: bool,
}

impl<R: BufRead> ChunkedBodyReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            remaining: 0,
            done: false,
            consume_crlf: false,
        }
    }
}

impl<R: BufRead> Read for ChunkedBodyReader<R> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if self.done || output.is_empty() {
            return Ok(0);
        }
        if self.consume_crlf {
            let mut crlf = [0_u8; 2];
            self.inner.read_exact(&mut crlf)?;
            self.consume_crlf = false;
        }
        if self.remaining == 0 {
            let mut size_line = String::new();
            self.inner.read_line(&mut size_line)?;
            let size = size_line
                .trim_end()
                .split(';')
                .next()
                .unwrap_or_default()
                .trim();
            self.remaining = usize::from_str_radix(size, 16).map_err(|error| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
            })?;
            if self.remaining == 0 {
                self.done = true;
                return Ok(0);
            }
        }
        let count = output.len().min(self.remaining);
        let read = self.inner.read(&mut output[..count])?;
        self.remaining = self.remaining.saturating_sub(read);
        if self.remaining == 0 {
            self.consume_crlf = true;
        }
        Ok(read)
    }
}

pub fn parse_event_payload(payload: &str) -> Option<OpencodeEvent> {
    parse_event_payload_classified(payload).map(|(event, _)| event)
}

fn parse_event_payload_classified(
    payload: &str,
) -> Option<(OpencodeEvent, Option<OpencodeSnapshotFacet>)> {
    let value = serde_json::from_str::<Value>(payload).ok()?;
    let event_type = string_field(&value, &["type", "event"]).unwrap_or_default();
    let object = event_body(&value).unwrap_or(&value);
    let session_id = session_id_field(&value)
        .or_else(|| session_id_field(object))
        .or_else(|| object.get("info").and_then(session_id_field));
    let state = string_field(object, &["status", "state"])
        .or_else(|| {
            object
                .get("status")
                .and_then(|status| string_field(status, &["type"]))
        })
        .or_else(|| string_field(&value, &["status", "state"]))
        .and_then(|value| parse_state_label(&value))
        .or_else(|| event_type_state(&event_type))
        .or_else(|| message_turn_state(&event_type, object));
    let detail = message_error(&event_type, object);
    let todos = if event_type.contains("todo") || object.get("todos").is_some() {
        Some(parse_todos_value(object))
    } else {
        None
    };
    let latest_message = if event_type.contains("message") || event_type.contains("part") {
        message_text(object).or_else(|| message_text(&value))
    } else {
        None
    };
    let active_tool = if event_type.contains("tool")
        || is_active_tool(object)
        || object.get("tool").is_some_and(Value::is_object)
    {
        tool_label(object)
            .or_else(|| object.get("tool").and_then(tool_label))
            .or_else(|| tool_label(&value))
    } else {
        None
    };
    let title = string_field(object, &["title"]).or_else(|| string_field(&value, &["title"]));
    let snapshot_facet = match event_type.as_str() {
        "session.status" | "session.idle" | "session.error" => Some(OpencodeSnapshotFacet::Status),
        "message.updated"
            if latest_message.is_none() && active_tool.is_none() && todos.is_none() =>
        {
            Some(OpencodeSnapshotFacet::Status)
        }
        event_type
            if (event_type.contains("message") || event_type.contains("part"))
                && latest_message.is_some()
                && state.is_none()
                && detail.is_none()
                && active_tool.is_none()
                && todos.is_none() =>
        {
            Some(OpencodeSnapshotFacet::Message)
        }
        _ => None,
    };

    let event = OpencodeEvent {
        session_id,
        title,
        state,
        detail,
        latest_message,
        active_tool,
        todos,
    };
    (event.session_id.is_some()
        || event.title.is_some()
        || event.state.is_some()
        || event.detail.is_some()
        || event.latest_message.is_some()
        || event.active_tool.is_some()
        || event.todos.is_some())
    .then_some((event, snapshot_facet))
}

fn prompt_async_body(prompt: &str) -> String {
    format!(
        r#"{{"parts":[{{"type":"text","text":"{}"}}]}}"#,
        json_escape(prompt)
    )
}

#[derive(Default)]
struct MessageSummary {
    latest_message: Option<String>,
    latest_user_message: Option<String>,
    recent_messages: Vec<String>,
    active_tool: Option<String>,
    latest_turn_state: Option<OpencodeState>,
    latest_error: Option<String>,
}

fn fetch_session_state(
    server_url: &str,
    session_id: &str,
    directory: Option<&Path>,
) -> Result<OpencodeState, String> {
    let path = request_path("/session/status", directory);
    let response = get("opencode.session.status", server_url, &path, API_TIMEOUT)?;
    if !success_status(response.status_code) {
        return Err(http_error_message(
            "read opencode session status",
            response.status_code,
            &response.body,
        ));
    }
    Ok(session_state_from_status_body(&response.body, session_id))
}

fn session_state_from_status_body(body: &str, session_id: &str) -> OpencodeState {
    parse_session_state(body, session_id).unwrap_or(OpencodeState::Idle)
}

fn fetch_pending_permission(
    server_url: &str,
    session_id: &str,
    directory: Option<&Path>,
) -> Result<bool, String> {
    let path = request_path("/permission", directory);
    let response = get("opencode.permission.list", server_url, &path, API_TIMEOUT)?;
    if !success_status(response.status_code) {
        return Err(http_error_message(
            "read opencode permissions",
            response.status_code,
            &response.body,
        ));
    }
    Ok(has_pending_permission(&response.body, session_id))
}

fn fetch_message_summary(
    server_url: &str,
    session_id: &str,
    directory: Option<&Path>,
) -> Result<MessageSummary, String> {
    let path = request_path(
        &format!("/session/{}/message?limit=10", url_path_segment(session_id)),
        directory,
    );
    let response = get("opencode.session.messages", server_url, &path, API_TIMEOUT)?;
    if !success_status(response.status_code) {
        return Err(http_error_message(
            "read opencode messages",
            response.status_code,
            &response.body,
        ));
    }
    Ok(parse_message_summary(&response.body))
}

fn fetch_todos(
    server_url: &str,
    session_id: &str,
    directory: Option<&Path>,
) -> Result<Vec<OpencodeTodo>, String> {
    let path = request_path(
        &format!("/session/{}/todo", url_path_segment(session_id)),
        directory,
    );
    let response = get("opencode.session.todos", server_url, &path, API_TIMEOUT)?;
    if !success_status(response.status_code) {
        return Err(http_error_message(
            "read opencode todos",
            response.status_code,
            &response.body,
        ));
    }
    Ok(parse_todos(&response.body))
}

fn resolve_session(runtime: &OpencodeRuntime, worktree: &Path) -> Result<OpencodeSession, String> {
    let worktree_path = worktree.display().to_string();
    let stored_session = if let Some(session_id) = runtime.opencode_session_id.as_deref()
        && let Some(session) = get_session_for_worktree(&runtime.server_url, session_id, worktree)?
        && session_matches_worktree(&session, &worktree_path)
    {
        Some(session)
    } else {
        None
    };

    match newest_listed_session_for_worktree(runtime, worktree) {
        Ok(Some(session)) => return Ok(session),
        Ok(None) => {}
        Err(_) if stored_session.is_some() => return Ok(stored_session.unwrap()),
        Err(error) => return Err(error),
    }

    if let Some(session) = stored_session {
        return Ok(session);
    }

    create_session(&runtime.server_url, worktree, &runtime.branch)
}

fn newest_listed_session_for_worktree(
    runtime: &OpencodeRuntime,
    worktree: &Path,
) -> Result<Option<OpencodeSession>, String> {
    let worktree_path = worktree.display().to_string();
    let sessions = list_sessions_for_worktree(&runtime.server_url, &worktree_path)?;
    Ok(newest_session_for_worktree(&sessions, &worktree_path).cloned())
}

fn list_sessions_for_worktree(
    server_url: &str,
    worktree_path: &str,
) -> Result<Vec<OpencodeSession>, String> {
    let path = format!(
        "/session?directory={}&limit=100",
        url_path_segment(worktree_path)
    );
    let response = get("opencode.session.list", server_url, &path, API_TIMEOUT)?;
    if response.status_code != 200 {
        return Err(format!(
            "list opencode sessions failed with HTTP {}",
            response.status_code
        ));
    }
    Ok(parse_sessions(&response.body))
}

fn save_runtime_session(
    repo: &Repository,
    runtime: &mut OpencodeRuntime,
    session_id: String,
) -> Result<(), String> {
    if runtime.opencode_session_id.as_deref() != Some(session_id.as_str()) {
        runtime.opencode_session_id = Some(session_id);
        runtime.generation = runtime.generation.saturating_add(1);
        runtime.updated_unix_ms = unix_ms();
        save_runtime(repo, runtime)?;
    }
    Ok(())
}

pub fn load_runtime(
    repo: &Repository,
    harness_id: &str,
    branch: &str,
    worktree: &Path,
) -> Result<Option<OpencodeRuntime>, String> {
    load_runtime_snapshot(repo, harness_id, branch, worktree)
}

pub fn load_runtime_snapshot(
    repo: &Repository,
    harness_id: &str,
    branch: &str,
    worktree: &Path,
) -> Result<Option<OpencodeRuntime>, String> {
    let repo_root = repo.root.display().to_string();
    let worktree_path = worktree.display().to_string();
    crate::persistence::session::load_runtime(
        &observability::db_path(repo),
        &repo_root,
        harness_id,
        branch,
        &worktree_path,
    )
    .map_err(|error| format!("read opencode runtime: {error}"))
}

fn load_runtimes_for_harness(
    repo: &Repository,
    harness_id: &str,
) -> Result<Vec<OpencodeRuntime>, String> {
    let repo_root = repo.root.display().to_string();
    crate::persistence::session::list_runtimes_for_harness(
        &observability::db_path(repo),
        &repo_root,
        harness_id,
    )
    .map_err(|error| format!("read OpenCode servers: {error}"))
}

pub(crate) fn load_runtimes_for_worktree_session(
    repo: &Repository,
    branch: &str,
    worktree: &Path,
) -> Result<Vec<OpencodeRuntime>, String> {
    let repo_root = repo.root.display().to_string();
    let worktree_path = worktree.display().to_string();
    crate::persistence::session::list_runtimes_for_worktree(
        &observability::db_path(repo),
        &repo_root,
        branch,
        &worktree_path,
    )
    .map_err(|error| format!("read opencode runtime: {error}"))
}

pub fn save_runtime(repo: &Repository, runtime: &OpencodeRuntime) -> Result<(), String> {
    crate::persistence::session::save_runtime(&observability::db_path(repo), runtime)
        .map_err(|error| format!("write opencode runtime: {error}"))
}

fn save_shared_server_runtime(repo: &Repository, runtime: &OpencodeRuntime) -> Result<(), String> {
    crate::persistence::session::save_shared_server_runtime(&observability::db_path(repo), runtime)
        .map_err(|error| format!("write shared OpenCode runtime: {error}"))
}

pub(crate) fn reconcile_session_refresh(
    current: &mut Option<OpencodeStatus>,
    previous: Option<OpencodeStatus>,
) {
    *current = previous;
}

pub(crate) fn shutdown_worktree_session_runtimes(
    repo: &Repository,
    branch: &str,
    worktree: &Path,
) -> Result<(), String> {
    let _server_lock = lock_repository_server(repo)?;
    let runtimes = load_runtimes_for_worktree_session(repo, branch, worktree)?;
    let mut errors = Vec::new();
    for runtime in runtimes {
        if runtime.branch != branch || runtime.worktree_path != worktree.display().to_string() {
            continue;
        }
        let references = match server_reference_count(repo, &runtime) {
            Ok(references) => references,
            Err(error) => {
                errors.push(error);
                continue;
            }
        };
        if references <= 1
            && let Err(error) = shutdown_stored_server(&runtime)
        {
            errors.push(error);
            continue;
        }
        let result =
            crate::persistence::session::delete_runtime(&observability::db_path(repo), &runtime)
                .map_err(|error| format!("remove shut down OpenCode runtime: {error}"));
        if let Err(error) = result {
            errors.push(error);
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

pub(crate) fn shutdown_worktree_session_runtime_processes_with_lock_held(
    repo: &Repository,
    runtimes: &[OpencodeRuntime],
) -> Result<(), String> {
    let mut seen = BTreeMap::new();
    for runtime in runtimes {
        seen.entry(runtime.server_url.as_str()).or_insert(runtime);
    }
    let mut errors = Vec::new();
    for runtime in seen.into_values() {
        let removed_references = runtimes
            .iter()
            .filter(|candidate| candidate.server_url == runtime.server_url)
            .count() as i64;
        match server_reference_count(repo, runtime) {
            Ok(references) if references <= removed_references => {
                if let Err(error) = shutdown_stored_server(runtime) {
                    errors.push(error);
                }
            }
            Ok(_) => {}
            Err(error) => errors.push(error),
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

pub fn allocate_port(
    repo_root: &str,
    worktree_path: &str,
    stored_port: Option<u16>,
    port_base: u16,
    port_span: u16,
    mut status: impl FnMut(u16) -> PortStatus,
) -> Result<u16, String> {
    if let Some(port) = stored_port
        && matches!(status(port), PortStatus::Free | PortStatus::OpenCode)
    {
        return Ok(port);
    }

    let span = port_span.max(1);
    let offset = stable_hash_text(&format!("{repo_root}{worktree_path}")) % u64::from(span);
    let start = port_base
        .checked_add(u16::try_from(offset).unwrap_or_default())
        .ok_or_else(|| "opencode port base overflowed".to_string())?;
    for step in 0..span {
        let Some(port) = start.checked_add(step) else {
            break;
        };
        if matches!(status(port), PortStatus::Free) {
            return Ok(port);
        }
    }
    Err(format!(
        "no free opencode port found from {start} through {}",
        start.saturating_add(span - 1)
    ))
}

pub fn server_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

pub fn check_health(server_url: &str) -> bool {
    get(
        "opencode.health",
        server_url,
        "/global/health",
        HEALTH_TIMEOUT,
    )
    .map(|response| response.status_code == 200)
    .unwrap_or(false)
}

pub fn port_status(port: u16) -> PortStatus {
    let url = server_url(port);
    if check_health(&url) {
        return PortStatus::OpenCode;
    }
    if tcp_connects(port, HEALTH_TIMEOUT) {
        PortStatus::Occupied
    } else {
        PortStatus::Free
    }
}

fn wait_for_health(server_url: &str) -> Result<(), String> {
    let started = std::time::Instant::now();
    while started.elapsed() < SERVER_START_TIMEOUT {
        if check_health(server_url) {
            return Ok(());
        }
        std::thread::sleep(SERVER_START_POLL);
    }
    Err(format!(
        "opencode server did not become healthy at {server_url}"
    ))
}

fn tcp_connects(port: u16, timeout: Duration) -> bool {
    let Ok(mut addresses) = ("127.0.0.1", port).to_socket_addrs() else {
        return false;
    };
    let Some(address) = addresses.next() else {
        return false;
    };
    TcpStream::connect_timeout(&address, timeout).is_ok()
}

fn get(
    name: &'static str,
    server_url: &str,
    path: &str,
    timeout: Duration,
) -> Result<HttpResponse, String> {
    request(name, server_url, "GET", path, None, timeout)
}

fn post(
    name: &'static str,
    server_url: &str,
    path: &str,
    body: &str,
    timeout: Duration,
) -> Result<HttpResponse, String> {
    request(name, server_url, "POST", path, Some(body), timeout)
}

fn success_status(status_code: u16) -> bool {
    (200..300).contains(&status_code)
}

fn http_error_message(operation: &str, status_code: u16, body: &str) -> String {
    let body = body.trim();
    if body.is_empty() {
        return format!("{operation} failed with HTTP {status_code}");
    }
    let body = if body.len() > 240 {
        format!("{}...", &body[..240])
    } else {
        body.to_string()
    };
    format!("{operation} failed with HTTP {status_code}: {body}")
}

fn request(
    name: &'static str,
    server_url: &str,
    method: &'static str,
    path: &str,
    body: Option<&str>,
    timeout: Duration,
) -> Result<HttpResponse, String> {
    let mut trace = crate::flight_recorder::ExternalCallTrace::begin(
        crate::flight_recorder::ExternalCallCategory::Http,
        name,
        vec![
            crate::flight_recorder::text("method", method),
            crate::flight_recorder::unsigned("timeout_ms", timeout.as_millis()),
        ],
    );
    let result = request_inner(server_url, method, path, body, timeout);
    match result {
        Ok((response, metrics)) => {
            let outcome = if success_status(response.status_code) {
                crate::flight_recorder::ExternalCallOutcome::Success
            } else {
                crate::flight_recorder::ExternalCallOutcome::Failed
            };
            let mut fields = metrics.fields();
            fields.push(crate::flight_recorder::unsigned(
                "status_code",
                response.status_code,
            ));
            if !success_status(response.status_code) {
                fields.push(crate::flight_recorder::text("error_kind", "http_status"));
            }
            trace.finish(outcome, fields);
            Ok(response)
        }
        Err(failure) => {
            let outcome = if failure.timed_out {
                crate::flight_recorder::ExternalCallOutcome::TimedOut
            } else {
                crate::flight_recorder::ExternalCallOutcome::Failed
            };
            let mut fields = failure.metrics.fields();
            fields.push(crate::flight_recorder::text(
                "error_kind",
                failure.error_kind,
            ));
            if let Some(status_code) = failure.status_code {
                fields.push(crate::flight_recorder::unsigned("status_code", status_code));
            }
            trace.finish(outcome, fields);
            Err(failure.message)
        }
    }
}

#[derive(Clone, Default)]
struct HttpMetrics {
    resolve_us: Option<u128>,
    connect_us: Option<u128>,
    write_us: Option<u128>,
    first_byte_us: Option<u128>,
    read_us: Option<u128>,
    request_bytes: usize,
    response_bytes: usize,
}

impl HttpMetrics {
    fn fields(&self) -> Vec<crate::flight_recorder::Field> {
        let mut fields = vec![
            crate::flight_recorder::unsigned("request_bytes", self.request_bytes),
            crate::flight_recorder::unsigned("response_bytes", self.response_bytes),
        ];
        for (name, value) in [
            ("resolve_us", self.resolve_us),
            ("connect_us", self.connect_us),
            ("write_us", self.write_us),
            ("first_byte_us", self.first_byte_us),
            ("read_us", self.read_us),
        ] {
            if let Some(value) = value {
                fields.push(crate::flight_recorder::unsigned(name, value));
            }
        }
        fields
    }
}

struct HttpFailure {
    message: String,
    error_kind: &'static str,
    timed_out: bool,
    status_code: Option<u16>,
    metrics: HttpMetrics,
}

fn http_failure(
    message: String,
    error_kind: &'static str,
    error: Option<&std::io::Error>,
    status_code: Option<u16>,
    metrics: &HttpMetrics,
) -> Box<HttpFailure> {
    let timed_out = error.is_some_and(is_timeout);
    Box::new(HttpFailure {
        message,
        error_kind: if timed_out { "timeout" } else { error_kind },
        timed_out,
        status_code,
        metrics: metrics.clone(),
    })
}

fn request_inner(
    server_url: &str,
    method: &'static str,
    path: &str,
    body: Option<&str>,
    timeout: Duration,
) -> Result<(HttpResponse, HttpMetrics), Box<HttpFailure>> {
    let mut metrics = HttpMetrics::default();
    let resolve_started = Instant::now();
    let (host, port) = parse_localhost_url(server_url)
        .map_err(|message| http_failure(message, "parse", None, None, &metrics))?;
    let address = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|error| {
            http_failure(
                format!("resolve {server_url}: {error}"),
                "resolve",
                Some(&error),
                None,
                &metrics,
            )
        })?
        .next()
        .ok_or_else(|| {
            http_failure(
                format!("resolve {server_url}: no address"),
                "resolve",
                None,
                None,
                &metrics,
            )
        })?;
    metrics.resolve_us = Some(resolve_started.elapsed().as_micros());

    let connect_started = Instant::now();
    let mut stream = TcpStream::connect_timeout(&address, timeout).map_err(|error| {
        http_failure(
            format!("connect {server_url}: {error}"),
            "connect",
            Some(&error),
            None,
            &metrics,
        )
    })?;
    metrics.connect_us = Some(connect_started.elapsed().as_micros());
    stream.set_read_timeout(Some(timeout)).map_err(|error| {
        http_failure(
            format!("configure read timeout: {error}"),
            "read",
            Some(&error),
            None,
            &metrics,
        )
    })?;
    stream.set_write_timeout(Some(timeout)).map_err(|error| {
        http_failure(
            format!("configure write timeout: {error}"),
            "write",
            Some(&error),
            None,
            &metrics,
        )
    })?;

    let request = match body {
        Some(body) => format!(
            "{method} {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ),
        None => {
            format!("{method} {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n")
        }
    };
    metrics.request_bytes = request.len();
    let write_started = Instant::now();
    stream.write_all(request.as_bytes()).map_err(|error| {
        http_failure(
            format!("write HTTP request: {error}"),
            "write",
            Some(&error),
            None,
            &metrics,
        )
    })?;
    metrics.write_us = Some(write_started.elapsed().as_micros());

    let read_started = Instant::now();
    let mut first_byte_at = None;
    let mut response = Vec::new();
    loop {
        let mut buffer = [0_u8; 8192];
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                if first_byte_at.is_none() {
                    let now = Instant::now();
                    metrics.first_byte_us = Some(read_started.elapsed().as_micros());
                    first_byte_at = Some(now);
                }
                response.extend_from_slice(&buffer[..count]);
                metrics.response_bytes = response.len();
                if http_response_is_complete(&response) {
                    break;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                metrics.response_bytes = response.len();
                metrics.read_us = first_byte_at.map(|started| started.elapsed().as_micros());
                let status_code = response_status_code(&response);
                return Err(http_failure(
                    format!("read HTTP response: {error}"),
                    "read",
                    Some(&error),
                    status_code,
                    &metrics,
                ));
            }
        }
    }
    metrics.read_us = first_byte_at.map(|started| started.elapsed().as_micros());
    let response_text = String::from_utf8_lossy(&response);
    let parsed = parse_response(&response_text).map_err(|message| {
        http_failure(
            message,
            "parse",
            None,
            response_status_code(&response),
            &metrics,
        )
    })?;
    Ok((parsed, metrics))
}

fn response_status_code(response: &[u8]) -> Option<u16> {
    let line_end = response.windows(2).position(|window| window == b"\r\n")?;
    std::str::from_utf8(&response[..line_end])
        .ok()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

fn http_response_is_complete(response: &[u8]) -> bool {
    let Some(headers_end) = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
    else {
        return false;
    };
    let headers = String::from_utf8_lossy(&response[..headers_end]);
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse::<u16>().ok());
    if status.is_some_and(|status| status == 204 || status == 304) {
        return true;
    }
    if header_value(&headers, "transfer-encoding")
        .is_some_and(|value| value.eq_ignore_ascii_case("chunked"))
    {
        return decode_chunked_body(&response[headers_end..]).is_some();
    }
    let content_length = headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>().ok())
            .flatten()
    });
    content_length.is_some_and(|length| response.len() >= headers_end + length)
}

fn header_value<'a>(headers: &'a str, expected: &str) -> Option<&'a str> {
    headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case(expected).then(|| value.trim())
    })
}

fn decode_chunked_body(body: &[u8]) -> Option<String> {
    let mut decoded = Vec::new();
    let mut position = 0;
    loop {
        let line_end = body[position..]
            .windows(2)
            .position(|window| window == b"\r\n")?
            + position;
        let size_text = std::str::from_utf8(&body[position..line_end]).ok()?;
        let size = usize::from_str_radix(size_text.split(';').next()?.trim(), 16).ok()?;
        position = line_end + 2;
        if size == 0 {
            let trailers = body.get(position..)?;
            let complete = trailers.starts_with(b"\r\n")
                || trailers.windows(4).any(|window| window == b"\r\n\r\n");
            return complete.then(|| String::from_utf8_lossy(&decoded).to_string());
        }
        let chunk_end = position.checked_add(size)?;
        decoded.extend_from_slice(body.get(position..chunk_end)?);
        if body.get(chunk_end..chunk_end + 2)? != b"\r\n" {
            return None;
        }
        position = chunk_end + 2;
    }
}

pub(crate) fn parse_localhost_url(url: &str) -> Result<(String, u16), String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("unsupported opencode URL: {url}"))?;
    let authority = rest.split('/').next().unwrap_or(rest);
    let (host, port) = authority
        .rsplit_once(':')
        .ok_or_else(|| format!("opencode URL missing port: {url}"))?;
    if host != "127.0.0.1" && host != "localhost" {
        return Err(format!("opencode URL must be local: {url}"));
    }
    let port = port
        .parse::<u16>()
        .map_err(|error| format!("parse opencode URL port: {error}"))?;
    Ok((host.to_string(), port))
}

struct HttpResponse {
    status_code: u16,
    body: String,
}

fn parse_response(response: &str) -> Result<HttpResponse, String> {
    let status_line = response
        .lines()
        .next()
        .ok_or_else(|| "empty HTTP response".to_string())?;
    let status_code = status_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| format!("invalid HTTP status line: {status_line}"))?
        .parse::<u16>()
        .map_err(|error| format!("parse HTTP status: {error}"))?;
    let (headers, raw_body) = response.split_once("\r\n\r\n").unwrap_or((response, ""));
    let body = if header_value(headers, "transfer-encoding")
        .is_some_and(|value| value.eq_ignore_ascii_case("chunked"))
    {
        decode_chunked_body(raw_body.as_bytes())
            .ok_or_else(|| "invalid chunked HTTP response".to_string())?
    } else {
        raw_body.to_string()
    };
    Ok(HttpResponse { status_code, body })
}

fn parse_sessions(body: &str) -> Vec<OpencodeSession> {
    let Some(value) = parse_json_value(body) else {
        return Vec::new();
    };
    collection_items(&value, &["data", "sessions", "items"])
        .into_iter()
        .filter_map(parse_session_object)
        .collect()
}

fn parse_session(body: &str) -> Option<OpencodeSession> {
    let value = parse_json_value(body)?;
    let object = object_field(&value, &["data", "session"]).unwrap_or(&value);
    parse_session_object(object)
}

fn parse_session_object(object: &Value) -> Option<OpencodeSession> {
    let id = string_field(object, &["id", "sessionID"])?;
    let time_updated =
        string_field(object, &["timeUpdated", "updatedAt", "updated_at"]).or_else(|| {
            object
                .get("time")
                .and_then(|time| time.get("updated").or_else(|| time.get("updatedAt")))
                .and_then(|updated| {
                    updated
                        .as_str()
                        .map(str::to_string)
                        .or_else(|| updated.as_u64().map(|value| value.to_string()))
                })
        });
    Some(OpencodeSession {
        id,
        directory: string_field(object, &["directory", "cwd", "path"]),
        title: string_field(object, &["title"]),
        time_updated,
        parent_id: string_field(object, &["parentID", "parentId", "parent_id"]),
    })
}

fn parse_session_state(body: &str, session_id: &str) -> Option<OpencodeState> {
    let value = parse_json_value(body)?;
    let objects = collection_items(&value, &["data", "sessions", "items"]);
    if !objects.is_empty() {
        for object in objects {
            let object_session_id = session_id_field(object);
            if object_session_id
                .as_deref()
                .is_none_or(|id| id == session_id)
                && let Some(state) = string_field(object, &["status", "state"])
                    .and_then(|value| parse_state_label(&value))
            {
                return Some(state);
            }
        }
        return None;
    }

    if let Some(object) = value.get(session_id).filter(|value| value.is_object()) {
        return string_field(object, &["status", "state"])
            .and_then(|value| parse_state_label(&value));
    }
    value
        .get(session_id)
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| string_field(&value, &["status", "state"]))
        .and_then(|value| parse_state_label(&value))
}

fn has_pending_permission(body: &str, session_id: &str) -> bool {
    let Some(value) = parse_json_value(body) else {
        return false;
    };
    collection_items(&value, &["data", "permissions", "items"])
        .into_iter()
        .any(|permission| session_id_field(permission).as_deref() == Some(session_id))
}

fn parse_state_label(value: &str) -> Option<OpencodeState> {
    OpencodeState::parse(value)
}

fn event_type_state(event_type: &str) -> Option<OpencodeState> {
    match event_type {
        "session.idle" => Some(OpencodeState::Idle),
        "session.error" => Some(OpencodeState::Error),
        "permission.asked" | "permission.updated" => Some(OpencodeState::NeedsInput),
        _ => None,
    }
}

fn session_id_field(object: &Value) -> Option<String> {
    string_field(object, &["sessionID", "sessionId", "session_id", "id"])
}

fn parse_message_summary(body: &str) -> MessageSummary {
    let Some(value) = parse_json_value(body) else {
        return MessageSummary::default();
    };
    let mut summary = MessageSummary::default();
    for object in collection_items(&value, &["data", "messages", "items"])
        .into_iter()
        .rev()
    {
        if summary.latest_turn_state.is_none() {
            summary.latest_turn_state = stored_message_turn_state(object);
            summary.latest_error = stored_message_error(object);
        }
        if summary.recent_messages.len() < 5
            && let Some(text) = assistant_message_text(object)
        {
            if summary.latest_message.is_none() {
                summary.latest_message = Some(text.clone());
            }
            summary.recent_messages.push(text);
        }
        if summary.latest_user_message.is_none()
            && let Some(text) = role_message_text(object, "user")
        {
            summary.latest_user_message = Some(text);
        }
        if summary.active_tool.is_none()
            && is_active_tool(object)
            && let Some(tool) = tool_label(object)
        {
            summary.active_tool = Some(tool);
        }
        if let Some(parts) = object.get("parts").and_then(Value::as_array) {
            for part in parts.iter().rev() {
                if summary.active_tool.is_none()
                    && is_active_tool(part)
                    && let Some(tool) = tool_label(part)
                {
                    summary.active_tool = Some(tool);
                }
            }
        }
    }
    summary
}

fn stored_message_turn_state(object: &Value) -> Option<OpencodeState> {
    let info = object.get("info").unwrap_or(object);
    match string_field(info, &["role"]).as_deref()? {
        "user" => Some(OpencodeState::Busy),
        "assistant" => Some(assistant_turn_state(info)),
        _ => None,
    }
}

fn assistant_turn_state(info: &Value) -> OpencodeState {
    let completed = info
        .get("time")
        .and_then(|time| time.get("completed"))
        .is_some_and(|completed| completed.is_number());
    let finish = string_field(info, &["finish"]);
    if completed
        && !finish
            .as_deref()
            .is_some_and(|finish| matches!(finish, "tool-calls" | "unknown"))
    {
        OpencodeState::Done
    } else {
        OpencodeState::Busy
    }
}

fn stored_message_error(object: &Value) -> Option<String> {
    let info = object.get("info").unwrap_or(object);
    message_error_value(info)
}

fn message_turn_state(event_type: &str, object: &Value) -> Option<OpencodeState> {
    if event_type != "message.updated" {
        return None;
    }
    let info = object.get("info").unwrap_or(object);
    stored_message_turn_state(info)
}

fn message_error(event_type: &str, object: &Value) -> Option<String> {
    (event_type == "message.updated")
        .then(|| object.get("info").unwrap_or(object))
        .and_then(message_error_value)
}

fn message_error_value(info: &Value) -> Option<String> {
    let error = info.get("error")?;
    string_field(error, &["name", "message"]).or_else(|| error.as_str().map(str::to_string))
}

fn assistant_message_text(object: &Value) -> Option<String> {
    if is_assistant_like(object) {
        return message_text(object);
    }
    role_message_text(object, "assistant")
}

fn role_message_text(object: &Value, role: &str) -> Option<String> {
    let matches_role =
        |value: &Value| string_field(value, &["role"]).is_some_and(|value_role| value_role == role);
    if matches_role(object) {
        return message_text(object);
    }
    if !object.get("info").is_some_and(matches_role) {
        return None;
    }
    let text = object
        .get("parts")
        .and_then(Value::as_array)?
        .iter()
        .filter(|part| is_assistant_like(part))
        .filter_map(message_text)
        .collect::<Vec<_>>()
        .join(" ");
    (!text.is_empty()).then_some(text)
}

fn is_assistant_like(object: &Value) -> bool {
    string_field(object, &["role"]).is_some_and(|role| role == "assistant")
        || string_field(object, &["type"]).is_some_and(|event_type| event_type.contains("text"))
        || string_field(object, &["partType"]).is_some_and(|part_type| part_type == "text")
}

fn message_text(object: &Value) -> Option<String> {
    string_field(object, &["text", "content", "message"])
        .map(|text| text.replace('\n', " ").trim().to_string())
        .filter(|text| !text.is_empty())
}

fn is_active_tool(object: &Value) -> bool {
    let type_is_tool = string_field(object, &["type", "partType"])
        .is_some_and(|event_type| event_type.contains("tool"));
    let status_is_active = tool_status(object)
        .map(|status| {
            matches!(
                status.as_str(),
                "running" | "pending" | "in_progress" | "in-progress" | "busy"
            )
        })
        .unwrap_or(true);
    type_is_tool && status_is_active
}

fn tool_label(object: &Value) -> Option<String> {
    let name = string_field(object, &["tool", "name", "title"])?;
    let status = tool_status(object);
    Some(match status {
        Some(status) if !status.is_empty() => format!("{name} {status}"),
        _ => name,
    })
}

fn tool_status(object: &Value) -> Option<String> {
    string_field(object, &["status", "state"]).or_else(|| {
        object
            .get("state")
            .filter(|state| state.is_object())
            .and_then(|state| string_field(state, &["status", "state"]))
    })
}

fn parse_todos(body: &str) -> Vec<OpencodeTodo> {
    let Some(value) = parse_json_value(body) else {
        return Vec::new();
    };
    parse_todos_value(&value)
}

fn parse_todos_value(value: &Value) -> Vec<OpencodeTodo> {
    collection_items(value, &["data", "todos", "items", "todo"])
        .into_iter()
        .filter_map(|object| {
            let text = string_field(object, &["content", "text", "title"])?;
            Some(OpencodeTodo {
                text: text.replace('\n', " ").trim().to_string(),
                status: string_field(object, &["status", "state"])
                    .unwrap_or_else(|| "pending".to_string()),
            })
        })
        .filter(|todo| !todo.text.is_empty())
        .collect()
}

fn parse_json_value(body: &str) -> Option<Value> {
    serde_json::from_str(body).ok()
}

fn collection_items<'a>(value: &'a Value, envelope_keys: &[&str]) -> Vec<&'a Value> {
    if let Value::Array(items) = value {
        return items.iter().filter(|item| item.is_object()).collect();
    }
    envelope_keys
        .iter()
        .find_map(|key| value.get(*key).and_then(Value::as_array))
        .map(|items| items.iter().filter(|item| item.is_object()).collect())
        .unwrap_or_default()
}

fn event_body(value: &Value) -> Option<&Value> {
    object_field(value, &["properties", "data", "session"])
}

fn object_field<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter()
        .find_map(|key| value.get(*key).filter(|value| value.is_object()))
}

fn string_field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::to_string)
}

fn newest_session_for_worktree<'a>(
    sessions: &'a [OpencodeSession],
    worktree_path: &str,
) -> Option<&'a OpencodeSession> {
    sessions
        .iter()
        .filter(|session| {
            session.parent_id.is_none() && listed_session_matches_worktree(session, worktree_path)
        })
        .max_by(|left, right| left.time_updated.cmp(&right.time_updated))
}

fn listed_session_matches_worktree(session: &OpencodeSession, worktree_path: &str) -> bool {
    session.directory.as_deref() == Some(worktree_path)
}

fn session_matches_worktree(session: &OpencodeSession, worktree_path: &str) -> bool {
    session
        .directory
        .as_deref()
        .is_none_or(|directory| directory == worktree_path)
}

fn request_path(path: &str, directory: Option<&Path>) -> String {
    let Some(directory) = directory else {
        return path.to_string();
    };
    let separator = if path.contains('?') { '&' } else { '?' };
    format!(
        "{path}{separator}directory={}",
        url_path_segment(&directory.display().to_string())
    )
}

fn url_path_segment(value: &str) -> String {
    let mut output = String::new();
    for byte in value.bytes() {
        let ch = byte as char;
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '~') {
            output.push(ch);
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn stable_hash_text(value: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;

    use super::*;

    #[test]
    fn server_url_maps_port_to_local_http_url() {
        assert_eq!(server_url(41_234), "http://127.0.0.1:41234");
    }

    #[test]
    fn event_listener_stops_when_canceled_while_receiver_is_idle() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (ready_tx, ready_rx) = mpsc::sync_channel(0);
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 512];
            let _ = stream.read(&mut request).unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n")
                .unwrap();
            stream.flush().unwrap();
            ready_tx.send(()).unwrap();
            let _ = release_rx.recv();
        });
        let canceled = Arc::new(AtomicBool::new(false));
        let listener_canceled = canceled.clone();
        let (result_tx, result_rx) = mpsc::sync_channel(0);
        std::thread::spawn(move || {
            let result = listen_events_until(
                &url,
                || listener_canceled.load(Ordering::Acquire),
                |_| Ok(()),
            );
            result_tx.send(result).unwrap();
        });

        ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        canceled.store(true, Ordering::Release);
        assert!(
            result_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .is_ok()
        );
        release_tx.send(()).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn event_listener_reports_an_idle_stream_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 512];
            let _ = stream.read(&mut request).unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n")
                .unwrap();
            stream.flush().unwrap();
            std::thread::sleep(Duration::from_millis(250));
        });

        let started = Instant::now();
        let error = listen_event_payloads_with_stop(
            &url,
            Duration::from_millis(100),
            Duration::from_millis(20),
            Duration::from_millis(80),
            &mut || false,
            &mut |_| Ok(()),
        )
        .unwrap_err();

        assert!(error.contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(1));
        server.join().unwrap();
    }

    #[test]
    fn event_listener_keeps_callback_errors_distinct_from_transport_errors() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let mut buffer = [0_u8; 256];
                let count = stream.read(&mut buffer).unwrap();
                assert!(count > 0, "client closed before completing request headers");
                request.extend_from_slice(&buffer[..count]);
            }
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\ndata: {}\r\n\r\n",
                )
                .unwrap();
        });
        let mut metrics = SseMetrics::default();

        let failure = listen_event_payloads_with_stop_inner(
            &url,
            SseRequest {
                path: "/event",
                connect_timeout: Duration::from_millis(100),
                read_poll_interval: Duration::from_millis(20),
                inactivity_timeout: Duration::from_millis(100),
            },
            &mut || false,
            &mut |_| Err("HTTP invalid closed application error".to_string()),
            &mut metrics,
        )
        .unwrap_err();

        assert_eq!(failure.kind, SseFailureKind::Callback);
        assert_eq!(failure.kind.error_kind(), None);
        assert_eq!(metrics.payload_count, 1);
        server.join().unwrap();
    }

    #[test]
    fn stored_server_args_match_requires_expected_host_and_port() {
        let args = [
            "/home/mockuser/.npm/bin/opencode",
            "serve",
            "--hostname",
            "127.0.0.1",
            "--port",
            "41234",
        ];

        assert!(stored_server_args_match(&args, 41_234));
        assert!(!stored_server_args_match(&args, 41_235));
        assert!(!stored_server_args_match(
            &[
                "/home/mockuser/.npm/bin/opencode",
                "serve",
                "--port",
                "41234"
            ],
            41_234,
        ));
    }

    #[test]
    fn stored_server_shutdown_reports_argument_inspection_failure() {
        let runtime = OpencodeRuntime {
            repo_root: "/repo".to_string(),
            harness_id: "opencode".to_string(),
            branch: "feature/test".to_string(),
            worktree_path: "/repo/worktree".to_string(),
            server_port: 41_234,
            server_url: "http://127.0.0.1:41234".to_string(),
            server_pid: Some(42),
            server_process_identity: Some(7),
            opencode_session_id: None,
            generation: 0,
            updated_unix_ms: 0,
        };

        let error = shutdown_stored_server_with(&runtime, |pid| {
            Err(crate::process::ProcessLifecycleError::Inspect {
                pid,
                source: std::io::Error::other("injected argument inspection failure"),
            })
        })
        .unwrap_err();

        assert!(error.contains("inspect stored opencode server 42 before shutdown"));
        assert!(error.contains("injected argument inspection failure"));
    }

    #[test]
    fn allocate_port_uses_stored_healthy_port() {
        let port = allocate_port(
            "/repo",
            "/repo/wt",
            Some(41_111),
            41_000,
            1_000,
            |candidate| {
                if candidate == 41_111 {
                    PortStatus::OpenCode
                } else {
                    PortStatus::Free
                }
            },
        )
        .unwrap();

        assert_eq!(port, 41_111);
    }

    #[test]
    fn allocate_port_skips_occupied_stored_port() {
        let derived = allocate_port("/repo", "/repo/wt", None, 41_000, 1_000, |_| {
            PortStatus::Free
        })
        .unwrap();
        let port = allocate_port(
            "/repo",
            "/repo/wt",
            Some(41_111),
            41_000,
            1_000,
            |candidate| {
                if candidate == 41_111 || candidate == derived {
                    PortStatus::Occupied
                } else {
                    PortStatus::Free
                }
            },
        )
        .unwrap();

        assert_eq!(port, derived + 1);
    }

    #[test]
    fn allocate_port_skips_unstored_open_code_port() {
        let derived = allocate_port("/repo", "/repo/wt", None, 41_000, 1_000, |_| {
            PortStatus::Free
        })
        .unwrap();
        let port = allocate_port("/repo", "/repo/wt", None, 41_000, 1_000, |candidate| {
            if candidate == derived {
                PortStatus::OpenCode
            } else {
                PortStatus::Free
            }
        })
        .unwrap();

        assert_eq!(port, derived + 1);
    }

    #[test]
    fn allocate_port_uses_configured_base_and_span() {
        let port =
            allocate_port("/repo", "/repo/wt", None, 45_000, 10, |_| PortStatus::Free).unwrap();

        assert!((45_000..45_010).contains(&port));
    }

    #[test]
    fn runtime_metadata_round_trips_session_mapping() {
        let temp = unique_temp_dir("prism-opencode-runtime-test");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let worktree = temp.join("feature");
        let runtime = OpencodeRuntime {
            repo_root: temp.display().to_string(),
            harness_id: "opencode".to_string(),
            branch: "feature".to_string(),
            worktree_path: worktree.display().to_string(),
            server_port: 41_222,
            server_url: server_url(41_222),
            server_pid: Some(123),
            server_process_identity: Some(456),
            opencode_session_id: Some("ses_123".to_string()),
            generation: 7,
            updated_unix_ms: 42,
        };

        save_runtime(&repo, &runtime).unwrap();
        let loaded = load_runtime(&repo, "opencode", "feature", &worktree)
            .unwrap()
            .unwrap();

        assert_eq!(loaded, runtime);
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn worktrees_in_one_repository_reuse_one_healthy_server() {
        let temp = unique_temp_dir("prism-opencode-shared-server-test");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let first_worktree = temp.join("first");
        let second_worktree = temp.join("second");
        let (server_url, stop, server) = start_health_server(first_worktree.clone());
        let server_port = parse_localhost_url(&server_url).unwrap().1;
        let first = OpencodeRuntime {
            repo_root: temp.display().to_string(),
            harness_id: "opencode".to_string(),
            branch: "feature/first".to_string(),
            worktree_path: first_worktree.display().to_string(),
            server_port,
            server_url: server_url.clone(),
            server_pid: None,
            server_process_identity: None,
            opencode_session_id: Some("ses_first".to_string()),
            generation: 0,
            updated_unix_ms: 42,
        };
        save_runtime(&repo, &first).unwrap();

        let second = ensure_opencode_server_with_program(
            &repo,
            &Config::load(&repo),
            "opencode",
            "feature/second",
            &second_worktree,
            "/definitely/missing/opencode",
        )
        .unwrap();

        assert_eq!(second.server_url, first.server_url);
        assert_eq!(second.server_port, first.server_port);
        assert_eq!(second.server_pid, first.server_pid);
        stop.store(true, Ordering::Release);
        server.join().unwrap();
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn legacy_worktree_servers_converge_to_one_canonical_server() {
        let temp = unique_temp_dir("prism-opencode-legacy-server-test");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let first_worktree = temp.join("first");
        let second_worktree = temp.join("second");
        let (first_url, first_stop, first_server) = start_health_server(first_worktree.clone());
        let (second_url, second_stop, second_server) = start_health_server(second_worktree.clone());
        let runtime =
            |branch: &str, worktree: &Path, server_url: String, session_id: &str| OpencodeRuntime {
                repo_root: temp.display().to_string(),
                harness_id: "opencode".to_string(),
                branch: branch.to_string(),
                worktree_path: worktree.display().to_string(),
                server_port: parse_localhost_url(&server_url).unwrap().1,
                server_url,
                server_pid: None,
                server_process_identity: None,
                opencode_session_id: Some(session_id.to_string()),
                generation: 0,
                updated_unix_ms: 42,
            };
        let first = runtime("feature/first", &first_worktree, first_url, "ses_first");
        let second = runtime("feature/second", &second_worktree, second_url, "ses_first");
        save_runtime(&repo, &first).unwrap();
        save_runtime(&repo, &second).unwrap();
        let canonical_url = [&first, &second]
            .into_iter()
            .min_by_key(|runtime| runtime.server_port)
            .unwrap()
            .server_url
            .clone();
        let noncanonical = [&first, &second]
            .into_iter()
            .find(|runtime| runtime.server_url != canonical_url)
            .unwrap();

        let selected = ensure_opencode_server_with_program(
            &repo,
            &Config::load(&repo),
            "opencode",
            &noncanonical.branch,
            Path::new(&noncanonical.worktree_path),
            "/definitely/missing/opencode",
        )
        .unwrap();

        assert_eq!(selected.server_url, canonical_url);
        for runtime in load_runtimes_for_harness(&repo, "opencode").unwrap() {
            assert_eq!(runtime.server_url, canonical_url);
        }
        first_stop.store(true, Ordering::Release);
        second_stop.store(true, Ordering::Release);
        first_server.join().unwrap();
        second_server.join().unwrap();
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn replacing_shared_server_updates_every_worktree_reference() {
        let temp = unique_temp_dir("prism-opencode-replacement-test");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let runtime = |branch: &str, worktree: &str, session_id: &str| OpencodeRuntime {
            repo_root: temp.display().to_string(),
            harness_id: "opencode".to_string(),
            branch: branch.to_string(),
            worktree_path: temp.join(worktree).display().to_string(),
            server_port: 41_000,
            server_url: server_url(41_000),
            server_pid: Some(100),
            server_process_identity: Some(200),
            opencode_session_id: Some(session_id.to_string()),
            generation: 1,
            updated_unix_ms: 42,
        };
        let first = runtime("feature/first", "first", "ses_first");
        let second = runtime("feature/second", "second", "ses_second");
        save_runtime(&repo, &first).unwrap();
        save_runtime(&repo, &second).unwrap();
        let replacement = OpencodeRuntime {
            server_port: 41_001,
            server_url: server_url(41_001),
            server_pid: Some(300),
            server_process_identity: Some(400),
            updated_unix_ms: 84,
            ..first.clone()
        };

        save_shared_server_runtime(&repo, &replacement).unwrap();

        let runtimes = load_runtimes_for_harness(&repo, "opencode").unwrap();
        assert_eq!(runtimes.len(), 2);
        for runtime in &runtimes {
            assert_eq!(runtime.server_port, replacement.server_port);
            assert_eq!(runtime.server_url, replacement.server_url);
            assert_eq!(runtime.server_pid, replacement.server_pid);
            assert_eq!(
                runtime.server_process_identity,
                replacement.server_process_identity
            );
        }
        let sessions = runtimes
            .iter()
            .map(|runtime| {
                (
                    runtime.branch.as_str(),
                    runtime.opencode_session_id.as_deref(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            sessions.get(first.branch.as_str()),
            Some(&Some("ses_first"))
        );
        assert_eq!(
            sessions.get(second.branch.as_str()),
            Some(&Some("ses_second"))
        );
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn replacing_shared_server_rolls_back_every_reference_on_failure() {
        let temp = unique_temp_dir("prism-opencode-replacement-rollback-test");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let runtime = |branch: &str, worktree: &str| OpencodeRuntime {
            repo_root: temp.display().to_string(),
            harness_id: "opencode".to_string(),
            branch: branch.to_string(),
            worktree_path: temp.join(worktree).display().to_string(),
            server_port: 41_000,
            server_url: server_url(41_000),
            server_pid: Some(100),
            server_process_identity: Some(200),
            opencode_session_id: None,
            generation: 1,
            updated_unix_ms: 42,
        };
        let first = runtime("feature/first", "first");
        let second = runtime("feature/second", "second");
        save_runtime(&repo, &first).unwrap();
        save_runtime(&repo, &second).unwrap();
        observability::with_writable_db(&repo, |path| {
            crate::persistence::session::test_install_shared_server_runtime_upsert_failure(path)
                .map_err(|error| error.to_string())
        })
        .unwrap();
        let replacement = OpencodeRuntime {
            server_port: 41_001,
            server_url: server_url(41_001),
            server_pid: Some(300),
            server_process_identity: Some(400),
            updated_unix_ms: 84,
            ..first.clone()
        };

        let error = save_shared_server_runtime(&repo, &replacement).unwrap_err();

        assert!(error.contains("forced runtime upsert failure"));
        for runtime in load_runtimes_for_harness(&repo, "opencode").unwrap() {
            assert_eq!(runtime.server_port, first.server_port);
            assert_eq!(runtime.server_url, first.server_url);
            assert_eq!(runtime.server_pid, first.server_pid);
            assert_eq!(
                runtime.server_process_identity,
                first.server_process_identity
            );
        }
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn runtime_identity_is_isolated_by_harness_id() {
        let temp = unique_temp_dir("prism-opencode-runtime-harness-test");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let worktree = temp.join("feature");
        for (harness_id, port, session_id) in [
            ("opencode-a", 41_222, "ses_a"),
            ("opencode-b", 41_223, "ses_b"),
        ] {
            save_runtime(
                &repo,
                &OpencodeRuntime {
                    repo_root: temp.display().to_string(),
                    harness_id: harness_id.to_string(),
                    branch: "feature".to_string(),
                    worktree_path: worktree.display().to_string(),
                    server_port: port,
                    server_url: server_url(port),
                    server_pid: None,
                    server_process_identity: None,
                    opencode_session_id: Some(session_id.to_string()),
                    generation: 1,
                    updated_unix_ms: 42,
                },
            )
            .unwrap();
        }

        assert_eq!(
            load_runtime(&repo, "opencode-a", "feature", &worktree)
                .unwrap()
                .unwrap()
                .opencode_session_id
                .as_deref(),
            Some("ses_a")
        );
        assert_eq!(
            load_runtime(&repo, "opencode-b", "feature", &worktree)
                .unwrap()
                .unwrap()
                .opencode_session_id
                .as_deref(),
            Some("ses_b")
        );
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn legacy_runtime_without_start_time_cannot_stop_a_matching_live_process() {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("while :; do sleep 1; done")
            .arg("legacy-opencode-fixture")
            .args(["serve", "--hostname", "127.0.0.1", "--port", "41222"])
            .spawn()
            .unwrap();
        let runtime = OpencodeRuntime {
            repo_root: "/repo".to_string(),
            harness_id: "opencode".to_string(),
            branch: "feature".to_string(),
            worktree_path: "/repo/feature".to_string(),
            server_port: 41_222,
            server_url: server_url(41_222),
            server_pid: Some(child.id()),
            server_process_identity: None,
            opencode_session_id: Some("ses_old".to_string()),
            generation: 2,
            updated_unix_ms: 42,
        };

        let result = shutdown_stored_server_with(&runtime, |_| {
            Ok(Some(vec![
                "legacy-opencode-fixture".to_string(),
                "serve".to_string(),
                "--hostname".to_string(),
                "127.0.0.1".to_string(),
                "--port".to_string(),
                "41222".to_string(),
            ]))
        });
        let child_was_running = child.try_wait().unwrap().is_none();
        child.kill().unwrap();
        child.wait().unwrap();

        assert_eq!(
            result.unwrap_err(),
            format!(
                "refusing to stop opencode server {}: reusable process identity is unavailable",
                child.id()
            )
        );
        assert!(child_was_running);
    }

    #[test]
    fn parse_sessions_accepts_top_level_array() {
        let sessions = parse_sessions(
            r#"[
                {"id":"ses_old","directory":"/repo/wt","title":"old","timeUpdated":"2026-01-01T00:00:00Z"},
                {"id":"ses_new","directory":"/repo/wt","title":"new","timeUpdated":"2026-01-02T00:00:00Z"}
            ]"#,
        );

        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id, "ses_old");
        assert_eq!(sessions[1].directory.as_deref(), Some("/repo/wt"));
    }

    #[test]
    fn parse_sessions_accepts_data_envelope() {
        let sessions = parse_sessions(
            r#"{"data":[{"id":"ses_1","path":"/repo/wt","updatedAt":"2026-01-01T00:00:00Z"}]}"#,
        );

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "ses_1");
        assert_eq!(sessions[0].directory.as_deref(), Some("/repo/wt"));
    }

    #[test]
    fn parse_sessions_reads_nested_update_time_and_ignores_newer_child_session() {
        let sessions = parse_sessions(
            r#"[
                {"id":"current","directory":"/repo/wt","time":{"updated":200}},
                {"id":"child","directory":"/repo/wt","parentID":"current","time":{"updated":300}},
                {"id":"old","directory":"/repo/wt","time":{"updated":100}}
            ]"#,
        );

        let selected = newest_session_for_worktree(&sessions, "/repo/wt").unwrap();

        assert_eq!(selected.id, "current");
        assert_eq!(selected.time_updated.as_deref(), Some("200"));
    }

    #[test]
    fn parse_session_accepts_session_envelope_and_session_id_field() {
        let session = parse_session(
            r#"{"session":{"sessionID":"ses_1","cwd":"/repo/wt","title":"feature"}}"#,
        )
        .unwrap();

        assert_eq!(session.id, "ses_1");
        assert_eq!(session.directory.as_deref(), Some("/repo/wt"));
        assert_eq!(session.title.as_deref(), Some("feature"));
    }

    #[test]
    fn newest_session_for_worktree_prefers_latest_matching_update_time() {
        let sessions = vec![
            OpencodeSession {
                id: "wrong".to_string(),
                directory: Some("/repo/other".to_string()),
                title: None,
                time_updated: Some("2026-01-03T00:00:00Z".to_string()),
                parent_id: None,
            },
            OpencodeSession {
                id: "old".to_string(),
                directory: Some("/repo/wt".to_string()),
                title: None,
                time_updated: Some("2026-01-01T00:00:00Z".to_string()),
                parent_id: None,
            },
            OpencodeSession {
                id: "new".to_string(),
                directory: Some("/repo/wt".to_string()),
                title: None,
                time_updated: Some("2026-01-02T00:00:00Z".to_string()),
                parent_id: None,
            },
        ];

        let selected = newest_session_for_worktree(&sessions, "/repo/wt").unwrap();

        assert_eq!(selected.id, "new");
    }

    #[test]
    fn newest_session_for_worktree_ignores_sessions_without_matching_directory() {
        let sessions = vec![
            OpencodeSession {
                id: "old".to_string(),
                directory: Some("/repo/wt".to_string()),
                title: None,
                time_updated: Some("2026-01-01T00:00:00Z".to_string()),
                parent_id: None,
            },
            OpencodeSession {
                id: "new_without_directory".to_string(),
                directory: None,
                title: None,
                time_updated: Some("2026-01-03T00:00:00Z".to_string()),
                parent_id: None,
            },
            OpencodeSession {
                id: "new_other_worktree".to_string(),
                directory: Some("/repo/other".to_string()),
                title: None,
                time_updated: Some("2026-01-04T00:00:00Z".to_string()),
                parent_id: None,
            },
        ];

        let selected = newest_session_for_worktree(&sessions, "/repo/wt").unwrap();

        assert_eq!(selected.id, "old");
    }

    #[test]
    fn resolve_session_prefers_newer_worktree_session_over_stored_session() {
        let worktree = PathBuf::from("/repo/wt");
        let server_url = start_session_resolution_server();
        let runtime = OpencodeRuntime {
            repo_root: "/repo".to_string(),
            harness_id: "opencode".to_string(),
            branch: "feature".to_string(),
            worktree_path: worktree.display().to_string(),
            server_port: 41_234,
            server_url,
            server_pid: None,
            server_process_identity: None,
            opencode_session_id: Some("old".to_string()),
            generation: 0,
            updated_unix_ms: 0,
        };

        let selected = resolve_session(&runtime, &worktree).unwrap();

        assert_eq!(selected.id, "new");
    }

    #[test]
    fn refresh_session_keeps_runtime_when_session_listing_fails() {
        let temp = unique_temp_dir("prism-opencode-refresh-offline-test");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let worktree = temp.join("feature");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let runtime = OpencodeRuntime {
            repo_root: temp.display().to_string(),
            harness_id: "opencode".to_string(),
            branch: "feature".to_string(),
            worktree_path: worktree.display().to_string(),
            server_port: port,
            server_url: server_url(port),
            server_pid: None,
            server_process_identity: None,
            opencode_session_id: Some("stored".to_string()),
            generation: 3,
            updated_unix_ms: 42,
        };

        let refreshed = refresh_opencode_session(&repo, runtime.clone(), &worktree).unwrap();

        assert_eq!(refreshed, runtime);
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn url_path_segment_percent_encodes_non_segment_bytes() {
        assert_eq!(url_path_segment("session/id 1"), "session%2Fid%201");
        assert_eq!(url_path_segment("ses_1-2.3~4"), "ses_1-2.3~4");
    }

    #[test]
    fn request_path_routes_requests_to_the_worktree_directory() {
        let directory = Path::new("/repo/work tree");

        assert_eq!(
            request_path("/session/status", Some(directory)),
            "/session/status?directory=%2Frepo%2Fwork%20tree"
        );
        assert_eq!(
            request_path("/session/ses_1/message?limit=10", Some(directory)),
            "/session/ses_1/message?limit=10&directory=%2Frepo%2Fwork%20tree"
        );
        assert_eq!(request_path("/global/health", None), "/global/health");
    }

    #[test]
    fn create_session_routes_request_to_worktree_directory() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let server_url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            let mut content_length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    request.push_str(&line);
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = value.trim().parse().unwrap();
                }
                request.push_str(&line);
            }
            let mut request_body = vec![0; content_length];
            reader.read_exact(&mut request_body).unwrap();
            request.push_str(&String::from_utf8_lossy(&request_body));
            drop(reader);
            let body = r#"{"id":"ses_1","directory":"/repo/work tree","title":"feature"}"#;
            let response = format!(
                "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
            request
        });

        let created = create_session(&server_url, Path::new("/repo/work tree"), "feature").unwrap();
        let request = server.join().unwrap();

        assert_eq!(created.id, "ses_1");
        assert!(
            request.starts_with("POST /session?directory=%2Frepo%2Fwork%20tree HTTP/1.1"),
            "{request}"
        );
        assert!(request.contains(r#"{"title":"feature"}"#));
        assert!(!request.contains(r#""directory""#));
    }

    #[test]
    fn async_prompt_body_escapes_text() {
        assert_eq!(
            prompt_async_body("  hello world\n\"quotes\" and $PATH && true\n--leading-dash"),
            r#"{"parts":[{"type":"text","text":"  hello world\n\"quotes\" and $PATH && true\n--leading-dash"}]}"#
        );
    }

    #[test]
    fn parses_status_messages_tools_and_todos() {
        assert_eq!(
            parse_session_state(
                r#"{"data":[{"sessionID":"ses_other","status":"idle"},{"sessionID":"ses_1","status":"busy"}]}"#,
                "ses_1"
            ),
            Some(OpencodeState::Busy)
        );

        let summary = parse_message_summary(
            r#"{"data":[
                {"role":"assistant","text":"first\nreply"},
                {"type":"tool","name":"bash","status":"running"}
            ]}"#,
        );
        assert_eq!(summary.latest_message.as_deref(), Some("first reply"));
        assert_eq!(summary.active_tool.as_deref(), Some("bash running"));

        assert!(has_pending_permission(
            r#"[{"id":"per_1","sessionID":"ses_1","permission":"read"}]"#,
            "ses_1"
        ));
        assert!(!has_pending_permission(
            r#"[{"id":"per_1","sessionID":"ses_other","permission":"read"}]"#,
            "ses_1"
        ));

        let summary = parse_message_summary(
            r#"[
                {"info":{"role":"user"},"parts":[{"type":"text","text":"question"}]},
                {"info":{"role":"assistant"},"parts":[
                    {"type":"text","text":"latest\nreply"},
                    {"type":"tool","tool":"bash","state":{"status":"completed"}}
                ]}
            ]"#,
        );
        assert_eq!(summary.latest_message.as_deref(), Some("latest reply"));
        assert_eq!(summary.latest_user_message.as_deref(), Some("question"));
        assert_eq!(summary.recent_messages, vec!["latest reply"]);
        assert_eq!(summary.active_tool, None);

        let completed = parse_message_summary(
            r#"[{"info":{"sessionID":"ses_1","role":"assistant","time":{"created":1,"completed":2},"finish":"stop"},"parts":[{"type":"text","text":"done"}]}]"#,
        );
        assert_eq!(completed.latest_turn_state, Some(OpencodeState::Done));
        assert_eq!(completed.latest_error, None);

        let aborted = parse_message_summary(
            r#"[{"info":{"sessionID":"ses_1","role":"assistant","time":{"created":1,"completed":2},"error":{"name":"MessageAbortedError"}},"parts":[]}]"#,
        );
        assert_eq!(aborted.latest_turn_state, Some(OpencodeState::Done));
        assert_eq!(aborted.latest_error.as_deref(), Some("MessageAbortedError"));

        let continuing = parse_message_summary(
            r#"[{"info":{"sessionID":"ses_1","role":"assistant","time":{"created":1,"completed":2},"finish":"tool-calls"},"parts":[]}]"#,
        );
        assert_eq!(continuing.latest_turn_state, Some(OpencodeState::Busy));

        let in_progress = parse_message_summary(
            r#"[{"info":{"sessionID":"ses_1","role":"assistant","time":{"created":1}},"parts":[]}]"#,
        );
        assert_eq!(in_progress.latest_turn_state, Some(OpencodeState::Busy));

        let todos = parse_todos(
            r#"{"todos":[
                {"content":"write code","status":"in_progress"},
                {"title":"run tests","state":"pending"}
            ]}"#,
        );
        assert_eq!(todos.len(), 2);
        assert_eq!(todos[0].text, "write code");
        assert_eq!(todos[1].status, "pending");
    }

    #[test]
    fn missing_session_status_means_the_session_is_idle() {
        assert_eq!(
            session_state_from_status_body(r#"{}"#, "ses_1"),
            OpencodeState::Idle
        );
        assert_eq!(
            session_state_from_status_body(r#"{"ses_other":{"status":"busy"}}"#, "ses_1"),
            OpencodeState::Idle
        );
    }

    #[test]
    fn parses_opencode_status_sse_event() {
        let event = parse_event_payload(
            r#"{"type":"session.status","properties":{"sessionID":"ses_1","status":"busy","title":"Feature"}}"#,
        )
        .unwrap();

        assert_eq!(event.session_id.as_deref(), Some("ses_1"));
        assert_eq!(event.state, Some(OpencodeState::Busy));
        assert_eq!(event.title.as_deref(), Some("Feature"));

        let event = parse_event_payload(
            r#"{"type":"session.status","properties":{"sessionID":"ses_1","status":{"type":"retry","attempt":2}}}"#,
        )
        .unwrap();
        assert_eq!(event.state, Some(OpencodeState::Retry));

        let event = parse_event_payload(
            r#"{"type":"permission.updated","properties":{"id":"per_1","sessionID":"ses_1","title":"Run command"}}"#,
        )
        .unwrap();
        assert_eq!(event.state, Some(OpencodeState::NeedsInput));

        let event = parse_event_payload(
            r#"{"type":"permission.asked","properties":{"id":"per_2","sessionID":"ses_1","permission":"read"}}"#,
        )
        .unwrap();
        assert_eq!(event.state, Some(OpencodeState::NeedsInput));
    }

    #[test]
    fn parses_opencode_message_tool_and_todo_events() {
        let message = parse_event_payload(
            r#"{"type":"message.part.updated","properties":{"sessionID":"ses_1","role":"assistant","text":"hello\nthere"}}"#,
        )
        .unwrap();
        assert_eq!(message.latest_message.as_deref(), Some("hello there"));

        let completed = parse_event_payload(
            r#"{"type":"message.updated","properties":{"info":{"sessionID":"ses_1","role":"assistant","time":{"created":1,"completed":2},"finish":"stop"}}}"#,
        )
        .unwrap();
        assert_eq!(completed.session_id.as_deref(), Some("ses_1"));
        assert_eq!(completed.state, Some(OpencodeState::Done));

        let aborted = parse_event_payload(
            r#"{"type":"message.updated","properties":{"info":{"sessionID":"ses_1","role":"assistant","time":{"created":1,"completed":2},"error":{"name":"MessageAbortedError"}}}}"#,
        )
        .unwrap();
        assert_eq!(aborted.state, Some(OpencodeState::Done));
        assert_eq!(aborted.detail.as_deref(), Some("MessageAbortedError"));

        let tool_calls = parse_event_payload(
            r#"{"type":"message.updated","properties":{"info":{"sessionID":"ses_1","role":"assistant","time":{"created":1,"completed":2},"finish":"tool-calls"}}}"#,
        )
        .unwrap();
        assert_eq!(tool_calls.state, Some(OpencodeState::Busy));

        let tool = parse_event_payload(
            r#"{"type":"tool.updated","properties":{"sessionID":"ses_1","name":"bash","status":"running"}}"#,
        )
        .unwrap();
        assert_eq!(tool.active_tool.as_deref(), Some("bash running"));

        let todo = parse_event_payload(
            r#"{"type":"todo.updated","properties":{"sessionID":"ses_1","todos":[{"content":"ship it","status":"in_progress"}]}}"#,
        )
        .unwrap();
        assert_eq!(todo.todos.unwrap()[0].text, "ship it");
    }

    #[test]
    fn classifies_only_supersedable_status_and_text_snapshots() {
        let (_, status_facet) = parse_event_payload_classified(
            r#"{"type":"session.status","properties":{"sessionID":"ses_1","status":"busy"}}"#,
        )
        .unwrap();
        let (_, message_facet) = parse_event_payload_classified(
            r#"{"type":"message.part.updated","properties":{"sessionID":"ses_1","role":"assistant","text":"latest"}}"#,
        )
        .unwrap();
        let (_, permission_facet) = parse_event_payload_classified(
            r#"{"type":"permission.asked","properties":{"sessionID":"ses_1","permission":"bash"}}"#,
        )
        .unwrap();
        let (_, tool_facet) = parse_event_payload_classified(
            r#"{"type":"message.part.updated","properties":{"sessionID":"ses_1","type":"tool","name":"bash","status":"running"}}"#,
        )
        .unwrap();

        assert_eq!(status_facet, Some(OpencodeSnapshotFacet::Status));
        assert_eq!(message_facet, Some(OpencodeSnapshotFacet::Message));
        assert_eq!(permission_facet, None);
        assert_eq!(tool_facet, None);
    }

    #[test]
    fn ignores_malformed_opencode_events() {
        assert_eq!(parse_event_payload("not json"), None);
        assert_eq!(parse_event_payload(r#"{"type":"session.status"}"#), None);
    }

    #[test]
    fn opencode_event_schema_drift_does_not_read_unrelated_nested_status() {
        let event = parse_event_payload(
            r#"{"type":"session.status","properties":{"sessionID":"ses_1","metadata":{"status":"busy"}}}"#,
        )
        .unwrap();

        assert_eq!(event.session_id.as_deref(), Some("ses_1"));
        assert_eq!(event.state, None);
    }

    #[test]
    fn opencode_status_schema_drift_does_not_read_unrelated_nested_status() {
        assert_eq!(
            parse_session_state(
                r#"{"sessionID":"ses_1","metadata":{"status":"busy"}}"#,
                "ses_1",
            ),
            None
        );
    }

    #[test]
    fn opencode_state_maps_to_existing_agent_state() {
        assert_eq!(OpencodeState::Busy.agent_state(), AgentState::Running);
        assert_eq!(OpencodeState::Idle.agent_state(), AgentState::Idle);
        assert_eq!(OpencodeState::Done.agent_state(), AgentState::ExitedOk);
        assert_eq!(
            OpencodeState::NeedsInput.agent_state(),
            AgentState::NeedsInput
        );
        assert_eq!(
            OpencodeState::Offline.agent_state(),
            AgentState::NeedsRestart
        );
    }

    #[test]
    fn parse_localhost_url_rejects_remote_hosts() {
        assert!(parse_localhost_url("http://example.com:41000").is_err());
        assert_eq!(
            parse_localhost_url("http://127.0.0.1:41000").unwrap(),
            ("127.0.0.1".to_string(), 41_000)
        );
    }

    #[test]
    fn http_response_completion_uses_content_length_without_waiting_for_eof() {
        let complete =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]";
        let partial = &complete[..complete.len() - 1];

        assert!(!http_response_is_complete(partial));
        assert!(http_response_is_complete(complete));
        assert!(http_response_is_complete(
            b"HTTP/1.1 204 No Content\r\nConnection: keep-alive\r\n\r\n"
        ));
        assert!(!http_response_is_complete(b"HTTP/1.1 100 Continue\r\n\r\n"));
        let chunked = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n[]\r\n0\r\n\r\n";
        assert!(http_response_is_complete(chunked.as_bytes()));
        assert_eq!(parse_response(chunked).unwrap().body, "[]");
        let chunked_with_trailer = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n[]\r\n0\r\nChecksum: x\r\n\r\n";
        assert!(http_response_is_complete(chunked_with_trailer.as_bytes()));
        assert_eq!(parse_response(chunked_with_trailer).unwrap().body, "[]");
    }

    #[test]
    #[ignore = "requires PRISM_TEST_OPENCODE pointing to a real OpenCode binary"]
    fn real_opencode_server_round_trips_prism_session_api() {
        let opencode = std::env::var("PRISM_TEST_OPENCODE")
            .expect("set PRISM_TEST_OPENCODE to the real OpenCode binary");
        let temp = unique_temp_dir("prism-real-opencode-test");
        let worktree = temp.join("worktree");
        let second_worktree = temp.join("second-worktree");
        let home = temp.join("home");
        let config_dir = temp.join("opencode-config");
        let data_dir = temp.join("data");
        for path in [&worktree, &second_worktree, &home, &config_dir, &data_dir] {
            fs::create_dir_all(path).unwrap();
        }
        let worktree = fs::canonicalize(worktree).unwrap();
        let second_worktree = fs::canonicalize(second_worktree).unwrap();
        let repo = Repository::with_config_dir_for_test(worktree.clone(), temp.join("config"));
        #[cfg(unix)]
        let wrapper = {
            let wrapper = temp.join("opencode-isolated");
            let real_home = std::env::var("HOME").unwrap_or_default();
            let mise_data_dir = std::env::var("MISE_DATA_DIR").unwrap_or_else(|_| {
                PathBuf::from(&real_home)
                    .join(".local/share/mise")
                    .display()
                    .to_string()
            });
            fs::write(
                &wrapper,
                format!(
                    "#!/bin/sh\nexport HOME={}\nexport MISE_DATA_DIR={}\nexport npm_config_cache={}\nexport OPENCODE_CONFIG_DIR={}\nexport OPENCODE_DISABLE_AUTOUPDATE=true\nexport OPENCODE_DISABLE_DEFAULT_PLUGINS=true\nexport OPENCODE_DISABLE_LSP_DOWNLOAD=true\nexport OPENCODE_DISABLE_MODELS_FETCH=true\nexport XDG_DATA_HOME={}\nexec {} \"$@\"\n",
                    shell_quote_for_test(&home.display().to_string()),
                    shell_quote_for_test(&mise_data_dir),
                    shell_quote_for_test(&format!("{real_home}/.npm")),
                    shell_quote_for_test(&config_dir.display().to_string()),
                    shell_quote_for_test(&data_dir.display().to_string()),
                    shell_quote_for_test(&opencode),
                ),
            )
            .unwrap();
            let mut permissions = fs::metadata(&wrapper).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&wrapper, permissions).unwrap();
            wrapper
        };
        #[cfg(windows)]
        let wrapper = {
            let wrapper = temp.join("opencode-isolated.cmd");
            fs::write(
                &wrapper,
                format!(
                    "@echo off\r\nset \"HOME={}\"\r\nset \"OPENCODE_CONFIG_DIR={}\"\r\nset \"OPENCODE_DISABLE_AUTOUPDATE=true\"\r\nset \"OPENCODE_DISABLE_DEFAULT_PLUGINS=true\"\r\nset \"OPENCODE_DISABLE_LSP_DOWNLOAD=true\"\r\nset \"OPENCODE_DISABLE_MODELS_FETCH=true\"\r\nset \"XDG_DATA_HOME={}\"\r\ncall \"{}\" %*\r\n",
                    home.display(),
                    config_dir.display(),
                    data_dir.display(),
                    opencode,
                ),
            )
            .unwrap();
            wrapper
        };
        let mut config = Config::load(&repo);
        config.opencode_port_base = 41_000;
        config.opencode_port_span = 1_000;
        config
            .tools
            .insert("opencode".to_string(), wrapper.display().to_string());

        let runtime = ensure_opencode_server(&repo, &config, "feature/smoke", &worktree).unwrap();
        let result = (|| -> Result<(), String> {
            if !check_health(&runtime.server_url) {
                return Err("OpenCode server did not remain healthy".to_string());
            }
            let second_runtime =
                ensure_opencode_server(&repo, &config, "feature/second", &second_worktree)?;
            if second_runtime.server_url != runtime.server_url
                || second_runtime.server_pid != runtime.server_pid
            {
                return Err("worktrees did not reuse one OpenCode server".to_string());
            }
            let second_session =
                create_session(&runtime.server_url, &second_worktree, "Second worktree")?;
            if get_session_for_worktree(&runtime.server_url, &second_session.id, &second_worktree)?
                .is_none()
            {
                return Err("shared server did not route the second worktree".to_string());
            }
            let created = create_session(&runtime.server_url, &worktree, "Prism smoke test")?;
            let listed = list_sessions(&runtime.server_url)?;
            if !listed.iter().any(|session| session.id == created.id) {
                return Err(format!(
                    "created OpenCode session {} was not listed",
                    created.id
                ));
            }
            let resolved = ensure_opencode_session(&repo, &config, "feature/smoke", &worktree)?;
            if resolved.opencode_session_id.as_deref() != Some(created.id.as_str()) {
                return Err(format!(
                    "Prism did not select created OpenCode session {} for {}",
                    created.id,
                    worktree.display()
                ));
            }
            let fetched = get_session(&runtime.server_url, &created.id)?
                .ok_or_else(|| format!("created OpenCode session {} was not found", created.id))?;
            if fetched.id != created.id {
                return Err(format!(
                    "fetched OpenCode session {} instead of {}",
                    fetched.id, created.id
                ));
            }
            let prompt = "Prism persisted prompt smoke test";
            submit_prompt(&runtime.server_url, &created.id, prompt)?;
            let mut persisted = false;
            for _ in 0..20 {
                let summary = fetch_message_summary(&runtime.server_url, &created.id, None)?;
                if summary.latest_user_message.as_deref() == Some(prompt) {
                    persisted = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            if !persisted {
                return Err("submitted OpenCode prompt was not persisted".to_string());
            }
            Ok(())
        })();
        let shutdown = shutdown_owned_server(&runtime);
        let _ = fs::remove_dir_all(temp);

        result.unwrap();
        shutdown.unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn worktree_cleanup_keeps_a_server_referenced_by_another_worktree() {
        let temp = unique_temp_dir("prism-shared-opencode-cleanup");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        let mut command = Command::new("sh");
        command
            .args(["-c", "while :; do sleep 1; done"])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = crate::process::SupervisedChild::spawn(&mut command, None, None).unwrap();
        let process_id = child.id();
        record_owned_server_process(child);
        let runtime = |branch: &str, worktree: &str| OpencodeRuntime {
            repo_root: repo.root.display().to_string(),
            harness_id: "opencode".to_string(),
            branch: branch.to_string(),
            worktree_path: worktree.to_string(),
            server_port: 41_000,
            server_url: "http://127.0.0.1:41000".to_string(),
            server_pid: Some(process_id),
            server_process_identity: stored_process_identity(process_id),
            opencode_session_id: None,
            generation: 0,
            updated_unix_ms: 0,
        };
        let first = runtime("feature/first", "/repo/first");
        let second = runtime("feature/second", "/repo/second");
        save_runtime(&repo, &first).unwrap();
        save_runtime(&repo, &second).unwrap();

        shutdown_worktree_session_runtime_processes_with_lock_held(
            &repo,
            std::slice::from_ref(&first),
        )
        .unwrap();
        assert!(owned_server_process(process_id));

        crate::persistence::session::delete_runtime(&observability::db_path(&repo), &first)
            .unwrap();
        shutdown_worktree_session_runtime_processes_with_lock_held(
            &repo,
            std::slice::from_ref(&second),
        )
        .unwrap();
        assert!(!owned_server_process(process_id));
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn owned_server_shutdown_kills_term_ignoring_descendant_and_reaps_leader() {
        let temp = unique_temp_dir("prism-owned-opencode-process");
        fs::create_dir_all(&temp).unwrap();
        let descendant_path = temp.join("descendant.pid");
        let script = r#"
            trap '' TERM
            (
                trap '' TERM
                while :; do sleep 1; done
            ) &
            descendant=$!
            printf '%s\n' "$descendant" > "$1"
            wait "$descendant"
        "#;
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(script)
            .arg("owned-opencode-fixture")
            .arg(&descendant_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = crate::process::SupervisedChild::spawn(&mut command, None, None).unwrap();
        let process_id = child.id();
        let recorded_process = crate::process::record_process(process_id).unwrap();
        record_owned_server_process(child);
        let runtime = OpencodeRuntime {
            repo_root: "/repo".to_string(),
            harness_id: "opencode".to_string(),
            branch: "feature/test".to_string(),
            worktree_path: "/repo/worktree".to_string(),
            server_port: 41_000,
            server_url: "http://127.0.0.1:41000".to_string(),
            server_pid: Some(process_id),
            server_process_identity: recorded_process
                .identity
                .map(crate::process::ProcessIdentity::stored_value),
            opencode_session_id: None,
            generation: 0,
            updated_unix_ms: 0,
        };
        let ready_deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !descendant_path.exists() {
            assert!(std::time::Instant::now() < ready_deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        let descendant_id = fs::read_to_string(&descendant_path)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        let recorded_descendant = crate::process::record_process(descendant_id).unwrap();

        let started = std::time::Instant::now();
        shutdown_owned_server(&runtime).unwrap();

        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(!owned_server_process(process_id));
        for process in [recorded_process, recorded_descendant] {
            let gone_deadline = std::time::Instant::now() + Duration::from_secs(2);
            loop {
                if crate::process::observe_process(process).unwrap()
                    == crate::process::ProcessObservation::Missing
                {
                    break;
                }
                assert!(
                    std::time::Instant::now() < gone_deadline,
                    "owned server process {} survived shutdown",
                    process.pid
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        fs::remove_dir_all(temp).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn stored_server_shutdown_uses_verified_bounded_process_group_recovery() {
        let temp = unique_temp_dir("prism-stored-opencode-process");
        fs::create_dir_all(&temp).unwrap();
        let descendant_path = temp.join("descendant.pid");
        let script = r#"
            trap '' TERM
            (
                trap '' TERM
                while :; do sleep 1; done
            ) &
            descendant=$!
            printf '%s\n' "$descendant" > "$1"
            wait "$descendant"
        "#;
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(script)
            .arg("stored-opencode-fixture")
            .arg(&descendant_path)
            .args(["serve", "--hostname", "127.0.0.1", "--port", "41000"])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = crate::process::SupervisedChild::spawn(&mut command, None, None).unwrap();
        let process_id = child.id();
        let recorded_process = crate::process::record_process(process_id).unwrap();
        let runtime = OpencodeRuntime {
            repo_root: "/repo".to_string(),
            harness_id: "opencode".to_string(),
            branch: "feature/test".to_string(),
            worktree_path: "/repo/worktree".to_string(),
            server_port: 41_000,
            server_url: "http://127.0.0.1:41000".to_string(),
            server_pid: Some(process_id),
            server_process_identity: recorded_process
                .identity
                .map(crate::process::ProcessIdentity::stored_value),
            opencode_session_id: None,
            generation: 0,
            updated_unix_ms: 0,
        };
        let ready_deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !descendant_path.exists() {
            assert!(std::time::Instant::now() < ready_deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        let descendant_id = fs::read_to_string(&descendant_path)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        let recorded_descendant = crate::process::record_process(descendant_id).unwrap();
        let reaper = std::thread::spawn(move || child.wait().unwrap());

        let started = std::time::Instant::now();
        shutdown_stored_server(&runtime).unwrap();

        assert!(started.elapsed() < Duration::from_secs(3));
        reaper.join().unwrap();
        let gone_deadline = std::time::Instant::now() + Duration::from_secs(2);
        while crate::process::observe_process(recorded_descendant).unwrap()
            != crate::process::ProcessObservation::Missing
        {
            assert!(
                std::time::Instant::now() < gone_deadline,
                "stored server descendant {descendant_id} survived shutdown"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        fs::remove_dir_all(temp).unwrap();
    }

    #[cfg(unix)]
    fn shell_quote_for_test(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }

    fn start_health_server(
        worktree: PathBuf,
    ) -> (String, Arc<AtomicBool>, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = stop.clone();
        let server = std::thread::spawn(move || {
            while !server_stop.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("accept health request: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                let mut request = [0_u8; 1024];
                let count = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..count]);
                let body = if request.starts_with("GET /global/health ") {
                    r#"{"healthy":true}"#.to_string()
                } else {
                    format!(
                        r#"{{"id":"ses_first","directory":"{}"}}"#,
                        worktree.display()
                    )
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        (url, stop, server)
    }

    fn start_session_resolution_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming().take(2) {
                let mut stream = stream.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut buffer = [0_u8; 256];
                    let count = stream.read(&mut buffer).unwrap();
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..count]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&request);
                let body = if request.starts_with("GET /session/old ") {
                    r#"{"id":"old","directory":"/repo/wt","timeUpdated":"2026-01-01T00:00:00Z"}"#
                } else if request.starts_with("GET /session ")
                    || request.starts_with("GET /session?")
                {
                    r#"[
                        {"id":"old","directory":"/repo/wt","timeUpdated":"2026-01-01T00:00:00Z"},
                        {"id":"new","directory":"/repo/wt","timeUpdated":"2026-01-02T00:00:00Z"}
                    ]"#
                } else {
                    r#"{}"#
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        url
    }
}
