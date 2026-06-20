use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::params;

use crate::agent::AgentState;
use crate::config::Config;
use crate::git::git_status_label;
use crate::github::{PrCache, load_pr_cache};
use crate::json::json_string_field;
use crate::observability::{self, LogLevel};
use crate::opencode::OpencodeStatus;
use crate::process::run_capture;
use crate::repo::Repository;
use crate::util::{safe_branch_filename, truncate};

#[derive(Debug)]
pub struct Session {
    pub repo_index: usize,
    pub repo_label: String,
    pub repo_key: Option<char>,
    pub path: PathBuf,
    pub path_display: String,
    pub branch: String,
    pub prompt_summary: String,
    pub adopted: bool,
    pub hidden: bool,
    pub status_label: String,
    pub agent_state: AgentState,
    pub opencode_status: Option<OpencodeStatus>,
    pub pr: PrCache,
    pub wt_columns: BTreeMap<String, String>,
    pub unseen_comments: bool,
}

pub fn discover_sessions(repo: &Repository, config: &Config) -> Result<Vec<Session>, String> {
    let output = run_capture(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(&repo.root)
            .args(["worktree", "list", "--porcelain"]),
    )?;
    let mut sessions = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_branch: Option<String> = None;

    for line in output.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            if let Some(path) = current_path.take() {
                let branch = current_branch
                    .take()
                    .unwrap_or_else(|| "(detached)".to_string());
                if path.exists() {
                    sessions.push(build_session(repo, path, branch, config));
                } else {
                    observability::emit(observability::EventInput {
                        level: LogLevel::Warn,
                        target: "session",
                        action: "skip_missing_worktree",
                        operation_id: None,
                        parent_operation_id: None,
                        branch: Some(branch),
                        session: Some(path.display().to_string()),
                        message: format!("skipping missing worktree {}", path.display()),
                        data_json: None,
                    });
                }
            }
            continue;
        }
        if let Some(path) = line.strip_prefix("worktree ") {
            current_path = Some(PathBuf::from(path));
        } else if let Some(branch) = line.strip_prefix("branch ") {
            current_branch = Some(
                branch
                    .strip_prefix("refs/heads/")
                    .unwrap_or(branch)
                    .to_string(),
            );
        } else if line.starts_with("detached") {
            current_branch = Some("(detached)".to_string());
        }
    }

    sessions.sort_by(|a, b| {
        let a_default = config.is_default_branch(&a.branch);
        let b_default = config.is_default_branch(&b.branch);
        b_default
            .cmp(&a_default)
            .then_with(|| a.branch.cmp(&b.branch))
            .then_with(|| a.path.cmp(&b.path))
    });
    Ok(sessions)
}

fn build_session(repo: &Repository, path: PathBuf, branch: String, config: &Config) -> Session {
    let legacy_metadata_path = path
        .join(".agent/tasks")
        .join(format!("{}.json", safe_branch_filename(&branch)));
    let metadata = load_task_metadata(repo, &branch);
    let prompt_summary = metadata
        .as_ref()
        .map(|metadata| metadata.prompt_summary.clone())
        .or_else(|| read_prompt_summary(&legacy_metadata_path))
        .unwrap_or_default();
    let adopted = metadata.is_some() || legacy_metadata_path.exists();
    let status_label = git_status_label(&path, config);
    let path_display = path.display().to_string();
    let agent_state = load_agent_state(repo, &branch).unwrap_or(AgentState::Idle);
    let pr = if config.is_default_branch(&branch) {
        let _ = crate::github::remove_pr_cache(repo, &branch);
        PrCache::default()
    } else {
        load_pr_cache(repo, &branch)
    };
    Session {
        repo_index: 0,
        repo_label: String::new(),
        repo_key: None,
        path,
        path_display,
        branch,
        prompt_summary,
        adopted,
        hidden: false,
        status_label,
        agent_state,
        opencode_status: None,
        pr,
        wt_columns: BTreeMap::new(),
        unseen_comments: false,
    }
}

pub fn write_task_metadata(
    repo: &Repository,
    session: &Session,
    initial_prompt: &str,
) -> Result<(), String> {
    let summary = truncate(&initial_prompt.replace('\n', " "), 50);
    observability::with_writable_db(repo, |conn| {
        conn.execute(
            "insert into task_metadata (
                branch, prompt_summary, initial_prompt, worktree, updated_unix_ms
             ) values (?1, ?2, ?3, ?4, ?5)
             on conflict(branch) do update set
                prompt_summary = excluded.prompt_summary,
                initial_prompt = excluded.initial_prompt,
                worktree = excluded.worktree,
                updated_unix_ms = excluded.updated_unix_ms",
            params![
                session.branch.as_str(),
                summary.as_str(),
                initial_prompt,
                session.path_display.as_str(),
                unix_seconds(),
            ],
        )
        .map_err(|error| format!("write task metadata: {error}"))?;
        Ok(())
    })
}

pub fn remove_session_db_records(repo: &Repository, branch: &str) -> Result<(), String> {
    observability::with_writable_db(repo, |conn| {
        conn.execute_batch("begin transaction")
            .map_err(|error| format!("begin session cleanup transaction: {error}"))?;
        let result = (|| -> Result<(), String> {
            conn.execute(
                "delete from task_metadata where branch = ?1",
                params![branch],
            )
            .map_err(|error| format!("remove task metadata: {error}"))?;
            conn.execute("delete from pr_cache where branch = ?1", params![branch])
                .map_err(|error| format!("remove PR cache: {error}"))?;
            conn.execute(
                "delete from pr_details_cache where branch = ?1",
                params![branch],
            )
            .map_err(|error| format!("remove PR details cache: {error}"))?;
            conn.execute("delete from agent_state where branch = ?1", params![branch])
                .map_err(|error| format!("remove process state: {error}"))?;
            conn.execute(
                "delete from opencode_runtime where branch = ?1",
                params![branch],
            )
            .map_err(|error| format!("remove opencode runtime: {error}"))?;
            conn.execute(
                "delete from hidden_session where branch = ?1",
                params![branch],
            )
            .map_err(|error| format!("remove hidden marker: {error}"))?;
            Ok(())
        })();
        match result {
            Ok(()) => conn
                .execute_batch("commit")
                .map_err(|error| format!("commit session cleanup transaction: {error}")),
            Err(error) => {
                let _ = conn.execute_batch("rollback");
                Err(error)
            }
        }
    })
}

