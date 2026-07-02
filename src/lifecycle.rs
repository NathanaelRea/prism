use std::path::Path;
use std::process::Command;

use crate::config::{Config, MergeMethod};
use crate::github::{PrCache, refresh_pr_cache};
use crate::observability;
use crate::opencode;
use crate::process::{run_capture, run_configured_commands, run_status};
use crate::repo::Repository;
use crate::session::{
    clear_hidden_session_marker, clear_hidden_session_marker_with_conn, hidden_session_exists,
    remove_agent_state_with_conn, remove_task_metadata_with_conn,
};
use crate::util::safe_branch_filename;

pub(crate) fn create_worktree_session(
    repo: &Repository,
    config: &Config,
    branch: &str,
) -> Result<(), String> {
    if hidden_session_exists(repo, branch)? && branch_has_worktree(repo, config, branch)? {
        clear_hidden_session_marker(repo, branch)?;
        return Ok(());
    }
    run_capture(
        Command::new(config.tool(&config.worktree_command)).args(create_worktree_args(
            &repo.root,
            branch,
            config.default_base.as_deref(),
        )),
    )?;
    clear_hidden_session_marker(repo, branch)?;
    Ok(())
}

fn branch_has_worktree(repo: &Repository, config: &Config, branch: &str) -> Result<bool, String> {
    let output = run_capture(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(&repo.root)
            .args(["worktree", "list", "--porcelain"]),
    )?;
    Ok(output.lines().any(|line| {
        line.strip_prefix("branch refs/heads/")
            .is_some_and(|current| current == branch)
    }))
}

pub(crate) fn move_current_branch_to_worktree(
    repo: &Repository,
    config: &Config,
    branch: &str,
    base: &str,
) -> Result<(), String> {
    run_status(Command::new(config.tool("git")).args(switch_checkout_args(&repo.root, base)))?;
    run_status(
        Command::new(config.tool(&config.worktree_command))
            .args(move_branch_to_worktree_args(&repo.root, branch)),
    )?;
    let _ = crate::session::append_runtime_log(
        repo,
        &format!("moved {branch} into Worktrunk worktree and switched checkout to {base}"),
    );
    Ok(())
}

pub(crate) fn push_branch(
    config: &Config,
    path: &Path,
    branch: &str,
    set_upstream: bool,
) -> Result<(), String> {
    let args = if set_upstream {
        vec![
            "push".to_string(),
            "-u".to_string(),
            "origin".to_string(),
            branch.to_string(),
        ]
    } else {
        vec!["push".to_string()]
    };
    run_capture(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(path)
            .args(args),
    )?;
    Ok(())
}

pub(crate) fn run_pre_push_checks(config: &Config, path: &Path) -> Result<(), String> {
    run_configured_commands(&config.checks.pre_push, path, "pre_push")
}

pub(crate) fn run_pre_pr_checks(config: &Config, path: &Path) -> Result<(), String> {
    run_configured_commands(&config.checks.pre_pr, path, "pre_pr")
}

pub(crate) fn refresh_branch_pr_cache(
    repo: &Repository,
    config: &Config,
    branch: &str,
    path: &Path,
    cache: &mut PrCache,
    force: bool,
) {
    refresh_pr_cache(repo, branch, cache, path, config, force);
}

pub(crate) fn create_pull_request(
    repo: &Repository,
    config: &Config,
    branch: &str,
    path: &Path,
    body: &str,
    cache: &mut PrCache,
) -> Result<(), String> {
    run_capture(
        Command::new(config.tool("gh"))
            .args(create_pr_args(config.default_base.as_deref(), body))
            .current_dir(path),
    )?;
    refresh_branch_pr_cache(repo, config, branch, path, cache, true);
    Ok(())
}

pub(crate) fn merge_pull_request(
    config: &Config,
    path: &Path,
    pr_number: u64,
) -> Result<(), String> {
    run_status(
        Command::new(config.tool("gh"))
            .args(merge_pr_args(&pr_number.to_string(), config.merge_method))
            .current_dir(path),
    )?;
    Ok(())
}

pub(crate) fn delete_worktree_session_local_data(
    repo: &Repository,
    branch: &str,
) -> Result<(), String> {
    remove_worktree_session_db_records(repo, branch)?;
    remove_worktree_session_logs(repo, branch)?;
    Ok(())
}

