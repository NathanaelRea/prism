mod client;
mod model;
mod registry;
mod server;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::config::Config;
use crate::observability;
use crate::repo::Repository;

pub(crate) use model::OpencodeSnapshotFacet;
pub use model::{
    OpencodeEvent, OpencodeRuntime, OpencodeSession, OpencodeState, OpencodeStatus, OpencodeTodo,
    PortStatus,
};

#[allow(
    unused_imports,
    reason = "preserves the established crate::opencode client façade"
)]
pub use client::{
    abort_session, create_session, get_session, list_sessions, listen_event_payloads,
    listen_events, listen_events_until, parse_event_payload, poll_session_status, poll_status,
    poll_status_authoritative, submit_prompt,
};
#[allow(
    unused_imports,
    reason = "preserves internal worktree-scoped client APIs"
)]
pub(crate) use client::{
    list_sessions_for_directory, listen_classified_events_until, parse_localhost_url,
    submit_prompt_for_worktree, submit_prompt_for_worktree_with_selection,
};
pub(crate) use registry::load_runtimes_for_worktree_session;
#[allow(
    unused_imports,
    reason = "preserves the established runtime registry façade"
)]
pub use registry::{load_runtime, load_runtime_snapshot, save_runtime};
#[allow(
    unused_imports,
    reason = "preserves the established server lifecycle façade"
)]
pub use server::{allocate_port, check_health, port_status, server_url, shutdown_owned_server};
#[allow(unused_imports, reason = "preserves internal server lifecycle APIs")]
pub(crate) use server::{lock_repository_server, shutdown_stored_server};

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
            last_updated_unix_ms: Some(client::unix_ms()),
        }
    }
}

#[allow(dead_code, reason = "optional OpenCode server lifecycle API")]
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
    let _server_lock = server::lock_repository_server(repo)?;
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
    let existing = registry::load_runtime(repo, harness_id, branch, worktree)?;
    let runtimes = registry::load_runtimes_for_harness(repo, harness_id)?;
    if let Some(shared) = healthy_shared_runtime(&runtimes) {
        let runtime =
            registry::runtime_for_worktree(repo, harness_id, branch, worktree, &shared, &existing);
        registry::save_shared_server_runtime(repo, &runtime)?;
        return Ok(runtime);
    }

    let runtime_identity = format!("{}:{harness_id}", repo.root.display());
    let stored_port = runtimes
        .iter()
        .filter(|runtime| {
            runtime.server_pid.is_some() && server::stored_server_identity_is_valid(runtime)
        })
        .min_by_key(|runtime| (runtime.server_port, runtime.server_url.as_str()))
        .map(|runtime| runtime.server_port);
    let port = server::allocate_port(
        &runtime_identity,
        "",
        stored_port,
        config.opencode_port_base,
        config.opencode_port_span,
        server::port_status,
    )?;
    let server_url = server::server_url(port);
    let mut started_server = None;
    let server_pid = if server::check_health(&server_url) {
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
        if let Err(error) = server::wait_for_health(&server_url) {
            let _ = child.terminate();
            return Err(error);
        }
        let pid = child.id();
        started_server = Some(child);
        Some(pid)
    };

    let server_process_identity = if started_server.is_some() {
        server_pid.and_then(server::stored_process_identity)
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
        updated_unix_ms: client::unix_ms(),
    };
    if let Err(error) = registry::save_shared_server_runtime(repo, &runtime) {
        if let Some(mut child) = started_server {
            let _ = child.terminate();
        }
        return Err(error);
    }
    if let Some(child) = started_server {
        server::record_owned_server_process(child);
    }
    Ok(runtime)
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
            server::check_health(&runtime.server_url)
                && (server::stored_server_identity_is_valid(runtime)
                    || runtime.server_pid.is_none())
        })
        .cloned()
}

#[allow(dead_code, reason = "optional OpenCode session lifecycle API")]
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
    let _server_lock = server::lock_repository_server(repo)?;
    let mut runtime =
        ensure_opencode_server_locked(repo, config, harness_id, branch, worktree, program)?;
    let session = resolve_session(&runtime, worktree)?;
    registry::save_runtime_session(repo, &mut runtime, session.id)?;
    Ok(runtime)
}

pub fn refresh_opencode_session(
    repo: &Repository,
    mut runtime: OpencodeRuntime,
    worktree: &Path,
) -> Result<OpencodeRuntime, String> {
    let _server_lock = server::lock_repository_server(repo)?;
    let Some(current) = registry::load_runtime(
        repo,
        &runtime.harness_id,
        &runtime.branch,
        Path::new(&runtime.worktree_path),
    )?
    else {
        return Ok(runtime);
    };
    runtime = current;
    let Some(session) =
        client::newest_listed_session_for_worktree(&runtime, worktree).unwrap_or(None)
    else {
        return Ok(runtime);
    };
    registry::save_runtime_session(repo, &mut runtime, session.id)?;
    Ok(runtime)
}

fn resolve_session(runtime: &OpencodeRuntime, worktree: &Path) -> Result<OpencodeSession, String> {
    let worktree_path = worktree.display().to_string();
    let stored_session = if let Some(session_id) = runtime.opencode_session_id.as_deref()
        && let Some(session) =
            client::get_session_for_worktree(&runtime.server_url, session_id, worktree)?
        && client::session_matches_worktree(&session, &worktree_path)
    {
        Some(session)
    } else {
        None
    };

    match client::newest_listed_session_for_worktree(runtime, worktree) {
        Ok(Some(session)) => return Ok(session),
        Ok(None) => {}
        Err(_) if stored_session.is_some() => return Ok(stored_session.unwrap()),
        Err(error) => return Err(error),
    }

    if let Some(session) = stored_session {
        return Ok(session);
    }

    client::create_session(&runtime.server_url, worktree, &runtime.branch)
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
    let _server_lock = server::lock_repository_server(repo)?;
    let runtimes = registry::load_runtimes_for_worktree_session(repo, branch, worktree)?;
    let mut errors = Vec::new();
    for runtime in runtimes {
        if runtime.branch != branch || runtime.worktree_path != worktree.display().to_string() {
            continue;
        }
        let references = match registry::server_reference_count(repo, &runtime) {
            Ok(references) => references,
            Err(error) => {
                errors.push(error);
                continue;
            }
        };
        if references <= 1
            && let Err(error) = server::shutdown_stored_server(&runtime)
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
        match registry::server_reference_count(repo, runtime) {
            Ok(references) if references <= removed_references => {
                if let Err(error) = server::shutdown_stored_server(runtime) {
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
