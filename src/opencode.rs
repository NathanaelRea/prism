#![allow(dead_code)]

use std::collections::BTreeMap;
#[cfg(target_os = "linux")]
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{OptionalExtension, params};

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
const SERVER_START_TIMEOUT: Duration = Duration::from_secs(5);
const SERVER_START_POLL: Duration = Duration::from_millis(100);

static OWNED_SERVER_PROCESSES: OnceLock<Mutex<BTreeMap<u32, OwnedServerProcess>>> = OnceLock::new();

#[derive(Clone, Copy, Debug)]
struct OwnedServerProcess {
    start_time_ticks: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpencodeRuntime {
    pub repo_root: String,
    pub branch: String,
    pub worktree_path: String,
    pub server_port: u16,
    pub server_url: String,
    pub server_pid: Option<u32>,
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
    ensure_opencode_server_with_program(repo, config, branch, worktree, &config.tool("opencode"))
}

pub fn ensure_opencode_server_with_program(
    repo: &Repository,
    config: &Config,
    branch: &str,
    worktree: &Path,
    program: &str,
) -> Result<OpencodeRuntime, String> {
    let existing = load_runtime(repo, branch, worktree)?;
    if let Some(runtime) = existing.as_ref()
        && check_health(&runtime.server_url)
    {
        return Ok(runtime.clone());
    }

    let port = allocate_port(
        &repo.root.display().to_string(),
        &worktree.display().to_string(),
        existing.as_ref().map(|runtime| runtime.server_port),
        config.opencode_port_base,
        config.opencode_port_span,
        port_status,
    )?;
    let server_url = server_url(port);
    let mut started_server = None;
    let server_pid = if check_health(&server_url) {
        existing.as_ref().and_then(|runtime| runtime.server_pid)
    } else {
        let mut child = Command::new(program)
            .arg("serve")
            .args(["--hostname", "127.0.0.1"])
            .args(["--port", &port.to_string()])
            .current_dir(worktree)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("start opencode server: {error}"))?;
        record_owned_server_process(child.id());
        if let Err(error) = wait_for_health(&server_url) {
            let _ = child.kill();
            let _ = child.wait();
            forget_owned_server_process(child.id());
            return Err(error);
        }
        let pid = child.id();
        started_server = Some(child);
        Some(pid)
    };

    let runtime = OpencodeRuntime {
        repo_root: repo.root.display().to_string(),
        branch: branch.to_string(),
        worktree_path: worktree.display().to_string(),
        server_port: port,
        server_url,
        server_pid,
        opencode_session_id: existing.and_then(|runtime| runtime.opencode_session_id),
        generation: 0,
        updated_unix_ms: unix_ms(),
    };
    if let Err(error) = save_runtime(repo, &runtime) {
        if let Some(mut child) = started_server {
            let _ = child.kill();
            let _ = child.wait();
            forget_owned_server_process(child.id());
        }
        return Err(error);
    }
    Ok(runtime)
}

pub fn ensure_opencode_session(
    repo: &Repository,
    config: &Config,
    branch: &str,
    worktree: &Path,
) -> Result<OpencodeRuntime, String> {
    let mut runtime = ensure_opencode_server(repo, config, branch, worktree)?;
    let session = resolve_session(&runtime, worktree)?;
    save_runtime_session(repo, &mut runtime, session.id)?;
    Ok(runtime)
}

pub fn refresh_opencode_session(
    repo: &Repository,
    mut runtime: OpencodeRuntime,
    worktree: &Path,
) -> Result<OpencodeRuntime, String> {
    let Some(session) = newest_listed_session_for_worktree(&runtime, worktree).unwrap_or(None)
    else {
        return Ok(runtime);
    };
    save_runtime_session(repo, &mut runtime, session.id)?;
    Ok(runtime)
}

pub fn list_sessions(server_url: &str) -> Result<Vec<OpencodeSession>, String> {
    let response = get(server_url, "/session", API_TIMEOUT)?;
    if response.status_code != 200 {
        return Err(format!(
            "list opencode sessions failed with HTTP {}",
            response.status_code
        ));
    }
    Ok(parse_sessions(&response.body))
}