pub(crate) fn delete_worktree_session(
    repo: &Repository,
    config: &Config,
    path: &Path,
    branch: &str,
) -> Result<(), String> {
    delete_worktree_session_processes(repo, config, branch)?;
    delete_worktree_session_local_data(repo, branch)?;
    remove_worktree(repo, config, path)?;
    delete_branch_if_attached(repo, config, branch)?;
    Ok(())
}

fn delete_worktree_session_processes(
    repo: &Repository,
    config: &Config,
    branch: &str,
) -> Result<(), String> {
    crate::tmux::kill_agent_sessions_for_branch(repo, config, branch)?;
    for runtime in opencode::load_runtimes_for_branch(repo, branch)? {
        opencode::shutdown_stored_server(&runtime)?;
    }
    Ok(())
}

fn create_worktree_args(repo_root: &Path, branch: &str, default_base: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "-C".to_string(),
        repo_root.display().to_string(),
        "switch".to_string(),
        "--create".to_string(),
        "--no-cd".to_string(),
        "--format".to_string(),
        "json".to_string(),
    ];
    if let Some(base) = default_base.map(str::trim).filter(|base| !base.is_empty()) {
        args.push("--base".to_string());
        args.push(base.to_string());
    }
    args.push(branch.to_string());
    args
}

fn switch_checkout_args(repo_root: &Path, branch: &str) -> Vec<String> {
    vec![
        "-C".to_string(),
        repo_root.display().to_string(),
        "switch".to_string(),
        branch.to_string(),
    ]
}

fn move_branch_to_worktree_args(repo_root: &Path, branch: &str) -> Vec<String> {
    vec![
        "-C".to_string(),
        repo_root.display().to_string(),
        "switch".to_string(),
        "--no-cd".to_string(),
        "--format".to_string(),
        "json".to_string(),
        branch.to_string(),
    ]
}

fn create_pr_args(default_base: Option<&str>, body: &str) -> Vec<String> {
    let mut args = vec![
        "pr".to_string(),
        "create".to_string(),
        "--fill".to_string(),
        "--body".to_string(),
        body.to_string(),
    ];
    if let Some(base) = default_base.map(str::trim).filter(|base| !base.is_empty()) {
        args.push("--base".to_string());
        args.push(base.to_string());
    }
    args
}

fn merge_pr_args(pr_number: &str, method: MergeMethod) -> Vec<String> {
    vec![
        "pr".to_string(),
        "merge".to_string(),
        pr_number.to_string(),
        method.gh_flag().to_string(),
    ]
}

fn delete_branch_if_attached(
    repo: &Repository,
    config: &Config,
    branch: &str,
) -> Result<(), String> {
    if branch == "(detached)" {
        return Ok(());
    }
    run_status(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(&repo.root)
            .args(["branch", "-D", branch]),
    )
}

fn remove_worktree(repo: &Repository, config: &Config, path: &Path) -> Result<(), String> {
    let remove_result = run_status(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(&repo.root)
            .args(["worktree", "remove", "--force"])
            .arg(path),
    );
    match remove_result {
        Ok(()) => Ok(()),
        Err(error) if !path.exists() => prune_worktrees(repo, config).map_err(|prune_error| {
            format!("{error}; also failed to prune worktrees: {prune_error}")
        }),
        Err(error) => Err(error),
    }
}

fn prune_worktrees(repo: &Repository, config: &Config) -> Result<(), String> {
    run_status(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(&repo.root)
            .args(["worktree", "prune"]),
    )
}

fn remove_worktree_session_db_records(repo: &Repository, branch: &str) -> Result<(), String> {
    observability::with_writable_db(repo, |conn| {
        conn.execute_batch("begin transaction")
            .map_err(|error| format!("begin worktree session cleanup transaction: {error}"))?;
        let result = (|| -> Result<(), String> {
            remove_task_metadata_with_conn(conn, branch)?;
            crate::github::remove_pr_cache_with_conn(conn, branch)?;
            remove_agent_state_with_conn(conn, branch)?;
            crate::opencode::remove_runtime_for_branch_with_conn(conn, branch)?;
            clear_hidden_session_marker_with_conn(conn, branch)?;
            conn.execute(
                "delete from archived_worktree where branch = ?1",
                rusqlite::params![branch],
            )
            .map_err(|error| format!("remove archived worktree metadata: {error}"))?;
            Ok(())
        })();
        match result {
            Ok(()) => conn
                .execute_batch("commit")
                .map_err(|error| format!("commit worktree session cleanup transaction: {error}")),
            Err(error) => {
                let _ = conn.execute_batch("rollback");
                Err(error)
            }
        }
    })
}

