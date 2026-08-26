use std::path::Path;

use super::OpencodeRuntime;
use super::client::{get_session_for_worktree, unix_ms};
use crate::observability;
use crate::repo::Repository;

pub(super) fn stored_runtime_session_matches(runtime: &OpencodeRuntime) -> bool {
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

pub(super) fn runtime_for_worktree(
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

pub(super) fn server_reference_count(
    repo: &Repository,
    runtime: &OpencodeRuntime,
) -> Result<i64, String> {
    crate::persistence::session::count_server_references(
        &observability::db_path(repo),
        &runtime.repo_root,
        &runtime.server_url,
    )
    .map_err(|error| format!("count OpenCode server references: {error}"))
}
pub fn save_runtime_session(
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

pub fn load_runtimes_for_harness(
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

pub fn load_runtimes_for_worktree_session(
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

pub(super) fn save_shared_server_runtime(
    repo: &Repository,
    runtime: &OpencodeRuntime,
) -> Result<(), String> {
    crate::persistence::session::save_shared_server_runtime(&observability::db_path(repo), runtime)
        .map_err(|error| format!("write shared OpenCode runtime: {error}"))
}