pub fn clear_hidden(repo: &Repository, branch: &str) -> Result<(), String> {
    observability::with_writable_db(repo, |conn| {
        conn.execute(
            "delete from hidden_session where branch = ?1",
            params![branch],
        )
        .map_err(|error| format!("remove hidden marker: {error}"))?;
        Ok(())
    })
}

pub fn append_runtime_log(repo: &Repository, message: &str) -> Result<(), String> {
    crate::observability::append_runtime_message(repo, message)
}

pub fn remove_logs(repo: &Repository, branch: &str) -> Result<(), String> {
    remove_if_exists(log_path(repo, branch), "agent log")
}

pub fn save_agent_state(repo: &Repository, branch: &str, state: AgentState) -> Result<(), String> {
    observability::with_writable_db(repo, |conn| {
        conn.execute(
            "insert into agent_state (branch, state, updated_unix_ms)
             values (?1, ?2, ?3)
             on conflict(branch) do update set
                state = excluded.state,
                updated_unix_ms = excluded.updated_unix_ms",
            params![branch, state.label(), unix_seconds()],
        )
        .map_err(|error| format!("write process state: {error}"))?;
        Ok(())
    })
}

fn load_agent_state(repo: &Repository, branch: &str) -> Option<AgentState> {
    let state = observability::with_writable_db(repo, |conn| {
        conn.query_row(
            "select state from agent_state where branch = ?1",
            params![branch],
            |row| row.get::<_, String>(0),
        )
        .map_err(|error| format!("read process state: {error}"))
    })
    .ok()?;
    AgentState::parse(&state)
}

struct TaskMetadata {
    prompt_summary: String,
}

fn load_task_metadata(repo: &Repository, branch: &str) -> Option<TaskMetadata> {
    observability::with_writable_db(repo, |conn| {
        conn.query_row(
            "select prompt_summary from task_metadata where branch = ?1",
            params![branch],
            |row| {
                Ok(TaskMetadata {
                    prompt_summary: row.get(0)?,
                })
            },
        )
        .map_err(|error| format!("read task metadata: {error}"))
    })
    .ok()
}

fn read_prompt_summary(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    for key in ["prompt_summary", "summary", "initial_prompt", "prompt"] {
        if let Some(value) = json_string_field(&text, key) {
            return Some(truncate(&value.replace('\n', " "), 50));
        }
    }
    None
}

fn log_path(repo: &Repository, branch: &str) -> PathBuf {
    repo.prism_dir()
        .join("logs")
        .join(format!("{}.log", safe_branch_filename(branch)))
}

fn remove_if_exists(path: PathBuf, label: &str) -> Result<(), String> {
    if path.exists() {
        fs::remove_file(path).map_err(|error| format!("remove {label}: {error}"))?;
    }
    Ok(())
}

fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::PromptMode;
    use crate::config::{Checks, EscapeKey, MergeMethod};

    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn discover_sessions_skips_missing_worktree_paths() {
        let temp = unique_temp_dir("prism-session-missing-worktree-test");
        let repo_path = temp.join("repo");
        let missing = temp.join("missing");
        fs::create_dir_all(&repo_path).unwrap();
        let git = temp.join("git");
        fs::write(
            &git,
            format!(
                r###"#!/bin/sh
case "$*" in
  *"worktree list --porcelain"*)
    cat <<'EOF'
worktree {}
HEAD abc
branch refs/heads/main

worktree {}
HEAD def
branch refs/heads/feat/missing

EOF
    exit 0
    ;;
  *"status --short --branch"*)
    echo "## main"
    exit 0
    ;;
esac
exit 0
"###,
                repo_path.display(),
                missing.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&git).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&git, permissions).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));

        let mut config = test_config();
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        let repo = Repository::with_config_dir_for_test(repo_path.clone(), temp.join("config"));

        let sessions = discover_sessions(&repo, &config).unwrap();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].path, repo_path);
        assert_eq!(sessions[0].branch, "main");

        let _ = fs::remove_dir_all(temp);
    }

    fn test_config() -> Config {
        Config {
            default_agent: "ask".to_string(),
            default_base: None,
            plan_dir: "plans".to_string(),
            review_packet_dir: ".agent/review".to_string(),
            worktree_command: "wt".to_string(),
            opencode_port_base: 41_000,
            opencode_port_span: 1_000,
            opencode_shutdown_owned_servers: false,
            escape_key: EscapeKey::EscEsc,
            merge_method: MergeMethod::Squash,
            checks: Checks::default(),
            worktree_columns: Vec::new(),
            tools: BTreeMap::new(),
            agent_commands: BTreeMap::new(),
            agent_prompt_modes: BTreeMap::<String, PromptMode>::new(),
            prompt_templates: BTreeMap::new(),
            user_path: PathBuf::from("/tmp/prism-test-user-config.toml"),
            repo_config_path: PathBuf::from("/tmp/prism-test-repo-config.toml"),
        }
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }
}
