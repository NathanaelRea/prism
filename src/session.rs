use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::params;

use crate::agent::AgentState;
use crate::config::Config;
use crate::git::git_status_label;
use crate::github::{PrCache, load_pr_cache_for_branch};
use crate::json::json_string_field;
use crate::observability::{self, LogLevel};
use crate::opencode::OpencodeStatus;
use crate::process::run_capture;
use crate::repo::Repository;
use crate::util::{safe_branch_filename, status_count, truncate};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum SessionClassification {
    #[default]
    Work,
    Planning,
    Exploration,
}

impl SessionClassification {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Work => "work",
            Self::Planning => "planning",
            Self::Exploration => "exploration",
        }
    }

    fn sort_rank(self) -> u8 {
        match self {
            Self::Work => 0,
            Self::Planning => 1,
            Self::Exploration => 2,
        }
    }

    fn parse(value: &str) -> Self {
        match value.trim() {
            "planning" => Self::Planning,
            "exploration" => Self::Exploration,
            _ => Self::Work,
        }
    }
}

#[derive(Debug)]
pub struct Session {
    pub repo_index: usize,
    pub repo_label: String,
    pub repo_key: Option<char>,
    pub path: PathBuf,
    pub path_display: String,
    pub branch: String,
    pub prompt_summary: String,
    pub classification: SessionClassification,
    pub adopted: bool,
    pub hidden: bool,
    pub status_label: String,
    pub agent_state: AgentState,
    pub opencode_status: Option<OpencodeStatus>,
    pub pr: PrCache,
    pub wt_columns: BTreeMap<String, String>,
    pub unseen_comments: bool,
}

impl Session {
    pub(crate) fn is_default_branch(&self, config: &Config) -> bool {
        config.is_default_branch(&self.branch)
    }

    pub(crate) fn is_detached(&self) -> bool {
        self.branch == "(detached)"
    }

    pub(crate) fn is_task_branch(&self, config: &Config) -> bool {
        !self.is_default_branch(config) && !self.is_detached()
    }

    pub(crate) fn identity_key(&self) -> WorktreeSessionKey {
        WorktreeSessionKey {
            repo_index: self.repo_index,
            path: self.path.clone(),
        }
    }

    pub(crate) fn matches_branch(&self, repo_index: usize, branch: &str) -> bool {
        self.repo_index == repo_index && self.branch == branch
    }

    pub(crate) fn apply_repo_identity(
        &mut self,
        repo_index: usize,
        repo_label: String,
        repo_key: Option<char>,
    ) {
        self.repo_index = repo_index;
        self.repo_label = repo_label;
        self.repo_key = repo_key;
    }

    pub(crate) fn preserve_refresh_state_from(&mut self, previous: Session, config: &Config) {
        self.agent_state = previous.agent_state;
        self.opencode_status = previous.opencode_status;
        self.wt_columns = previous.wt_columns;
        if self.is_task_branch(config) {
            self.pr = previous.pr;
            self.unseen_comments = previous.unseen_comments;
        } else {
            self.pr = PrCache::default();
            self.unseen_comments = false;
        }
    }

    pub(crate) fn mark_adopted_with_prompt(&mut self, initial_prompt: &str) {
        self.adopted = true;
        self.prompt_summary = prompt_summary_from_text(initial_prompt);
    }

    pub(crate) fn background_job_snapshot(&self) -> Self {
        Self {
            repo_index: self.repo_index,
            repo_label: self.repo_label.clone(),
            repo_key: self.repo_key,
            path: self.path.clone(),
            path_display: self.path_display.clone(),
            branch: self.branch.clone(),
            prompt_summary: self.prompt_summary.clone(),
            classification: self.classification,
            adopted: self.adopted,
            hidden: self.hidden,
            status_label: self.status_label.clone(),
            agent_state: self.agent_state,
            opencode_status: self.opencode_status.clone(),
            pr: self.pr.clone(),
            wt_columns: self.wt_columns.clone(),
            unseen_comments: self.unseen_comments,
        }
    }

