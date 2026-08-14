use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::json::json_escape;
use serde_json::Value;

use super::{
    OpencodeEvent, OpencodeRuntime, OpencodeSession, OpencodeSnapshotFacet, OpencodeState,
    OpencodeStatus, OpencodeTodo,
};

const API_TIMEOUT: Duration = Duration::from_secs(5);
#[allow(dead_code, reason = "reserved for the optional streaming client path")]
const SSE_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const SSE_READ_TIMEOUT: Duration = Duration::from_secs(60);
const SSE_CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(200);

#[allow(dead_code, reason = "optional OpenCode session discovery API")]
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

#[allow(dead_code, reason = "optional worktree-scoped OpenCode discovery API")]
pub fn list_sessions_for_directory(
    server_url: &str,
    directory: &Path,
) -> Result<Vec<OpencodeSession>, String> {
    list_sessions_for_worktree(server_url, &directory.display().to_string())
}

pub fn get_session(server_url: &str, session_id: &str) -> Result<Option<OpencodeSession>, String> {
    get_session_in_directory(server_url, session_id, None)
}

pub fn get_session_for_worktree(
    server_url: &str,
    session_id: &str,
    worktree: &Path,
) -> Result<Option<OpencodeSession>, String> {
    get_session_in_directory(server_url, session_id, Some(worktree))
}

pub fn get_session_in_directory(
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

#[allow(dead_code, reason = "optional OpenCode prompt submission API")]
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
        crate::harness::AgentSelection::default(),
    )
}

#[allow(dead_code, reason = "optional worktree-scoped prompt submission API")]
pub fn submit_prompt_for_worktree(
    server_url: &str,
    session_id: &str,
    prompt: &str,
    worktree: &Path,
) -> Result<(), String> {
    submit_prompt_for_worktree_with_selection(
        server_url,
        session_id,
        prompt,
        worktree,
        crate::harness::AgentSelection::default(),
    )
}

pub fn submit_prompt_for_worktree_with_selection(
    server_url: &str,
    session_id: &str,
    prompt: &str,
    worktree: &Path,
    selection: crate::harness::AgentSelection<'_>,
) -> Result<(), String> {
    submit_prompt_in_directory(server_url, session_id, prompt, Some(worktree), selection)
}