fn remove_worktree_session_logs(repo: &Repository, branch: &str) -> Result<(), String> {
    remove_if_exists(worktree_session_log_path(repo, branch), "agent log")
}

fn worktree_session_log_path(repo: &Repository, branch: &str) -> std::path::PathBuf {
    repo.prism_dir()
        .join("logs")
        .join(format!("{}.log", safe_branch_filename(branch)))
}

fn remove_if_exists(path: std::path::PathBuf, label: &str) -> Result<(), String> {
    if path.exists() {
        std::fs::remove_file(path).map_err(|error| format!("remove {label}: {error}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        create_pr_args, create_worktree_args, merge_pr_args, move_branch_to_worktree_args,
        remove_worktree, switch_checkout_args,
    };
    use crate::config::{Checks, Config, EscapeKey, MergeMethod};
    use crate::observability;
    use crate::repo::Repository;
    use rusqlite::params;
    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn create_worktree_uses_worktrunk_without_changing_directory() {
        let args = create_worktree_args(
            PathBuf::from("/repo/prism").as_path(),
            "feat/test",
            Some("main"),
        );

        assert_eq!(
            args,
            vec![
                "-C",
                "/repo/prism",
                "switch",
                "--create",
                "--no-cd",
                "--format",
                "json",
                "--base",
                "main",
                "feat/test",
            ]
        );
    }

    #[test]
    fn create_worktree_session_clears_stale_hidden_marker() {
        let temp = unique_temp_dir("prism-create-clears-hidden-test");
        fs::create_dir_all(&temp).unwrap();
        let wt = temp.join("wt");
        fs::write(&wt, "#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = fs::metadata(&wt).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&wt, permissions).unwrap();
        let git = temp.join("git");
        fs::write(&git, "#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = fs::metadata(&git).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&git, permissions).unwrap();

        let mut config = test_config();
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        config
            .tools
            .insert("wt".to_string(), wt.display().to_string());
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        observability::with_writable_db(&repo, |conn| {
            conn.execute(
                "insert into hidden_session (branch, hidden_unix_ms) values (?1, ?2)",
                params!["feature", 123_i64],
            )
            .unwrap();
            Ok(())
        })
        .unwrap();

        super::create_worktree_session(&repo, &config, "feature").unwrap();

        let hidden = count_rows(&repo, "hidden_session", "feature");
        assert_eq!(hidden, 0);

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn create_worktree_session_restores_archived_worktree_without_creating() {
        let temp = unique_temp_dir("prism-create-restores-archived-test");
        fs::create_dir_all(&temp).unwrap();
        let git = temp.join("git");
        fs::write(
            &git,
            "#!/bin/sh\nprintf 'worktree /repo/prism.feature\\nHEAD abc\\nbranch refs/heads/feature\\n\\n'\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&git).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&git, permissions).unwrap();
        let wt = temp.join("wt");
        fs::write(&wt, "#!/bin/sh\nexit 99\n").unwrap();
        let mut permissions = fs::metadata(&wt).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&wt, permissions).unwrap();

        let mut config = test_config();
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        config
            .tools
            .insert("wt".to_string(), wt.display().to_string());
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        observability::with_writable_db(&repo, |conn| {
            conn.execute(
                "insert into hidden_session (branch, hidden_unix_ms) values (?1, ?2)",
                params!["feature", 123_i64],
            )
            .unwrap();
            Ok(())
        })
        .unwrap();

        super::create_worktree_session(&repo, &config, "feature").unwrap();

        assert_eq!(count_rows(&repo, "hidden_session", "feature"), 0);

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn move_current_branch_args_switch_checkout_then_worktrunk_session() {
        let repo = PathBuf::from("/repo/prism");

        assert_eq!(
            switch_checkout_args(&repo, "main"),
            vec!["-C", "/repo/prism", "switch", "main"]
        );
        assert_eq!(
            move_branch_to_worktree_args(&repo, "feat/test"),
            vec![
                "-C",
                "/repo/prism",
                "switch",
                "--no-cd",
                "--format",
                "json",
                "feat/test",
            ]
        );
    }

    #[test]
    fn create_pr_uses_fill_with_explicit_empty_body_and_default_base_when_configured() {
        assert_eq!(
            create_pr_args(Some("main"), ""),
            vec!["pr", "create", "--fill", "--body", "", "--base", "main"]
        );
        assert_eq!(
            create_pr_args(None, "manual description"),
            vec!["pr", "create", "--fill", "--body", "manual description"]
        );
    }

    #[test]
    fn merge_pr_args_use_configured_method() {
        assert_eq!(
            merge_pr_args("42", MergeMethod::Squash),
            vec!["pr", "merge", "42", "--squash"]
        );
        assert_eq!(
            merge_pr_args("42", MergeMethod::Merge),
            vec!["pr", "merge", "42", "--merge"]
        );
        assert_eq!(
            merge_pr_args("42", MergeMethod::Rebase),
            vec!["pr", "merge", "42", "--rebase"]
        );
    }

    #[test]
    fn merge_pull_request_does_not_delegate_branch_deletion_to_gh() {
        let temp = unique_temp_dir("prism-merge-no-delete-branch-test");
        let worktree = temp.join("worktree");
        fs::create_dir_all(&worktree).unwrap();
        let log = temp.join("gh.log");
        let gh = temp.join("gh");
        fs::write(
            &gh,
            format!(
                r#"#!/bin/sh
printf 'pwd=%s\nargs=%s\n' "$PWD" "$*" > '{}'
exit 0
"#,
                log.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&gh).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&gh, permissions).unwrap();

        let mut config = test_config();
        config
            .tools
            .insert("gh".to_string(), gh.display().to_string());

        super::merge_pull_request(&config, &worktree, 42).unwrap();

        let commands = fs::read_to_string(&log).unwrap();
        let actual_pwd = commands
            .lines()
            .find_map(|line| line.strip_prefix("pwd="))
            .expect("gh shim should record its working directory");
        assert_eq!(
            Path::new(actual_pwd).canonicalize().unwrap(),
            worktree.canonicalize().unwrap()
        );
        assert!(commands.contains("args=pr merge 42 --squash"));
        assert!(!commands.contains("--delete-branch"));

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn remove_worktree_prunes_when_missing_path_cannot_be_removed() {
        let temp = unique_temp_dir("prism-remove-worktree-prune-test");
        fs::create_dir_all(&temp).unwrap();
        let log = temp.join("git.log");
        let git = temp.join("git");
        fs::write(
            &git,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$*" in
  *"worktree remove"*)
    echo "not a working tree" >&2
    exit 1
    ;;
  *"worktree prune"*)
    exit 0
    ;;
esac
exit 0
"#,
                log.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&git).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&git, permissions).unwrap();

        let mut config = test_config();
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let missing = temp.join("missing");

        remove_worktree(&repo, &config, &missing).unwrap();

        let commands = fs::read_to_string(&log).unwrap();
        assert!(commands.contains("worktree prune"));

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn remove_worktree_does_not_prune_after_successful_remove() {
        let temp = unique_temp_dir("prism-remove-worktree-no-prune-test");
        fs::create_dir_all(&temp).unwrap();
        let log = temp.join("git.log");
        let git = temp.join("git");
        fs::write(
            &git,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
exit 0
"#,
                log.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&git).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&git, permissions).unwrap();

        let mut config = test_config();
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let path = temp.join("worktree");
        fs::create_dir_all(&path).unwrap();

        remove_worktree(&repo, &config, &path).unwrap();

        let commands = fs::read_to_string(&log).unwrap();
        assert!(commands.contains("worktree remove --force"));
        assert!(!commands.contains("worktree prune"));

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn delete_worktree_session_kills_tmux_sessions_and_removes_state() {
        let temp = unique_temp_dir("prism-delete-kills-tmux-test");
        fs::create_dir_all(&temp).unwrap();
        let tmux_log = temp.join("tmux.log");
        let git_log = temp.join("git.log");
        let tmux = temp.join("tmux");
        let git = temp.join("git");
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        let branch = "feature/delete";
        let runtime = crate::tmux::TmuxAgentSession::for_worktree_session(&repo, branch, 3);
        let other_runtime =
            crate::tmux::TmuxAgentSession::for_worktree_session(&repo, "feature/keep", 0);
        fs::write(
            &tmux,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$1" in
  list-sessions)
    printf '%s\n' '{}' '{}'
    exit 0
    ;;
  kill-session)
    exit 0
    ;;
esac
exit 1
"#,
                tmux_log.display(),
                runtime.name(),
                other_runtime.name()
            ),
        )
        .unwrap();
        fs::write(
            &git,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
exit 0
"#,
                git_log.display()
            ),
        )
        .unwrap();
        for shim in [&tmux, &git] {
            let mut permissions = fs::metadata(shim).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(shim, permissions).unwrap();
        }

        let mut config = test_config();
        config
            .tools
            .insert("tmux".to_string(), tmux.display().to_string());
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        let path = temp.join("worktree");
        fs::create_dir_all(&path).unwrap();
        observability::with_writable_db(&repo, |conn| {
            conn.execute(
                "insert into opencode_runtime (
                    repo_root, branch, worktree_path, server_port, server_url,
                    generation, updated_unix_ms
                 ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    repo.root.display().to_string(),
                    branch,
                    path.display().to_string(),
                    41000_i64,
                    "http://127.0.0.1:41000",
                    1_i64,
                    123_i64,
                ],
            )
            .unwrap();
            Ok(())
        })
        .unwrap();

        super::delete_worktree_session(&repo, &config, &path, branch).unwrap();

        let tmux_commands = fs::read_to_string(&tmux_log).unwrap();
        assert!(tmux_commands.contains("list-sessions -F #{session_name}"));
        assert!(tmux_commands.contains(&format!("kill-session -t {}", runtime.name())));
        assert!(!tmux_commands.contains(&format!("kill-session -t {}", other_runtime.name())));
        let git_commands = fs::read_to_string(&git_log).unwrap();
        assert!(git_commands.contains("worktree remove --force"));
        assert!(git_commands.contains("branch -D feature/delete"));
        assert_eq!(count_rows(&repo, "opencode_runtime", branch), 0);

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn delete_worktree_session_local_data_removes_owned_state_and_logs() {
        let temp = unique_temp_dir("prism-delete-local-data-test");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        let branch = "feature/delete";

        observability::with_writable_db(&repo, |conn| {
            conn.execute(
                "insert into task_metadata (
                    branch, prompt_summary, initial_prompt, worktree, updated_unix_ms
                 ) values (?1, ?2, ?3, ?4, ?5)",
                params![branch, "summary", "prompt", "/repo/wt", 123_i64],
            )
            .unwrap();
            conn.execute(
                "insert into hidden_session (branch, hidden_unix_ms) values (?1, ?2)",
                params![branch, 123_i64],
            )
            .unwrap();
            conn.execute(
                "insert into agent_state (branch, state, updated_unix_ms) values (?1, ?2, ?3)",
                params![branch, "running", 123_i64],
            )
            .unwrap();
            conn.execute(
                "insert into opencode_runtime (
                    repo_root, branch, worktree_path, server_port, server_url,
                    generation, updated_unix_ms
                 ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    repo.root.display().to_string(),
                    branch,
                    "/repo/wt",
                    41000_i64,
                    "http://127.0.0.1:41000",
                    1_i64,
                    123_i64,
                ],
            )
            .unwrap();
            Ok(())
        })
        .unwrap();
        let log = repo
            .prism_dir()
            .join("logs")
            .join(format!("{}.log", crate::util::safe_branch_filename(branch)));
        fs::create_dir_all(log.parent().unwrap()).unwrap();
        fs::write(&log, "agent log\n").unwrap();

        super::delete_worktree_session_local_data(&repo, branch).unwrap();

        assert_eq!(count_rows(&repo, "task_metadata", branch), 0);
        assert_eq!(count_rows(&repo, "hidden_session", branch), 0);
        assert_eq!(count_rows(&repo, "agent_state", branch), 0);
        assert_eq!(count_rows(&repo, "opencode_runtime", branch), 0);
        assert!(!log.exists());

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
            opencode_plan_plugin: false,
            escape_key: EscapeKey::EscEsc,
            merge_method: MergeMethod::Squash,
            auto: crate::config::AutoConfig::default(),
            layout: crate::config::LayoutConfig::default(),
            checks: Checks::default(),
            worktree_columns: Vec::new(),
            tools: BTreeMap::new(),
            agent_commands: BTreeMap::new(),
            agent_prompt_modes: BTreeMap::new(),
            prompt_templates: BTreeMap::new(),
            user_path: PathBuf::from("/tmp/prism-user-config.toml"),
            repo_config_path: PathBuf::from("/tmp/prism-repo-config.toml"),
        }
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{id}"))
    }

    fn count_rows(repo: &Repository, table: &str, branch: &str) -> i64 {
        observability::with_writable_db(repo, |conn| {
            conn.query_row(
                &format!("select count(*) from {table} where branch = ?1"),
                params![branch],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|error| error.to_string())
        })
        .unwrap()
    }
}