    pub(crate) fn deletion_warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if status_count(&self.status_label, "dirty").is_some() {
            warnings.push("dirty worktree: uncommitted changes will be deleted".to_string());
        }
        if status_count(&self.status_label, "ahead").is_some() {
            warnings.push("branch is ahead of upstream: unpushed commits may be lost".to_string());
        }
        if status_count(&self.status_label, "behind").is_some() {
            warnings.push("branch is behind upstream".to_string());
        }
        if !self.adopted {
            warnings.push("session was not created by Prism".to_string());
        }
        if self.is_detached() {
            warnings.push("detached worktree: no local branch will be deleted".to_string());
        }
        if self.agent_state == AgentState::Running {
            warnings.push("agent is still running".to_string());
        }
        if let Some(summary) = &self.pr.summary
            && !summary.merged
        {
            warnings.push(format!("open PR #{} still exists", summary.number));
        }
        warnings
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct WorktreeSessionKey {
    pub repo_index: usize,
    pub path: PathBuf,
}

pub fn discover_sessions(repo: &Repository, config: &Config) -> Result<Vec<Session>, String> {
    let output = run_capture(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(&repo.root)
            .args(["worktree", "list", "--porcelain"]),
    )?;
    let hidden = load_hidden_sessions(repo);
    let mut sessions = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_branch: Option<String> = None;

    for line in output.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            if let Some(path) = current_path.take() {
                let branch = current_branch
                    .take()
                    .unwrap_or_else(|| "(detached)".to_string());
                if hidden.contains_key(&branch) {
                    observability::emit(observability::EventInput {
                        level: LogLevel::Debug,
                        target: "session",
                        action: "skip_archived_worktree",
                        operation_id: None,
                        parent_operation_id: None,
                        branch: Some(branch),
                        session: Some(path.display().to_string()),
                        message: format!("skipping archived worktree {}", path.display()),
                        data_json: None,
                    });
                } else if path.exists() {
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

    sessions.sort_by(|a, b| session_discovery_order(config, a, b));
    Ok(sessions)
}

pub(crate) fn session_discovery_order(
    config: &Config,
    a: &Session,
    b: &Session,
) -> std::cmp::Ordering {
    b.is_default_branch(config)
        .cmp(&a.is_default_branch(config))
        .then_with(|| {
            a.classification
                .sort_rank()
                .cmp(&b.classification.sort_rank())
        })
        .then_with(|| a.branch.cmp(&b.branch))
        .then_with(|| a.path.cmp(&b.path))
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
    let classification = metadata
        .as_ref()
        .map(|metadata| metadata.classification)
        .unwrap_or_default();
    let adopted = metadata.is_some() || legacy_metadata_path.exists();
    let status_label = git_status_label(&path, config);
    let path_display = path.display().to_string();
    let agent_state = load_agent_state(repo, &branch).unwrap_or(AgentState::Idle);
    let pr = load_pr_cache_for_branch(repo, config, &branch);
    Session {
        repo_index: 0,
        repo_label: String::new(),
        repo_key: None,
        path,
        path_display,
        branch,
        prompt_summary,
        classification,
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
    let summary = prompt_summary_from_text(initial_prompt);
    observability::with_writable_db(repo, |conn| {
        conn.execute(
            "insert into task_metadata (
                branch, prompt_summary, initial_prompt, worktree, classification, updated_unix_ms
             ) values (?1, ?2, ?3, ?4, ?5, ?6)
             on conflict(branch) do update set
                prompt_summary = excluded.prompt_summary,
                initial_prompt = excluded.initial_prompt,
                worktree = excluded.worktree,
                classification = excluded.classification,
                updated_unix_ms = excluded.updated_unix_ms",
            params![
                session.branch.as_str(),
                summary.as_str(),
                initial_prompt,
                session.path_display.as_str(),
                session.classification.label(),
                unix_seconds(),
            ],
        )
        .map_err(|error| format!("write task metadata: {error}"))?;
        Ok(())
    })
}

pub(crate) fn migrate_worktree_session_schema(conn: &rusqlite::Connection) -> Result<(), String> {
    conn.execute_batch(
        "
        create table if not exists task_metadata (
          branch text primary key,
          prompt_summary text not null,
          initial_prompt text not null,
          worktree text not null,
          classification text not null default 'work',
          updated_unix_ms integer not null
        );

        create table if not exists hidden_session (
          branch text primary key,
          hidden_unix_ms integer not null
        );

        create table if not exists archived_worktree (
          branch text primary key,
          repo_root text not null,
          worktree_path text not null,
          archived_unix_ms integer not null,
          classification text not null default 'work'
        );

        create table if not exists agent_state (
          branch text primary key,
          state text not null,
          updated_unix_ms integer not null
        );
        ",
    )
    .map_err(|error| format!("create worktree session schema: {error}"))?;
    add_column_if_missing(
        conn,
        "task_metadata",
        "classification",
        "alter table task_metadata add column classification text not null default 'work'",
    )?;
    Ok(())
}

pub(crate) fn archive_worktree_session(repo: &Repository, session: &Session) -> Result<(), String> {
    observability::with_writable_db(repo, |conn| {
        conn.execute_batch("begin transaction")
            .map_err(|error| format!("begin archive transaction: {error}"))?;
        let result = (|| -> Result<(), String> {
            conn.execute(
                "insert into hidden_session (branch, hidden_unix_ms)
                 values (?1, ?2)
                 on conflict(branch) do update set hidden_unix_ms = excluded.hidden_unix_ms",
                params![session.branch.as_str(), unix_seconds()],
            )
            .map_err(|error| format!("write hidden marker: {error}"))?;
            conn.execute(
                "insert into archived_worktree (
                    branch, repo_root, worktree_path, archived_unix_ms, classification
                 ) values (?1, ?2, ?3, ?4, ?5)
                 on conflict(branch) do update set
                    repo_root = excluded.repo_root,
                    worktree_path = excluded.worktree_path,
                    archived_unix_ms = excluded.archived_unix_ms,
                    classification = excluded.classification",
                params![
                    session.branch.as_str(),
                    repo.root.display().to_string(),
                    session.path_display.as_str(),
                    unix_seconds(),
                    session.classification.label(),
                ],
            )
            .map_err(|error| format!("write archived worktree metadata: {error}"))?;
            Ok(())
        })();
        match result {
            Ok(()) => conn
                .execute_batch("commit")
                .map_err(|error| format!("commit archive transaction: {error}")),
            Err(error) => {
                let _ = conn.execute_batch("rollback");
                Err(error)
            }
        }
    })
}

pub(crate) fn remove_task_metadata_with_conn(
    conn: &rusqlite::Connection,
    branch: &str,
) -> Result<(), String> {
    conn.execute(
        "delete from task_metadata where branch = ?1",
        params![branch],
    )
    .map_err(|error| format!("remove task metadata: {error}"))?;
    Ok(())
}

pub(crate) fn clear_hidden_session_marker_with_conn(
    conn: &rusqlite::Connection,
    branch: &str,
) -> Result<(), String> {
    conn.execute(
        "delete from hidden_session where branch = ?1",
        params![branch],
    )
    .map_err(|error| format!("remove hidden marker: {error}"))?;
    Ok(())
}

pub(crate) fn clear_hidden_session_marker(repo: &Repository, branch: &str) -> Result<(), String> {
    observability::with_writable_db(repo, |conn| {
        clear_hidden_session_marker_with_conn(conn, branch)
    })
}

pub fn append_runtime_log(repo: &Repository, message: &str) -> Result<(), String> {
    crate::observability::append_runtime_message(repo, message)
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

pub(crate) fn remove_agent_state_with_conn(
    conn: &rusqlite::Connection,
    branch: &str,
) -> Result<(), String> {
    conn.execute("delete from agent_state where branch = ?1", params![branch])
        .map_err(|error| format!("remove process state: {error}"))?;
    Ok(())
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
    classification: SessionClassification,
}

fn load_task_metadata(repo: &Repository, branch: &str) -> Option<TaskMetadata> {
    observability::with_writable_db(repo, |conn| {
        conn.query_row(
            "select prompt_summary, classification from task_metadata where branch = ?1",
            params![branch],
            |row| {
                Ok(TaskMetadata {
                    prompt_summary: row.get(0)?,
                    classification: SessionClassification::parse(&row.get::<_, String>(1)?),
                })
            },
        )
        .map_err(|error| format!("read task metadata: {error}"))
    })
    .ok()
}

fn load_hidden_sessions(repo: &Repository) -> BTreeMap<String, i64> {
    observability::with_writable_db(repo, |conn| {
        let mut statement = conn
            .prepare("select branch, hidden_unix_ms from hidden_session")
            .map_err(|error| format!("read hidden sessions: {error}"))?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(|error| format!("read hidden sessions: {error}"))?;
        let mut hidden = BTreeMap::new();
        for row in rows {
            let (branch, hidden_unix_ms) =
                row.map_err(|error| format!("read hidden session: {error}"))?;
            hidden.insert(branch, hidden_unix_ms);
        }
        Ok(hidden)
    })
    .unwrap_or_default()
}

fn add_column_if_missing(
    conn: &rusqlite::Connection,
    table: &str,
    column: &str,
    sql: &str,
) -> Result<(), String> {
    let mut statement = conn
        .prepare(&format!("pragma table_info({table})"))
        .map_err(|error| format!("inspect {table} schema: {error}"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| format!("inspect {table} schema: {error}"))?;
    for value in columns {
        if value.map_err(|error| format!("inspect {table} schema: {error}"))? == column {
            return Ok(());
        }
    }
    conn.execute_batch(sql)
        .map_err(|error| format!("migrate {table}.{column}: {error}"))
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

fn prompt_summary_from_text(text: &str) -> String {
    truncate(&text.replace('\n', " "), 50)
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

    #[test]
    fn worktree_session_default_branch_sorts_first() {
        let mut config = test_config();
        config.default_base = Some("main".to_string());
        let main = test_session("main", "/repo/main");
        let feature = test_session("feature", "/repo/feature");

        assert_eq!(
            session_discovery_order(&config, &main, &feature),
            std::cmp::Ordering::Less
        );
        assert!(main.is_default_branch(&config));
        assert!(feature.is_task_branch(&config));
    }

    #[test]
    fn planning_and_exploration_sessions_sort_below_work_sessions() {
        let config = test_config();
        let work = test_session("feature-a", "/repo/a");
        let mut planning = test_session("feature-b", "/repo/b");
        planning.classification = SessionClassification::Planning;
        let mut exploration = test_session("feature-c", "/repo/c");
        exploration.classification = SessionClassification::Exploration;

        assert_eq!(
            session_discovery_order(&config, &work, &planning),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            session_discovery_order(&config, &planning, &exploration),
            std::cmp::Ordering::Less
        );
    }

    #[test]
    fn archived_worktree_metadata_records_restore_details_and_hides_session() {
        let temp = unique_temp_dir("prism-archive-worktree-test");
        let repo_path = temp.join("repo");
        let worktree = temp.join("worktree");
        fs::create_dir_all(&repo_path).unwrap();
        fs::create_dir_all(&worktree).unwrap();
        let repo = Repository::with_config_dir_for_test(repo_path.clone(), temp.join("config"));
        let mut session = test_session("feature", &worktree.display().to_string());
        session.classification = SessionClassification::Planning;

        archive_worktree_session(&repo, &session).unwrap();

        let row = observability::with_writable_db(&repo, |conn| {
            conn.query_row(
                "select repo_root, worktree_path, classification from archived_worktree where branch = ?1",
                params!["feature"],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?)),
            )
            .map_err(|error| format!("read archived metadata: {error}"))
        })
        .unwrap();

        assert_eq!(row.0, repo_path.display().to_string());
        assert_eq!(row.1, worktree.display().to_string());
        assert_eq!(row.2, "planning");
        assert!(load_hidden_sessions(&repo).contains_key("feature"));

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn refresh_state_preserves_runtime_pr_columns_and_unseen_comments() {
        let config = test_config();
        let mut fresh = test_session("feature", "/repo/feature");
        let mut previous = test_session("feature", "/repo/feature");
        previous.agent_state = AgentState::Running;
        previous
            .wt_columns
            .insert("ci".to_string(), "passed".to_string());
        previous.unseen_comments = true;

        fresh.preserve_refresh_state_from(previous, &config);

        assert_eq!(fresh.agent_state, AgentState::Running);
        assert_eq!(
            fresh.wt_columns.get("ci").map(String::as_str),
            Some("passed")
        );
        assert!(fresh.unseen_comments);
    }

    #[test]
    fn refresh_state_clears_pr_state_for_non_task_branches() {
        let mut config = test_config();
        config.default_base = Some("main".to_string());
        let mut fresh = test_session("main", "/repo/main");
        let mut previous = test_session("main", "/repo/main");
        previous.pr.error = Some("stale".to_string());
        previous.unseen_comments = true;

        fresh.preserve_refresh_state_from(previous, &config);

        assert!(fresh.pr.error.is_none());
        assert!(!fresh.unseen_comments);
    }

    #[test]
    fn mark_adopted_with_prompt_updates_local_metadata_facts() {
        let mut session = test_session("feature", "/repo/feature");

        session.mark_adopted_with_prompt("first line\nsecond line with extra text");

        assert!(session.adopted);
        assert_eq!(
            session.prompt_summary,
            "first line second line with extra text"
        );
    }

    #[test]
    fn deletion_warnings_describe_worktree_session_local_risks() {
        let mut session = test_session("(detached)", "/repo/detached");
        session.status_label = "dirty 1 ahead 2 behind 3".to_string();
        session.adopted = false;
        session.agent_state = AgentState::Running;

        let warnings = session.deletion_warnings();

        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("dirty worktree"))
        );
        assert!(warnings.iter().any(|warning| warning.contains("unpushed")));
        assert!(warnings.iter().any(|warning| warning.contains("behind")));
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("not created"))
        );
        assert!(warnings.iter().any(|warning| warning.contains("detached")));
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("agent is still running"))
        );
    }

    fn test_session(branch: &str, path: &str) -> Session {
        Session {
            repo_index: 0,
            repo_label: "repo".to_string(),
            repo_key: None,
            path: PathBuf::from(path),
            path_display: path.to_string(),
            branch: branch.to_string(),
            prompt_summary: String::new(),
            classification: SessionClassification::Work,
            adopted: true,
            hidden: false,
            status_label: "clean".to_string(),
            agent_state: AgentState::Idle,
            opencode_status: None,
            pr: PrCache::default(),
            wt_columns: BTreeMap::new(),
            unseen_comments: false,
        }
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
            opencode_plan_plugin: false,
            escape_key: EscapeKey::EscEsc,
            merge_method: MergeMethod::Squash,
            auto: crate::config::AutoConfig::default(),
            layout: crate::config::LayoutConfig::default(),
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