pub fn get_session(server_url: &str, session_id: &str) -> Result<Option<OpencodeSession>, String> {
    let response = get(
        server_url,
        &format!("/session/{}", url_path_segment(session_id)),
        API_TIMEOUT,
    )?;
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
    match post(server_url, &path, &body, API_TIMEOUT) {
        Ok(response) if response.status_code == 200 || response.status_code == 201 => {
            parse_session(&response.body).ok_or_else(|| "created opencode session had no id".into())
        }
        Ok(response) if response.status_code == 400 || response.status_code == 415 => {
            let mut fallback = post(server_url, &path, "{}", API_TIMEOUT)?;
            if fallback.status_code == 400 || fallback.status_code == 415 {
                fallback = post(server_url, "/session", "{}", API_TIMEOUT)?;
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
    let body = prompt_async_body(prompt);
    let response = post(
        server_url,
        &format!("/session/{}/prompt_async", url_path_segment(session_id)),
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
    let response = post(
        server_url,
        &format!("/session/{}/abort", url_path_segment(session_id)),
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
    let Some(owned) = owned_server_process(pid) else {
        return Ok(());
    };
    if !process_matches_owned_start(pid, owned) {
        forget_owned_server_process(pid);
        return Ok(());
    }
    #[cfg(unix)]
    {
        let result = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        if result == 0 {
            forget_owned_server_process(pid);
            Ok(())
        } else {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                forget_owned_server_process(pid);
                Ok(())
            } else {
                Err(format!("stop opencode server {pid}: {error}"))
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        Ok(())
    }
}

pub(crate) fn shutdown_stored_server(runtime: &OpencodeRuntime) -> Result<(), String> {
    if runtime.server_pid.and_then(owned_server_process).is_some() {
        return shutdown_owned_server(runtime);
    }
    let Some(pid) = runtime.server_pid else {
        return Ok(());
    };
    if !stored_server_process_matches(pid, runtime.server_port) {
        return Ok(());
    }
    #[cfg(unix)]
    {
        let result = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        if result == 0 {
            Ok(())
        } else {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                Ok(())
            } else {
                Err(format!("stop opencode server {pid}: {error}"))
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        Ok(())
    }
}

fn stored_server_process_matches(pid: u32, port: u16) -> bool {
    #[cfg(target_os = "linux")]
    {
        let cmdline = fs::read_to_string(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        let args: Vec<&str> = cmdline.split('\0').filter(|arg| !arg.is_empty()).collect();
        stored_server_args_match(&args, port)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (pid, port);
        false
    }
}

fn stored_server_args_match(args: &[&str], port: u16) -> bool {
    let port = port.to_string();
    args.windows(2)
        .any(|window| window[0].ends_with("opencode") && window[1] == "serve")
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

fn record_owned_server_process(pid: u32) {
    if let Ok(mut processes) = owned_server_processes().lock() {
        processes.insert(
            pid,
            OwnedServerProcess {
                start_time_ticks: process_start_time_ticks(pid),
            },
        );
    }
}

fn owned_server_process(pid: u32) -> Option<OwnedServerProcess> {
    owned_server_processes().lock().ok()?.get(&pid).copied()
}

fn forget_owned_server_process(pid: u32) {
    if let Ok(mut processes) = owned_server_processes().lock() {
        processes.remove(&pid);
    }
}

fn process_matches_owned_start(pid: u32, owned: OwnedServerProcess) -> bool {
    match (owned.start_time_ticks, process_start_time_ticks(pid)) {
        (Some(expected), Some(actual)) => expected == actual,
        (Some(_), None) => false,
        (None, _) => true,
    }
}

fn process_start_time_ticks(pid: u32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let fields_after_comm = stat.rsplit_once(") ")?.1;
        fields_after_comm.split_whitespace().nth(19)?.parse().ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
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
    if !check_health(&runtime.server_url) {
        return Ok(OpencodeStatus::offline(
            Some(runtime.server_url.clone()),
            Some(session_id.to_string()),
        ));
    }

    let session = get_session(&runtime.server_url, session_id)?.unwrap_or(OpencodeSession {
        id: session_id.to_string(),
        directory: None,
        title: None,
        time_updated: None,
        parent_id: None,
    });
    let mut state =
        fetch_session_state(&runtime.server_url, session_id).unwrap_or(OpencodeState::Idle);
    if fetch_pending_permission(&runtime.server_url, session_id).unwrap_or(false) {
        state = OpencodeState::NeedsInput;
    }
    let mut messages = fetch_message_summary(&runtime.server_url, session_id).unwrap_or_default();
    if state == OpencodeState::Idle
        && let Some(message_state) = messages.latest_turn_state
    {
        state = message_state;
    }
    if state == OpencodeState::NeedsInput {
        messages.active_tool = None;
    }
    let todos = fetch_todos(&runtime.server_url, session_id).unwrap_or_default();

    Ok(OpencodeStatus {
        server_url: Some(runtime.server_url.clone()),
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
    if !check_health(server_url) {
        return Ok(OpencodeStatus::offline(
            Some(server_url.to_string()),
            Some(session_id.to_string()),
        ));
    }

    let session = get_session(server_url, session_id)?.unwrap_or(OpencodeSession {
        id: session_id.to_string(),
        directory: None,
        title: None,
        time_updated: None,
        parent_id: None,
    });
    let mut state = fetch_session_state(server_url, session_id).unwrap_or(OpencodeState::Idle);
    if fetch_pending_permission(server_url, session_id).unwrap_or(false) {
        state = OpencodeState::NeedsInput;
    }
    let mut messages = fetch_message_summary(server_url, session_id).unwrap_or_default();
    if state == OpencodeState::Idle
        && let Some(message_state) = messages.latest_turn_state
    {
        state = message_state;
    }
    if state == OpencodeState::NeedsInput {
        messages.active_tool = None;
    }
    let todos = fetch_todos(server_url, session_id).unwrap_or_default();

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

pub fn listen_event_payloads(
    server_url: &str,
    mut on_payload: impl FnMut(String) -> Result<(), String>,
) -> Result<(), String> {
    let (host, port) = parse_localhost_url(server_url)?;
    let mut stream = TcpStream::connect_timeout(
        &(host.as_str(), port)
            .to_socket_addrs()
            .map_err(|error| format!("resolve {server_url}: {error}"))?
            .next()
            .ok_or_else(|| format!("resolve {server_url}: no address"))?,
        SSE_CONNECT_TIMEOUT,
    )
    .map_err(|error| format!("connect {server_url}: {error}"))?;
    stream
        .set_read_timeout(Some(SSE_READ_TIMEOUT))
        .map_err(|error| format!("configure SSE read timeout: {error}"))?;
    stream
        .set_write_timeout(Some(SSE_CONNECT_TIMEOUT))
        .map_err(|error| format!("configure SSE write timeout: {error}"))?;
    write!(
        stream,
        "GET /event HTTP/1.1\r\nHost: {host}:{port}\r\nAccept: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"
    )
    .map_err(|error| format!("write SSE request: {error}"))?;

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .map_err(|error| format!("read SSE status: {error}"))?;
    let status_code = status_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| format!("invalid SSE status line: {}", status_line.trim_end()))?
        .parse::<u16>()
        .map_err(|error| format!("parse SSE status: {error}"))?;
    if !success_status(status_code) {
        return Err(format!(
            "open opencode event stream failed with HTTP {status_code}"
        ));
    }

    let mut line = String::new();
    let mut chunked = false;
    loop {
        line.clear();
        let count = reader
            .read_line(&mut line)
            .map_err(|error| format!("read SSE headers: {error}"))?;
        if count == 0 {
            return Err("opencode event stream closed before body".to_string());
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        let header = line.trim_end().to_ascii_lowercase();
        if header.starts_with("transfer-encoding:") && header.contains("chunked") {
            chunked = true;
        }
    }

    if chunked {
        read_sse_payloads(
            BufReader::new(ChunkedBodyReader::new(reader)),
            &mut on_payload,
        )
    } else {
        read_sse_payloads(reader, &mut on_payload)
    }
}

fn read_sse_payloads(
    mut reader: impl BufRead,
    on_payload: &mut impl FnMut(String) -> Result<(), String>,
) -> Result<(), String> {
    let mut line = String::new();
    let mut data = String::new();
    loop {
        line.clear();
        let count = reader
            .read_line(&mut line)
            .map_err(|error| format!("read opencode event stream: {error}"))?;
        if count == 0 {
            return Err("opencode event stream closed".to_string());
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            if !data.trim().is_empty() {
                on_payload(data.trim().to_string())?;
                data.clear();
            }
            continue;
        }
        if let Some(value) = trimmed.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.trim_start());
        }
    }
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
    .then_some(event)
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

fn fetch_session_state(server_url: &str, session_id: &str) -> Result<OpencodeState, String> {
    let response = get(server_url, "/session/status", API_TIMEOUT)?;
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

fn fetch_pending_permission(server_url: &str, session_id: &str) -> Result<bool, String> {
    let response = get(server_url, "/permission", API_TIMEOUT)?;
    if !success_status(response.status_code) {
        return Err(http_error_message(
            "read opencode permissions",
            response.status_code,
            &response.body,
        ));
    }
    Ok(has_pending_permission(&response.body, session_id))
}

fn fetch_message_summary(server_url: &str, session_id: &str) -> Result<MessageSummary, String> {
    let response = get(
        server_url,
        &format!("/session/{}/message?limit=10", url_path_segment(session_id)),
        API_TIMEOUT,
    )?;
    if !success_status(response.status_code) {
        return Err(http_error_message(
            "read opencode messages",
            response.status_code,
            &response.body,
        ));
    }
    Ok(parse_message_summary(&response.body))
}

fn fetch_todos(server_url: &str, session_id: &str) -> Result<Vec<OpencodeTodo>, String> {
    let response = get(
        server_url,
        &format!("/session/{}/todo", url_path_segment(session_id)),
        API_TIMEOUT,
    )?;
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
        && let Some(session) = get_session(&runtime.server_url, session_id)?
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
    let response = get(server_url, &path, API_TIMEOUT)?;
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
    branch: &str,
    worktree: &Path,
) -> Result<Option<OpencodeRuntime>, String> {
    let repo_root = repo.root.display().to_string();
    let worktree_path = worktree.display().to_string();
    observability::with_writable_db(repo, |conn| {
        conn.query_row(
            "select repo_root, branch, worktree_path, server_port, server_url, server_pid,
                    opencode_session_id, generation, updated_unix_ms
             from opencode_runtime
             where repo_root = ?1 and branch = ?2 and worktree_path = ?3",
            params![repo_root, branch, worktree_path],
            |row| {
                let server_port = row.get::<_, i64>(3)?;
                let server_pid = row
                    .get::<_, Option<i64>>(5)?
                    .and_then(|pid| u32::try_from(pid).ok());
                Ok(OpencodeRuntime {
                    repo_root: row.get(0)?,
                    branch: row.get(1)?,
                    worktree_path: row.get(2)?,
                    server_port: u16::try_from(server_port).unwrap_or_default(),
                    server_url: row.get(4)?,
                    server_pid,
                    opencode_session_id: row.get(6)?,
                    generation: row
                        .get::<_, i64>(7)
                        .ok()
                        .and_then(|value| u64::try_from(value).ok())
                        .unwrap_or_default(),
                    updated_unix_ms: row
                        .get::<_, i64>(8)
                        .ok()
                        .and_then(|value| u64::try_from(value).ok())
                        .unwrap_or_default(),
                })
            },
        )
        .optional()
        .map_err(|error| format!("read opencode runtime: {error}"))
    })
}

pub(crate) fn load_runtimes_for_worktree_session(
    repo: &Repository,
    branch: &str,
    worktree: &Path,
) -> Result<Vec<OpencodeRuntime>, String> {
    let repo_root = repo.root.display().to_string();
    let worktree_path = worktree.display().to_string();
    observability::with_writable_db(repo, |conn| {
        let mut statement = conn
            .prepare(
                "select repo_root, branch, worktree_path, server_port, server_url, server_pid,
                        opencode_session_id, generation, updated_unix_ms
                   from opencode_runtime
                  where repo_root = ?1 and branch = ?2 and worktree_path = ?3",
            )
            .map_err(|error| format!("prepare opencode runtime lookup: {error}"))?;
        let rows = statement
            .query_map(params![repo_root, branch, worktree_path], |row| {
                let server_pid = row
                    .get::<_, Option<i64>>(5)?
                    .and_then(|pid| u32::try_from(pid).ok());
                Ok(OpencodeRuntime {
                    repo_root: row.get(0)?,
                    branch: row.get(1)?,
                    worktree_path: row.get(2)?,
                    server_port: u16::try_from(row.get::<_, i64>(3)?).unwrap_or_default(),
                    server_url: row.get(4)?,
                    server_pid,
                    opencode_session_id: row.get(6)?,
                    generation: row
                        .get::<_, i64>(7)
                        .ok()
                        .and_then(|value| u64::try_from(value).ok())
                        .unwrap_or_default(),
                    updated_unix_ms: row
                        .get::<_, i64>(8)
                        .ok()
                        .and_then(|value| u64::try_from(value).ok())
                        .unwrap_or_default(),
                })
            })
            .map_err(|error| format!("read opencode runtime: {error}"))?;

        let mut runtimes = Vec::new();
        for row in rows {
            runtimes.push(row.map_err(|error| format!("read opencode runtime: {error}"))?);
        }
        Ok(runtimes)
    })
}

pub fn save_runtime(repo: &Repository, runtime: &OpencodeRuntime) -> Result<(), String> {
    observability::with_writable_db(repo, |conn| {
        conn.execute(
            "insert into opencode_runtime (
                repo_root, branch, worktree_path, server_port, server_url, server_pid,
                opencode_session_id, generation, updated_unix_ms
             ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             on conflict(repo_root, branch, worktree_path) do update set
                server_port = excluded.server_port,
                server_url = excluded.server_url,
                server_pid = excluded.server_pid,
                opencode_session_id = excluded.opencode_session_id,
                generation = excluded.generation,
                updated_unix_ms = excluded.updated_unix_ms",
            params![
                runtime.repo_root.as_str(),
                runtime.branch.as_str(),
                runtime.worktree_path.as_str(),
                i64::from(runtime.server_port),
                runtime.server_url.as_str(),
                runtime.server_pid.map(i64::from),
                runtime.opencode_session_id.as_deref(),
                i64::try_from(runtime.generation).unwrap_or(i64::MAX),
                i64::try_from(runtime.updated_unix_ms).unwrap_or(i64::MAX),
            ],
        )
        .map_err(|error| format!("write opencode runtime: {error}"))?;
        Ok(())
    })
}

pub(crate) fn migrate_runtime_schema(conn: &rusqlite::Connection) -> Result<(), String> {
    conn.execute_batch(
        "
        create table if not exists opencode_runtime (
          repo_root text not null,
          branch text not null,
          worktree_path text not null,
          server_port integer not null,
          server_url text not null,
          server_pid integer,
          opencode_session_id text,
          generation integer not null,
          updated_unix_ms integer not null,
          primary key (repo_root, branch, worktree_path)
        );

        create index if not exists opencode_runtime_branch_idx
          on opencode_runtime(repo_root, branch);
        ",
    )
    .map_err(|error| format!("create opencode runtime schema: {error}"))?;
    Ok(())
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
    let runtimes = load_runtimes_for_worktree_session(repo, branch, worktree)?;
    let mut errors = Vec::new();
    for runtime in runtimes {
        if runtime.branch != branch || runtime.worktree_path != worktree.display().to_string() {
            continue;
        }
        if let Err(error) = shutdown_stored_server(&runtime) {
            errors.push(error);
            continue;
        }
        let result = observability::with_writable_db(repo, |conn| {
            conn.execute(
                "delete from opencode_runtime
                 where repo_root = ?1 and branch = ?2 and worktree_path = ?3 and generation = ?4",
                params![
                    runtime.repo_root,
                    runtime.branch,
                    runtime.worktree_path,
                    i64::try_from(runtime.generation).unwrap_or(i64::MAX),
                ],
            )
            .map_err(|error| format!("remove shut down OpenCode runtime: {error}"))?;
            Ok(())
        });
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

pub(crate) fn remove_worktree_session_runtimes_with_conn(
    conn: &rusqlite::Connection,
    runtimes: &[OpencodeRuntime],
) -> Result<(), String> {
    for runtime in runtimes {
        conn.execute(
            "delete from opencode_runtime
              where repo_root = ?1 and branch = ?2 and worktree_path = ?3 and generation = ?4",
            params![
                runtime.repo_root,
                runtime.branch,
                runtime.worktree_path,
                i64::try_from(runtime.generation).unwrap_or(i64::MAX),
            ],
        )
        .map_err(|error| format!("remove opencode runtime state: {error}"))?;
    }
    Ok(())
}

pub(crate) fn shutdown_worktree_session_runtime_processes(
    runtimes: &[OpencodeRuntime],
) -> Result<(), String> {
    let errors = runtimes
        .iter()
        .filter_map(|runtime| shutdown_stored_server(runtime).err())
        .collect::<Vec<_>>();
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
    get(server_url, "/global/health", HEALTH_TIMEOUT)
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

fn get(server_url: &str, path: &str, timeout: Duration) -> Result<HttpResponse, String> {
    request(server_url, "GET", path, None, timeout)
}

fn post(
    server_url: &str,
    path: &str,
    body: &str,
    timeout: Duration,
) -> Result<HttpResponse, String> {
    request(server_url, "POST", path, Some(body), timeout)
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
    server_url: &str,
    method: &str,
    path: &str,
    body: Option<&str>,
    timeout: Duration,
) -> Result<HttpResponse, String> {
    let (host, port) = parse_localhost_url(server_url)?;
    let mut stream = TcpStream::connect_timeout(
        &(host.as_str(), port)
            .to_socket_addrs()
            .map_err(|error| format!("resolve {server_url}: {error}"))?
            .next()
            .ok_or_else(|| format!("resolve {server_url}: no address"))?,
        timeout,
    )
    .map_err(|error| format!("connect {server_url}: {error}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|error| format!("configure read timeout: {error}"))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|error| format!("configure write timeout: {error}"))?;
    match body {
        Some(body) => write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ),
        None => write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n"
        ),
    }
    .map_err(|error| format!("write HTTP request: {error}"))?;
    let mut response = Vec::new();
    loop {
        let mut buffer = [0_u8; 8192];
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                response.extend_from_slice(&buffer[..count]);
                if http_response_is_complete(&response) {
                    break;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(format!("read HTTP response: {error}")),
        }
    }
    let response = String::from_utf8_lossy(&response);
    parse_response(&response)
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

    use super::*;

    #[test]
    fn server_url_maps_port_to_local_http_url() {
        assert_eq!(server_url(41_234), "http://127.0.0.1:41234");
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
            branch: "feature".to_string(),
            worktree_path: worktree.display().to_string(),
            server_port: 41_222,
            server_url: server_url(41_222),
            server_pid: Some(123),
            opencode_session_id: Some("ses_123".to_string()),
            generation: 7,
            updated_unix_ms: 42,
        };

        save_runtime(&repo, &runtime).unwrap();
        let loaded = load_runtime(&repo, "feature", &worktree).unwrap().unwrap();

        assert_eq!(loaded, runtime);
        let _ = fs::remove_dir_all(temp);
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
            branch: "feature".to_string(),
            worktree_path: worktree.display().to_string(),
            server_port: 41_234,
            server_url,
            server_pid: None,
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
            branch: "feature".to_string(),
            worktree_path: worktree.display().to_string(),
            server_port: port,
            server_url: server_url(port),
            server_pid: None,
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
    #[cfg(unix)]
    #[ignore = "requires PRISM_TEST_OPENCODE pointing to a real OpenCode binary"]
    fn real_opencode_server_round_trips_prism_session_api() {
        let opencode = std::env::var("PRISM_TEST_OPENCODE")
            .expect("set PRISM_TEST_OPENCODE to the real OpenCode binary");
        let temp = unique_temp_dir("prism-real-opencode-test");
        let worktree = temp.join("worktree");
        let home = temp.join("home");
        let config_dir = temp.join("opencode-config");
        let data_dir = temp.join("data");
        for path in [&worktree, &home, &config_dir, &data_dir] {
            fs::create_dir_all(path).unwrap();
        }
        let worktree = fs::canonicalize(worktree).unwrap();
        let repo = Repository::with_config_dir_for_test(worktree.clone(), temp.join("config"));
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
                let summary = fetch_message_summary(&runtime.server_url, &created.id)?;
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
