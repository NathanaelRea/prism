#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{OptionalExtension, params};

use crate::agent::AgentState;
use crate::config::Config;
use crate::json::{
    json_array_field, json_escape, json_object_field, json_objects_in_array, json_string_field,
    json_top_level_objects,
};
use crate::observability;
use crate::repo::Repository;

const HEALTH_TIMEOUT: Duration = Duration::from_millis(250);
const API_TIMEOUT: Duration = Duration::from_secs(2);
const SSE_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const SSE_READ_TIMEOUT: Duration = Duration::from_secs(60);
const SERVER_START_TIMEOUT: Duration = Duration::from_secs(5);
const SERVER_START_POLL: Duration = Duration::from_millis(100);

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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpencodeState {
    Unknown,
    Starting,
    Idle,
    Busy,
    Retry,
    Error,
    Offline,
}

impl OpencodeState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Starting => "starting",
            Self::Idle => "idle",
            Self::Busy => "busy",
            Self::Retry => "retry",
            Self::Error => "error",
            Self::Offline => "offline",
        }
    }

    pub fn agent_state(self) -> AgentState {
        match self {
            Self::Unknown | Self::Starting => AgentState::NeedsRestart,
            Self::Idle => AgentState::NeedsInput,
            Self::Busy | Self::Retry => AgentState::Running,
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
    pub latest_message: Option<String>,
    pub active_tool: Option<String>,
    pub todos: Vec<OpencodeTodo>,
    pub last_updated_unix_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpencodeEvent {
    pub session_id: Option<String>,
    pub title: Option<String>,
    pub state: Option<OpencodeState>,
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
            latest_message: None,
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
    let server_pid = if check_health(&server_url) {
        existing.as_ref().and_then(|runtime| runtime.server_pid)
    } else {
        let child = Command::new(config.tool("opencode"))
            .arg("serve")
            .args(["--hostname", "127.0.0.1"])
            .args(["--port", &port.to_string()])
            .current_dir(worktree)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("start opencode server: {error}"))?;
        wait_for_health(&server_url)?;
        Some(child.id())
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
    save_runtime(repo, &runtime)?;
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
    if runtime.opencode_session_id.as_deref() != Some(session.id.as_str()) {
        runtime.opencode_session_id = Some(session.id);
        runtime.generation = runtime.generation.saturating_add(1);
        runtime.updated_unix_ms = unix_ms();
        save_runtime(repo, &runtime)?;
    }
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

pub fn create_session(server_url: &str, worktree: &Path) -> Result<OpencodeSession, String> {
    let directory = worktree.display().to_string();
    let body = format!(r#"{{"directory":"{}"}}"#, json_escape(&directory));
    match post(server_url, "/session", &body, API_TIMEOUT) {
        Ok(response) if response.status_code == 200 || response.status_code == 201 => {
            parse_session(&response.body).ok_or_else(|| "created opencode session had no id".into())
        }
        Ok(response) if response.status_code == 400 || response.status_code == 415 => {
            let fallback = post(server_url, "/session", "{}", API_TIMEOUT)?;
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
    append_prompt(server_url, session_id, prompt)?;
    submit_appended_prompt(server_url, session_id)
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

pub fn poll_status(runtime: &OpencodeRuntime) -> Result<OpencodeStatus, String> {
    let Some(session_id) = runtime.opencode_session_id.as_deref() else {
        return Ok(OpencodeStatus {
            server_url: Some(runtime.server_url.clone()),
            session_id: None,
            title: None,
            state: OpencodeState::Starting,
            latest_message: None,
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
    });
    let state = fetch_session_state(&runtime.server_url, session_id).unwrap_or(OpencodeState::Idle);
    let messages = fetch_message_summary(&runtime.server_url, session_id).unwrap_or_default();
    let todos = fetch_todos(&runtime.server_url, session_id).unwrap_or_default();

    Ok(OpencodeStatus {
        server_url: Some(runtime.server_url.clone()),
        session_id: Some(session_id.to_string()),
        title: session.title,
        state,
        latest_message: messages.latest_message,
        active_tool: messages.active_tool,
        todos,
        last_updated_unix_ms: Some(unix_ms()),
    })
}

pub fn listen_events(
    server_url: &str,
    mut on_event: impl FnMut(OpencodeEvent) -> Result<(), String>,
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
        read_sse_events(
            BufReader::new(ChunkedBodyReader::new(reader)),
            &mut on_event,
        )
    } else {
        read_sse_events(reader, &mut on_event)
    }
}

fn read_sse_events(
    mut reader: impl BufRead,
    on_event: &mut impl FnMut(OpencodeEvent) -> Result<(), String>,
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
                if let Some(event) = parse_event_payload(data.trim()) {
                    on_event(event)?;
                }
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
    let event_type = json_string_field(payload, "type")
        .or_else(|| json_string_field(payload, "event"))
        .unwrap_or_default();
    let object = json_object_field(payload, "properties")
        .or_else(|| json_object_field(payload, "data"))
        .or_else(|| json_object_field(payload, "session"))
        .unwrap_or(payload);
    let session_id = event_session_id(payload).or_else(|| event_session_id(object));
    let state = json_string_field(object, "status")
        .or_else(|| json_string_field(object, "state"))
        .or_else(|| json_string_field(payload, "status"))
        .or_else(|| json_string_field(payload, "state"))
        .and_then(|value| parse_state_label(&value))
        .or_else(|| event_type_state(&event_type));
    let todos = if event_type.contains("todo") || json_array_field(object, "todos").is_some() {
        Some(parse_todos(object))
    } else {
        None
    };
    let latest_message = if event_type.contains("message") || event_type.contains("part") {
        message_text(object).or_else(|| message_text(payload))
    } else {
        None
    };
    let active_tool = if event_type.contains("tool")
        || is_active_tool(object)
        || json_object_field(object, "tool").is_some()
    {
        tool_label(object)
            .or_else(|| json_object_field(object, "tool").and_then(tool_label))
            .or_else(|| tool_label(payload))
    } else {
        None
    };
    let title = json_string_field(object, "title").or_else(|| json_string_field(payload, "title"));

    let event = OpencodeEvent {
        session_id,
        title,
        state,
        latest_message,
        active_tool,
        todos,
    };
    (event.session_id.is_some()
        || event.title.is_some()
        || event.state.is_some()
        || event.latest_message.is_some()
        || event.active_tool.is_some()
        || event.todos.is_some())
    .then_some(event)
}

fn append_prompt(server_url: &str, session_id: &str, prompt: &str) -> Result<(), String> {
    let body = append_prompt_body(session_id, prompt);
    let response = post(server_url, "/tui/append-prompt", &body, API_TIMEOUT)?;
    if success_status(response.status_code) {
        Ok(())
    } else {
        Err(http_error_message(
            "append opencode prompt",
            response.status_code,
            &response.body,
        ))
    }
}

fn submit_appended_prompt(server_url: &str, session_id: &str) -> Result<(), String> {
    let body = submit_prompt_body(session_id);
    let response = post(server_url, "/tui/submit-prompt", &body, API_TIMEOUT)?;
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

fn append_prompt_body(session_id: &str, prompt: &str) -> String {
    format!(
        r#"{{"sessionID":"{}","text":"{}"}}"#,
        json_escape(session_id),
        json_escape(prompt)
    )
}

fn submit_prompt_body(session_id: &str) -> String {
    format!(r#"{{"sessionID":"{}"}}"#, json_escape(session_id))
}

#[derive(Default)]
struct MessageSummary {
    latest_message: Option<String>,
    active_tool: Option<String>,
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
    Ok(parse_session_state(&response.body, session_id).unwrap_or(OpencodeState::Unknown))
}

fn fetch_message_summary(server_url: &str, session_id: &str) -> Result<MessageSummary, String> {
    let response = get(
        server_url,
        &format!("/session/{}/message?limit=5", url_path_segment(session_id)),
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
    if let Some(session_id) = runtime.opencode_session_id.as_deref()
        && let Some(session) = get_session(&runtime.server_url, session_id)?
        && session_matches_worktree(&session, &worktree_path)
    {
        return Ok(session);
    }

    let sessions = list_sessions(&runtime.server_url)?;
    if let Some(session) = newest_session_for_worktree(&sessions, &worktree_path) {
        return Ok(session.clone());
    }

    create_session(&runtime.server_url, worktree)
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
        if matches!(status(port), PortStatus::Free | PortStatus::OpenCode) {
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
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|error| format!("read HTTP response: {error}"))?;
    parse_response(&response)
}

fn parse_localhost_url(url: &str) -> Result<(String, u16), String> {
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
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_string())
        .unwrap_or_default();
    Ok(HttpResponse { status_code, body })
}

fn parse_sessions(body: &str) -> Vec<OpencodeSession> {
    let objects = if body.trim_start().starts_with('[') {
        json_top_level_objects(body)
    } else {
        let mut objects = json_objects_in_array(body, "data");
        if objects.is_empty() {
            objects = json_objects_in_array(body, "sessions");
        }
        if objects.is_empty() {
            objects = json_objects_in_array(body, "items");
        }
        objects
    };
    objects
        .into_iter()
        .filter_map(parse_session_object)
        .collect()
}

fn parse_session(body: &str) -> Option<OpencodeSession> {
    if let Some(object) = json_object_field(body, "data") {
        parse_session_object(object)
    } else if let Some(object) = json_object_field(body, "session") {
        parse_session_object(object)
    } else {
        parse_session_object(body)
    }
}

fn parse_session_object(object: &str) -> Option<OpencodeSession> {
    let id = json_string_field(object, "id").or_else(|| json_string_field(object, "sessionID"))?;
    Some(OpencodeSession {
        id,
        directory: json_string_field(object, "directory")
            .or_else(|| json_string_field(object, "cwd"))
            .or_else(|| json_string_field(object, "path")),
        title: json_string_field(object, "title"),
        time_updated: json_string_field(object, "timeUpdated")
            .or_else(|| json_string_field(object, "updatedAt"))
            .or_else(|| json_string_field(object, "updated_at")),
    })
}

fn parse_session_state(body: &str, session_id: &str) -> Option<OpencodeState> {
    let objects = if body.trim_start().starts_with('[') {
        json_top_level_objects(body)
    } else {
        let mut objects = json_objects_in_array(body, "data");
        if objects.is_empty() {
            objects = json_objects_in_array(body, "sessions");
        }
        if objects.is_empty() {
            objects = json_objects_in_array(body, "items");
        }
        objects
    };
    if !objects.is_empty() {
        for object in objects {
            let object_session_id = json_string_field(object, "sessionID")
                .or_else(|| json_string_field(object, "sessionId"))
                .or_else(|| json_string_field(object, "session_id"))
                .or_else(|| json_string_field(object, "id"));
            if object_session_id
                .as_deref()
                .is_none_or(|id| id == session_id)
                && let Some(state) = parse_state_label(
                    &json_string_field(object, "status")
                        .or_else(|| json_string_field(object, "state"))?,
                )
            {
                return Some(state);
            }
        }
        return None;
    }

    for object in json_top_level_objects(body) {
        let object_session_id = json_string_field(object, "sessionID")
            .or_else(|| json_string_field(object, "sessionId"))
            .or_else(|| json_string_field(object, "session_id"))
            .or_else(|| json_string_field(object, "id"));
        if object_session_id
            .as_deref()
            .is_none_or(|id| id == session_id)
            && let Some(state) = parse_state_label(
                &json_string_field(object, "status")
                    .or_else(|| json_string_field(object, "state"))?,
            )
        {
            return Some(state);
        }
    }
    if let Some(object) = json_object_field(body, session_id) {
        return json_string_field(object, "status")
            .or_else(|| json_string_field(object, "state"))
            .and_then(|value| parse_state_label(&value));
    }
    json_string_field(body, session_id)
        .or_else(|| json_string_field(body, "status"))
        .or_else(|| json_string_field(body, "state"))
        .and_then(|value| parse_state_label(&value))
}

fn parse_state_label(value: &str) -> Option<OpencodeState> {
    match value.trim().to_ascii_lowercase().as_str() {
        "starting" | "loading" => Some(OpencodeState::Starting),
        "idle" | "ready" => Some(OpencodeState::Idle),
        "busy" | "running" | "working" => Some(OpencodeState::Busy),
        "retry" | "retrying" => Some(OpencodeState::Retry),
        "error" | "failed" => Some(OpencodeState::Error),
        "offline" | "disconnected" => Some(OpencodeState::Offline),
        _ => None,
    }
}

fn event_type_state(event_type: &str) -> Option<OpencodeState> {
    match event_type {
        "session.idle" => Some(OpencodeState::Idle),
        "session.error" => Some(OpencodeState::Error),
        _ => None,
    }
}

fn event_session_id(object: &str) -> Option<String> {
    json_string_field(object, "sessionID")
        .or_else(|| json_string_field(object, "sessionId"))
        .or_else(|| json_string_field(object, "session_id"))
        .or_else(|| json_string_field(object, "id"))
}

fn parse_message_summary(body: &str) -> MessageSummary {
    let objects = if body.trim_start().starts_with('[') {
        json_top_level_objects(body)
    } else {
        let mut objects = json_objects_in_array(body, "data");
        if objects.is_empty() {
            objects = json_objects_in_array(body, "messages");
        }
        if objects.is_empty() {
            objects = json_objects_in_array(body, "items");
        }
        objects
    };
    let mut summary = MessageSummary::default();
    for object in objects {
        if summary.latest_message.is_none()
            && is_assistant_like(object)
            && let Some(text) = message_text(object)
        {
            summary.latest_message = Some(text);
        }
        if summary.active_tool.is_none()
            && is_active_tool(object)
            && let Some(tool) = tool_label(object)
        {
            summary.active_tool = Some(tool);
        }
    }
    summary
}

fn is_assistant_like(object: &str) -> bool {
    json_string_field(object, "role").is_some_and(|role| role == "assistant")
        || json_string_field(object, "type").is_some_and(|event_type| event_type.contains("text"))
        || json_string_field(object, "partType").is_some_and(|part_type| part_type == "text")
}

fn message_text(object: &str) -> Option<String> {
    json_string_field(object, "text")
        .or_else(|| json_string_field(object, "content"))
        .or_else(|| json_string_field(object, "message"))
        .map(|text| text.replace('\n', " ").trim().to_string())
        .filter(|text| !text.is_empty())
}

fn is_active_tool(object: &str) -> bool {
    let type_is_tool = json_string_field(object, "type")
        .or_else(|| json_string_field(object, "partType"))
        .is_some_and(|event_type| event_type.contains("tool"));
    let status_is_active = json_string_field(object, "status")
        .or_else(|| json_string_field(object, "state"))
        .map(|status| {
            matches!(
                status.as_str(),
                "running" | "pending" | "in_progress" | "in-progress" | "busy"
            )
        })
        .unwrap_or(true);
    type_is_tool && status_is_active
}

fn tool_label(object: &str) -> Option<String> {
    let name = json_string_field(object, "tool")
        .or_else(|| json_string_field(object, "name"))
        .or_else(|| json_string_field(object, "title"))?;
    let status = json_string_field(object, "status").or_else(|| json_string_field(object, "state"));
    Some(match status {
        Some(status) if !status.is_empty() => format!("{name} {status}"),
        _ => name,
    })
}

fn parse_todos(body: &str) -> Vec<OpencodeTodo> {
    let objects = if body.trim_start().starts_with('[') {
        json_top_level_objects(body)
    } else {
        let mut objects = json_objects_in_array(body, "data");
        if objects.is_empty() {
            objects = json_objects_in_array(body, "todos");
        }
        if objects.is_empty() {
            objects = json_objects_in_array(body, "items");
        }
        if objects.is_empty()
            && let Some(array) = json_array_field(body, "todo")
        {
            objects = json_top_level_objects(array);
        }
        objects
    };
    objects
        .into_iter()
        .filter_map(|object| {
            let text = json_string_field(object, "content")
                .or_else(|| json_string_field(object, "text"))
                .or_else(|| json_string_field(object, "title"))?;
            Some(OpencodeTodo {
                text: text.replace('\n', " ").trim().to_string(),
                status: json_string_field(object, "status")
                    .or_else(|| json_string_field(object, "state"))
                    .unwrap_or_else(|| "pending".to_string()),
            })
        })
        .filter(|todo| !todo.text.is_empty())
        .collect()
}

fn newest_session_for_worktree<'a>(
    sessions: &'a [OpencodeSession],
    worktree_path: &str,
) -> Option<&'a OpencodeSession> {
    sessions
        .iter()
        .filter(|session| session_matches_worktree(session, worktree_path))
        .max_by(|left, right| left.time_updated.cmp(&right.time_updated))
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
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn server_url_maps_port_to_local_http_url() {
        assert_eq!(server_url(41_234), "http://127.0.0.1:41234");
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
            },
            OpencodeSession {
                id: "old".to_string(),
                directory: Some("/repo/wt".to_string()),
                title: None,
                time_updated: Some("2026-01-01T00:00:00Z".to_string()),
            },
            OpencodeSession {
                id: "new".to_string(),
                directory: Some("/repo/wt".to_string()),
                title: None,
                time_updated: Some("2026-01-02T00:00:00Z".to_string()),
            },
        ];

        let selected = newest_session_for_worktree(&sessions, "/repo/wt").unwrap();

        assert_eq!(selected.id, "new");
    }

    #[test]
    fn url_path_segment_percent_encodes_non_segment_bytes() {
        assert_eq!(url_path_segment("session/id 1"), "session%2Fid%201");
        assert_eq!(url_path_segment("ses_1-2.3~4"), "ses_1-2.3~4");
    }

    #[test]
    fn prompt_submission_bodies_include_session_and_escape_text() {
        assert_eq!(
            append_prompt_body("ses_123", "hello\n\"world\""),
            r#"{"sessionID":"ses_123","text":"hello\n\"world\""}"#
        );
        assert_eq!(submit_prompt_body("ses_123"), r#"{"sessionID":"ses_123"}"#);
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
    fn parses_opencode_status_sse_event() {
        let event = parse_event_payload(
            r#"{"type":"session.status","properties":{"sessionID":"ses_1","status":"busy","title":"Feature"}}"#,
        )
        .unwrap();

        assert_eq!(event.session_id.as_deref(), Some("ses_1"));
        assert_eq!(event.state, Some(OpencodeState::Busy));
        assert_eq!(event.title.as_deref(), Some("Feature"));
    }

    #[test]
    fn parses_opencode_message_tool_and_todo_events() {
        let message = parse_event_payload(
            r#"{"type":"message.part.updated","properties":{"sessionID":"ses_1","role":"assistant","text":"hello\nthere"}}"#,
        )
        .unwrap();
        assert_eq!(message.latest_message.as_deref(), Some("hello there"));

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
    fn opencode_state_maps_to_existing_agent_state() {
        assert_eq!(OpencodeState::Busy.agent_state(), AgentState::Running);
        assert_eq!(OpencodeState::Idle.agent_state(), AgentState::NeedsInput);
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

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }
}
