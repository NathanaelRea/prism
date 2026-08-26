use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::OnceLock;
use std::time::Duration;

use super::client::parse_localhost_url;
use super::registry::stored_runtime_session_matches;
use super::{OpencodeRuntime, PortStatus};
use crate::repo::Repository;

const HEALTH_TIMEOUT: Duration = Duration::from_millis(250);
const SERVER_START_TIMEOUT: Duration = Duration::from_secs(5);
const SERVER_START_POLL: Duration = Duration::from_millis(100);

static OWNED_SERVER_PROCESSES: OnceLock<tokio::sync::Mutex<BTreeMap<u32, OwnedServerProcess>>> =
    OnceLock::new();

#[derive(Clone)]
struct OwnedServerProcess {
    pid: u32,
    identity: Option<u64>,
    control: crate::process::ProcessControl,
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

pub(super) fn stored_server_identity_is_valid(runtime: &OpencodeRuntime) -> bool {
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

pub(super) async fn owned_server_identity_is_valid(runtime: &OpencodeRuntime) -> bool {
    let Some(pid) = runtime.server_pid else {
        return false;
    };
    owned_server_processes()
        .lock()
        .await
        .get(&pid)
        .is_some_and(|owned| {
            owned.identity == runtime.server_process_identity && !owned.control.is_finished()
        })
}

pub async fn shutdown_owned_server(runtime: &OpencodeRuntime) -> Result<(), String> {
    let Some(pid) = runtime.server_pid else {
        return Ok(());
    };
    let owned = match take_matching_owned_server_process(pid, runtime.server_process_identity)
        .await?
    {
        Some(owned) => owned,
        None => {
            #[cfg(unix)]
            return shutdown_external_server_with(runtime, crate::process::process_arguments).await;
            #[cfg(windows)]
            return Ok(());
        }
    };
    owned
        .control
        .shutdown()
        .await
        .map_err(|error| format!("stop opencode server {pid}: {error}"))
}

pub(crate) async fn shutdown_stored_server(runtime: &OpencodeRuntime) -> Result<(), String> {
    shutdown_stored_server_with(runtime, crate::process::process_arguments).await
}

pub(super) async fn shutdown_stored_server_with(
    runtime: &OpencodeRuntime,
    inspect_arguments: impl FnOnce(
        u32,
    )
        -> Result<Option<Vec<String>>, crate::process::ProcessLifecycleError>,
) -> Result<(), String> {
    if let Some(pid) = runtime.server_pid
        && owned_server_process(pid).await
    {
        return shutdown_owned_server(runtime).await;
    }
    shutdown_external_server_with(runtime, inspect_arguments).await
}

async fn shutdown_external_server_with(
    runtime: &OpencodeRuntime,
    inspect_arguments: impl FnOnce(
        u32,
    )
        -> Result<Option<Vec<String>>, crate::process::ProcessLifecycleError>,
) -> Result<(), String> {
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
        .await
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

pub(super) fn stored_server_args_match(args: &[&str], port: u16) -> bool {
    let port = port.to_string();
    args.windows(2).any(|window| window[1] == "serve")
        && args
            .windows(2)
            .any(|window| window[0] == "--hostname" && window[1] == "127.0.0.1")
        && args
            .windows(2)
            .any(|window| window[0] == "--port" && window[1] == port)
}

fn owned_server_processes() -> &'static tokio::sync::Mutex<BTreeMap<u32, OwnedServerProcess>> {
    OWNED_SERVER_PROCESSES.get_or_init(|| tokio::sync::Mutex::new(BTreeMap::new()))
}

#[cfg(any(windows, test))]
pub(super) async fn record_owned_server_process(control: crate::process::ProcessControl) {
    let pid = control.pid();
    let process = OwnedServerProcess {
        pid,
        identity: control.identity(),
        control,
    };
    owned_server_processes().lock().await.insert(pid, process);
}

pub(super) async fn owned_server_process(pid: u32) -> bool {
    owned_server_processes().lock().await.contains_key(&pid)
}

async fn take_matching_owned_server_process(
    pid: u32,
    identity: Option<u64>,
) -> Result<Option<OwnedServerProcess>, String> {
    let mut processes = owned_server_processes().lock().await;
    let Some(owned) = processes.get(&pid) else {
        return Ok(None);
    };
    if owned.pid != pid || owned.identity != identity {
        return Err(format!(
            "refusing to stop owned opencode server {pid}: registry identity disagrees with persisted identity"
        ));
    }
    Ok(processes.remove(&pid))
}

pub fn allocate_port(
    repo_root: &str,
    worktree_path: &str,
    stored_port: Option<u16>,
    port_base: u16,
    port_span: u16,
    mut status: impl FnMut(u16) -> PortStatus,
) -> Result<u16, String> {
    if port_base == 0 || port_span == 0 {
        return Err("opencode port range must use a nonzero base and span".to_string());
    }
    let range_end = u32::from(port_base) + u32::from(port_span) - 1;
    if range_end > u32::from(u16::MAX) {
        return Err("opencode port range overflowed".to_string());
    }
    let in_range =
        |port: u16| u32::from(port) >= u32::from(port_base) && u32::from(port) <= range_end;
    if let Some(port) = stored_port
        && in_range(port)
        && matches!(status(port), PortStatus::Free | PortStatus::OpenCode)
    {
        return Ok(port);
    }

    let offset = stable_hash_text(&format!("{repo_root}{worktree_path}")) % u64::from(port_span);
    for step in 0..port_span {
        let candidate_offset = (offset + u64::from(step)) % u64::from(port_span);
        let port = port_base + u16::try_from(candidate_offset).unwrap_or_default();
        if matches!(status(port), PortStatus::Free) {
            return Ok(port);
        }
    }
    Err(format!(
        "no free opencode port found from {port_base} through {range_end}"
    ))
}

fn stable_hash_text(value: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

pub fn server_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

pub fn check_health(server_url: &str) -> bool {
    super::client::check_health(server_url, HEALTH_TIMEOUT)
}

pub(super) async fn check_health_async(server_url: &str) -> bool {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let Ok((host, port)) = parse_localhost_url(server_url) else {
        return false;
    };
    let request = async {
        let mut stream = tokio::net::TcpStream::connect((host.as_str(), port)).await?;
        stream
            .write_all(
                format!(
                    "GET /global/health HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await?;
        let mut response = [0_u8; 64];
        let count = stream.read(&mut response).await?;
        Ok::<bool, std::io::Error>(
            response[..count].starts_with(b"HTTP/1.1 200")
                || response[..count].starts_with(b"HTTP/1.0 200"),
        )
    };
    tokio::time::timeout(HEALTH_TIMEOUT, request)
        .await
        .is_ok_and(|result| result.unwrap_or(false))
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

pub(super) async fn wait_for_health(server_url: &str) -> Result<(), String> {
    let started = tokio::time::Instant::now();
    while started.elapsed() < SERVER_START_TIMEOUT {
        if check_health_async(server_url).await {
            return Ok(());
        }
        tokio::time::sleep(SERVER_START_POLL).await;
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