fn submit_prompt_in_directory(
    server_url: &str,
    session_id: &str,
    prompt: &str,
    directory: Option<&Path>,
    selection: crate::harness::AgentSelection<'_>,
) -> Result<(), String> {
    let body = prompt_async_body(prompt, selection)?;
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

#[allow(dead_code, reason = "optional authoritative OpenCode status API")]
pub fn poll_status_authoritative(runtime: &OpencodeRuntime) -> Result<OpencodeStatus, String> {
    let Some(session_id) = runtime.opencode_session_id.as_deref() else {
        return poll_status(runtime);
    };
    let server_url = &runtime.server_url;
    if !check_health(server_url, API_TIMEOUT) {
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
    if !check_health(server_url, API_TIMEOUT) {
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

#[allow(dead_code, reason = "optional OpenCode streaming API")]
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

#[allow(dead_code, reason = "optional cancellable OpenCode streaming API")]
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

#[allow(dead_code, reason = "optional raw OpenCode event streaming API")]
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

#[allow(
    dead_code,
    reason = "shared implementation for optional streaming APIs"
)]
pub(super) fn listen_event_payloads_with_stop(
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

pub(super) fn listen_event_payloads_with_stop_at_path(
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
pub(super) struct SseMetrics {
    resolve_us: Option<u128>,
    connect_us: Option<u128>,
    write_us: Option<u128>,
    handshake_us: Option<u128>,
    stream_started: Option<Instant>,
    stream_lifetime_us: Option<u128>,
    status_code: Option<u16>,
    pub(super) payload_count: u64,
    payload_bytes: u64,
}

#[derive(Clone, Copy)]
pub(super) struct SseRequest<'a> {
    pub(super) path: &'a str,
    pub(super) connect_timeout: Duration,
    pub(super) read_poll_interval: Duration,
    pub(super) inactivity_timeout: Duration,
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
pub(super) enum SseFailureKind {
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

    pub(super) const fn error_kind(self) -> Option<&'static str> {
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

pub(super) struct SseFailure {
    pub(super) kind: SseFailureKind,
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

pub(super) fn listen_event_payloads_with_stop_inner(
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

#[allow(dead_code, reason = "optional OpenCode event decoder API")]
pub fn parse_event_payload(payload: &str) -> Option<OpencodeEvent> {
    parse_event_payload_classified(payload).map(|(event, _)| event)
}

pub(super) fn parse_event_payload_classified(
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

pub(super) fn prompt_async_body(
    prompt: &str,
    selection: crate::harness::AgentSelection<'_>,
) -> Result<String, String> {
    let mut fields = vec![format!(
        r#""parts":[{{"type":"text","text":"{}"}}]"#,
        json_escape(prompt)
    )];
    if let Some(model) = selection.model {
        let (provider_id, model_id) = model
            .split_once('/')
            .ok_or_else(|| format!("OpenCode model '{model}' must use provider/model format"))?;
        if provider_id.is_empty() || model_id.is_empty() {
            return Err(format!(
                "OpenCode model '{model}' must use provider/model format"
            ));
        }
        fields.push(format!(
            r#""model":{{"providerID":"{}","modelID":"{}"}}"#,
            json_escape(provider_id),
            json_escape(model_id)
        ));
    }
    if let Some(variant) = selection.variant {
        fields.push(format!(r#""variant":"{}""#, json_escape(variant)));
    }
    Ok(format!("{{{}}}", fields.join(",")))
}

#[derive(Default)]
pub(super) struct MessageSummary {
    pub(super) latest_message: Option<String>,
    pub(super) latest_user_message: Option<String>,
    pub(super) recent_messages: Vec<String>,
    pub(super) active_tool: Option<String>,
    pub(super) latest_turn_state: Option<OpencodeState>,
    pub(super) latest_error: Option<String>,
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

pub(super) fn session_state_from_status_body(body: &str, session_id: &str) -> OpencodeState {
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

pub(super) fn fetch_message_summary(
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

pub(super) fn newest_listed_session_for_worktree(
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
    let body = if body.chars().count() > 240 {
        format!("{}...", body.chars().take(240).collect::<String>())
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

pub(super) fn http_response_is_complete(response: &[u8]) -> bool {
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

pub(super) struct HttpResponse {
    status_code: u16,
    pub(super) body: String,
}

pub(super) fn parse_response(response: &str) -> Result<HttpResponse, String> {
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

pub(super) fn parse_sessions(body: &str) -> Vec<OpencodeSession> {
    let Some(value) = parse_json_value(body) else {
        return Vec::new();
    };
    collection_items(&value, &["data", "sessions", "items"])
        .into_iter()
        .filter_map(parse_session_object)
        .collect()
}

pub(super) fn parse_session(body: &str) -> Option<OpencodeSession> {
    let value = parse_json_value(body)?;
    let object = object_field(&value, &["data", "session"]).unwrap_or(&value);
    parse_session_object(object)
}

pub(super) fn parse_session_object(object: &Value) -> Option<OpencodeSession> {
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

pub(super) fn parse_session_state(body: &str, session_id: &str) -> Option<OpencodeState> {
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

pub(super) fn has_pending_permission(body: &str, session_id: &str) -> bool {
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

pub(super) fn parse_message_summary(body: &str) -> MessageSummary {
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

pub(super) fn parse_todos(body: &str) -> Vec<OpencodeTodo> {
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

pub(super) fn newest_session_for_worktree<'a>(
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

pub(super) fn session_matches_worktree(session: &OpencodeSession, worktree_path: &str) -> bool {
    session
        .directory
        .as_deref()
        .is_none_or(|directory| directory == worktree_path)
}

pub(super) fn request_path(path: &str, directory: Option<&Path>) -> String {
    let Some(directory) = directory else {
        return path.to_string();
    };
    let separator = if path.contains('?') { '&' } else { '?' };
    format!(
        "{path}{separator}directory={}",
        url_path_segment(&directory.display().to_string())
    )
}

pub(super) fn url_path_segment(value: &str) -> String {
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

pub(super) fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

pub(super) fn check_health(server_url: &str, timeout: Duration) -> bool {
    get("opencode.health", server_url, "/global/health", timeout)
        .map(|response| response.status_code == 200)
        .unwrap_or(false)
}
