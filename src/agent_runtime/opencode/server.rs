use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use super::client::parse_localhost_url;
use super::registry::stored_runtime_session_matches;
use super::{OpencodeRuntime, PortStatus};
use crate::repo::Repository;

const HEALTH_TIMEOUT: Duration = Duration::from_millis(250);
const SERVER_START_TIMEOUT: Duration = Duration::from_secs(5);
const SERVER_START_POLL: Duration = Duration::from_millis(100);

static OWNED_SERVER_PROCESSES: OnceLock<Mutex<BTreeMap<u32, OwnedServerProcess>>> = OnceLock::new();
struct OwnedServerProcess {
    child: crate::process::SupervisedChild,
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

pub(super) fn shutdown_stored_server_with(
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

fn owned_server_processes() -> &'static Mutex<BTreeMap<u32, OwnedServerProcess>> {
    OWNED_SERVER_PROCESSES.get_or_init(|| Mutex::new(BTreeMap::new()))
}

pub(super) fn record_owned_server_process(child: crate::process::SupervisedChild) {
    let pid = child.id();
    let process = OwnedServerProcess { child };
    owned_server_processes()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(pid, process);
}

pub(super) fn owned_server_process(pid: u32) -> bool {
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

pub(super) fn stored_process_identity(pid: u32) -> Option<u64> {
    crate::process::record_process(pid)
        .ok()?
        .identity
        .map(crate::process::ProcessIdentity::stored_value)
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

pub(super) fn wait_for_health(server_url: &str) -> Result<(), String> {
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
