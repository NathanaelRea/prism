use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;

use crate::config::Config;
use crate::observability;
use crate::process::{
    ProcessOutput, run_capture, run_configured_commands, run_output, run_output_allow_failure,
    run_status, run_status_inherited,
};
use crate::repo::Repository;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorktreeInventoryEntry {
    pub path: PathBuf,
    pub branch: String,
}

pub(crate) fn list_worktrees(
    repo: &Repository,
    config: &Config,
) -> Result<Vec<WorktreeInventoryEntry>, String> {
    let output = run_capture(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(&repo.root)
            .args(["worktree", "list", "--porcelain"]),
    )?;
    Ok(parse_worktree_inventory(&output))
}

fn parse_worktree_inventory(output: &str) -> Vec<WorktreeInventoryEntry> {
    let mut entries = Vec::new();
    let mut current_path = None;
    let mut current_branch = None;
    for line in output.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            if let Some(path) = current_path.take() {
                entries.push(WorktreeInventoryEntry {
                    path,
                    branch: current_branch
                        .take()
                        .unwrap_or_else(|| "(detached)".to_string()),
                });
            }
        } else if let Some(path) = line.strip_prefix("worktree ") {
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
    entries
}

static WORKTRUNK_APPROVAL_FAILURE_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?is)needs\s+approval.*cannot\s+prompt.*non[- ]interactive").unwrap()
});

pub(crate) fn create_worktree(
    repo: &Repository,
    config: &Config,
    branch: &str,
) -> Result<(), String> {
    let mut command = Command::new(config.tool(&config.worktree_command));
    command.args(create_worktree_args(
        &repo.root,
        branch,
        config.default_base.as_deref(),
    ));
    let command_display = observability::command_display(&command);
    let output = run_output(&mut command)?;
    if !output.status.success() {
        return Err(worktree_command_failure_message(
            &command_display,
            &output,
            repo,
            config,
        ));
    }
    Ok(())
}

pub(crate) fn checkout_worktree(
    repo: &Repository,
    config: &Config,
    branch: &str,
) -> Result<(), String> {
    let mut command = Command::new(config.tool(&config.worktree_command));
    command.args(checkout_worktree_args(&repo.root, branch));
    let command_display = observability::command_display(&command);
    let output = run_output(&mut command)?;
    if !output.status.success() {
        return Err(worktree_command_failure_message(
            &command_display,
            &output,
            repo,
            config,
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorktrunkApprovalStatus {
    NotWorktrunk,
    Approved,
    Pending,
}

pub(crate) fn check_worktrunk_approval_status(
    repo: &Repository,
    config: &Config,
) -> Result<WorktrunkApprovalStatus, String> {
    if config.worktree_command != "wt" {
        return Ok(WorktrunkApprovalStatus::NotWorktrunk);
    }
    let output = run_output_allow_failure(
        Command::new(config.tool(&config.worktree_command))
            .arg("-C")
            .arg(&repo.root)
            .args(["config", "approvals", "add"]),
    )?;
    if output.status.success() {
        return Ok(WorktrunkApprovalStatus::Approved);
    }
    if is_worktrunk_approval_failure(&process_output_text(&output)) {
        return Ok(WorktrunkApprovalStatus::Pending);
    }
    Err(format!(
        "{}: {}",
        worktrunk_approval_command_display(repo, config),
        process_failure_message(&output)
    ))
}

pub(crate) fn run_worktrunk_approval_prompt(
    repo: &Repository,
    config: &Config,
) -> Result<(), String> {
    run_status_inherited(
        Command::new(config.tool(&config.worktree_command))
            .arg("-C")
            .arg(&repo.root)
            .args(["config", "approvals", "add"]),
    )
}

pub(crate) fn is_worktrunk_approval_failure(output: &str) -> bool {
    WORKTRUNK_APPROVAL_FAILURE_RE.is_match(output)
}

pub(crate) fn branch_has_worktree(
    repo: &Repository,
    config: &Config,
    branch: &str,
) -> Result<bool, String> {
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
    let _ = crate::observability::append_runtime_message(
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

fn checkout_worktree_args(repo_root: &Path, branch: &str) -> Vec<String> {
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

fn worktree_command_failure_message(
    command_display: &str,
    output: &ProcessOutput,
    repo: &Repository,
    config: &Config,
) -> String {
    if is_worktrunk_approval_failure(&process_output_text(output)) {
        let message = format!("{command_display}: {}", process_output_text(output).trim());
        format!("{message}\n\n{}", worktrunk_approval_hint(repo, config))
    } else {
        let message = format!("{command_display}: {}", process_failure_message(output));
        message
    }
}

fn worktrunk_approval_hint(repo: &Repository, config: &Config) -> String {
    format!(
        "This repo has Worktrunk project commands that must be approved before Prism can create worktrees.\n\nRun:\n{}",
        worktrunk_approval_command_display(repo, config)
    )
}

fn worktrunk_approval_command_display(repo: &Repository, config: &Config) -> String {
    format!(
        "{} -C {} config approvals add",
        shell_quote(&config.tool(&config.worktree_command)),
        shell_quote(&repo.root.display().to_string())
    )
}

fn process_failure_message(output: &ProcessOutput) -> String {
    let stderr = first_non_empty_line(&output.stderr);
    let stdout = first_non_empty_line(&output.stdout);
    if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        format!("exited with {}", output.status)
    }
}

fn process_output_text(output: &ProcessOutput) -> String {
    format!("{}\n{}", output.stdout, output.stderr)
}

fn first_non_empty_line(output: &str) -> String {
    output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_string()
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | ':'))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
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

pub(crate) fn delete_branch_if_same_incarnation(
    repo: &Repository,
    config: &Config,
    branch: &str,
    expected_oid: Option<&str>,
) -> Result<(), String> {
    if branch == "(detached)" {
        return Ok(());
    }
    if branch_has_worktree(repo, config, branch)? {
        return Err(format!(
            "branch {branch} is attached to a new worktree and was retained"
        ));
    }
    let current_oid = branch_oid(repo, config, branch)?;
    if expected_oid.is_some() && Some(current_oid.as_str()) != expected_oid {
        return Err(format!(
            "branch {branch} changed while deletion was in progress and was retained"
        ));
    }
    run_status(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(&repo.root)
            .args(["branch", "-D", branch]),
    )
}

pub(crate) fn branch_oid(
    repo: &Repository,
    config: &Config,
    branch: &str,
) -> Result<String, String> {
    let oid = run_capture(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(&repo.root)
            .args(["rev-parse", "--verify", &format!("refs/heads/{branch}")]),
    )?;
    let oid = oid.trim();
    if oid.is_empty() {
        Err(format!("branch {branch} identity was empty; retained it"))
    } else {
        Ok(oid.to_string())
    }
}

pub(crate) fn remove_worktree(
    repo: &Repository,
    config: &Config,
    path: &Path,
) -> Result<(), String> {
    let remove_result = run_status(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(&repo.root)
            .args(["worktree", "remove", "--force"])
            .arg(path),
    );
    match remove_result {
        Ok(()) => Ok(()),
        Err(error) => {
            if !path.exists() {
                return prune_worktrees(repo, config).map_err(|prune_error| {
                    format!("{error}; also failed to prune worktrees: {prune_error}")
                });
            }
            recover_deregistered_worktree_remove_failure(repo, config, path, error)
        }
    }
}

fn recover_deregistered_worktree_remove_failure(
    repo: &Repository,
    config: &Config,
    path: &Path,
    error: String,
) -> Result<(), String> {
    if worktree_path_registered(repo, config, path).map_err(|list_error| {
        format!("{error}; also failed to inspect registered worktrees: {list_error}")
    })? {
        return Err(error);
    }
    std::fs::remove_dir_all(path).map_err(|remove_error| {
        format!(
            "{error}; worktree was deregistered but failed to delete {}: {remove_error}",
            path.display()
        )
    })?;
    prune_worktrees(repo, config)
        .map_err(|prune_error| format!("{error}; also failed to prune worktrees: {prune_error}"))
}

fn worktree_path_registered(
    repo: &Repository,
    config: &Config,
    path: &Path,
) -> Result<bool, String> {
    let output = run_capture(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(&repo.root)
            .args(["worktree", "list", "--porcelain"]),
    )?;
    Ok(output.lines().any(|line| {
        line.strip_prefix("worktree ")
            .is_some_and(|current| worktree_paths_match(Path::new(current), path))
    }))
}

fn worktree_paths_match(registered: &Path, selected: &Path) -> bool {
    if registered == selected {
        return true;
    }
    matches!(
        (registered.canonicalize(), selected.canonicalize()),
        (Ok(registered), Ok(selected)) if registered == selected
    )
}

pub(crate) fn prune_worktrees(repo: &Repository, config: &Config) -> Result<(), String> {
    run_status(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(&repo.root)
            .args(["worktree", "prune"]),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        WorktrunkApprovalStatus, check_worktrunk_approval_status, create_worktree_args,
        is_worktrunk_approval_failure, move_branch_to_worktree_args, remove_worktree,
        switch_checkout_args,
    };
    use crate::config::Config;
    use crate::observability;
    use crate::repo::Repository;
    use crate::test_support::write_executable;
    use rusqlite::params;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
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

        crate::session::create_worktree_session(&repo, &config, "feature").unwrap();

        let hidden = count_rows(&repo, "hidden_session", "feature");
        assert_eq!(hidden, 0);

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn create_worktree_session_restores_existing_hidden_worktree_without_creating() {
        let temp = unique_temp_dir("prism-create-restores-hidden-test");
        fs::create_dir_all(&temp).unwrap();
        let git = temp.join("git");
        write_executable(
            &git,
            "#!/bin/sh\nprintf 'worktree /repo/prism.feature\\nHEAD abc\\nbranch refs/heads/feature\\n\\n'\n",
        );
        let wt = temp.join("wt");
        write_executable(&wt, "#!/bin/sh\nexit 99\n");

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

        crate::session::create_worktree_session(&repo, &config, "feature").unwrap();

        assert_eq!(count_rows(&repo, "hidden_session", "feature"), 0);

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn phase_1_restore_by_create_clears_hidden_and_archived_state() {
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
            conn.execute(
                "insert into archived_worktree (
                    branch, repo_root, worktree_path, archived_unix_ms, classification
                 ) values (?1, ?2, ?3, ?4, ?5)",
                params![
                    "feature",
                    "/repo/prism",
                    "/repo/prism.feature",
                    123_i64,
                    "work"
                ],
            )
            .unwrap();
            Ok(())
        })
        .unwrap();

        crate::session::create_worktree_session(&repo, &config, "feature").unwrap();

        assert_eq!(count_rows(&repo, "hidden_session", "feature"), 0);
        assert_eq!(count_rows(&repo, "archived_worktree", "feature"), 0);

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn detects_worktrunk_approval_failure() {
        let output = "mock-repo needs approval before running commands:\ncannot prompt for approval in non-interactive environment";

        assert!(is_worktrunk_approval_failure(output));
        assert!(!is_worktrunk_approval_failure(
            "All commands already approved"
        ));
        assert!(!is_worktrunk_approval_failure(
            "mock-repo cannot prompt in non-interactive mode before it needs approval"
        ));
    }

    #[test]
    fn check_worktrunk_approval_status_reports_pending() {
        let temp = unique_temp_dir("prism-wt-approval-status-test");
        fs::create_dir_all(&temp).unwrap();
        let wt = temp.join("wt");
        write_executable(
            &wt,
            "#!/bin/sh\nprintf '%s\\n' 'repo needs approval to execute 1 command:' >&2\nprintf '%s\\n' 'Cannot prompt for approval in non-interactive environment' >&2\nexit 1\n",
        );

        let mut config = test_config();
        config
            .tools
            .insert("wt".to_string(), wt.display().to_string());
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));

        let status = check_worktrunk_approval_status(&repo, &config).unwrap();

        assert_eq!(status, WorktrunkApprovalStatus::Pending);

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn create_worktree_session_adds_worktrunk_approval_hint() {
        let temp = unique_temp_dir("prism-create-wt-approval-hint-test");
        fs::create_dir_all(&temp).unwrap();
        let wt = temp.join("wt");
        write_executable(
            &wt,
            "#!/bin/sh\nprintf '%s\\n' 'repo needs approval to execute 1 command:' >&2\nprintf '%s\\n' 'Cannot prompt for approval in non-interactive environment' >&2\nexit 1\n",
        );

        let mut config = test_config();
        config
            .tools
            .insert("wt".to_string(), wt.display().to_string());
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));

        let error = crate::session::create_worktree_session(&repo, &config, "feature").unwrap_err();

        assert!(error.contains("repo needs approval to execute 1 command"));
        assert!(error.contains("Cannot prompt for approval in non-interactive environment"));
        assert!(error.contains("Worktrunk project commands"));
        assert!(error.contains("config approvals add"));

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
    fn remove_worktree_does_not_recover_when_equivalent_path_is_registered() {
        let temp = unique_temp_dir("prism-remove-worktree-registered-path-test");
        fs::create_dir_all(&temp).unwrap();
        let actual_path = temp.join("worktree");
        let alternate_parent = temp.join("alternate-parent");
        fs::create_dir_all(&actual_path).unwrap();
        fs::create_dir_all(&alternate_parent).unwrap();
        fs::write(actual_path.join("leftover.txt"), "leftover\n").unwrap();
        let selected_path = alternate_parent.join("..").join("worktree");
        let log = temp.join("git.log");
        let git = temp.join("git");
        write_executable(
            &git,
            &format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$*" in
  *"worktree remove --force"*)
    echo "failed to delete '{}': Directory not empty" >&2
    exit 1
    ;;
  *"worktree list --porcelain"*)
    printf 'worktree {}\nbranch refs/heads/feature/delete\n\n'
    exit 0
    ;;
  *"worktree prune"*)
    exit 0
    ;;
esac
exit 0
"#,
                log.display(),
                selected_path.display(),
                actual_path.display()
            ),
        );

        let mut config = test_config();
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));

        let error = remove_worktree(&repo, &config, &selected_path).unwrap_err();

        assert!(error.contains("Directory not empty"));
        assert!(actual_path.exists());
        let commands = fs::read_to_string(&log).unwrap();
        assert!(commands.contains("worktree list --porcelain"));
        assert!(!commands.contains("worktree prune"));

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn complete_delete_removes_all_owned_state_and_preserves_auto_flow_audit() {
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
case "$*" in
  *"rev-parse --verify refs/heads/feature/delete"*) echo branch-oid ;;
esac
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
                "insert into task_metadata (
                    branch, prompt_summary, initial_prompt, worktree, updated_unix_ms
                 ) values (?1, 'summary', 'prompt', ?2, 123)",
                params![branch, path.display().to_string()],
            )
            .unwrap();
            conn.execute(
                "insert into hidden_session (branch, hidden_unix_ms) values (?1, 123)",
                params![branch],
            )
            .unwrap();
            conn.execute(
                "insert into archived_worktree (
                    branch, repo_root, worktree_path, archived_unix_ms, classification
                 ) values (?1, ?2, ?3, 123, 'work')",
                params![
                    branch,
                    repo.root.display().to_string(),
                    path.display().to_string()
                ],
            )
            .unwrap();
            conn.execute(
                "insert into agent_state (branch, state, updated_unix_ms)
                 values (?1, 'running', 123)",
                params![branch],
            )
            .unwrap();
            conn.execute(
                "insert into pr_cache (
                    branch, number, title, url, state, review_decision, head_ref, base_ref,
                    head_sha, updated_at, check_status, merged, draft, last_refreshed,
                    refreshed_unix_ms
                 ) values (?1, 42, 'Delete me', 'https://example.test/pull/42', 'OPEN', '',
                           ?1, 'main', 'abc123', '', 'pending', 0, 0, '', 123)",
                params![branch],
            )
            .unwrap();
            conn.execute(
                "insert into pr_details_cache (
                    branch, comments, reviews, review_comments, files, failing_checks,
                    refreshed_unix_ms
                 ) values (?1, '[]', '[]', '[]', '[]', '[]', 123)",
                params![branch],
            )
            .unwrap();
            conn.execute(
                "insert into opencode_runtime (
                    repo_root, branch, worktree_path, server_port, server_url,
                    opencode_session_id, generation, updated_unix_ms
                 ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    repo.root.display().to_string(),
                    branch,
                    path.display().to_string(),
                    41000_i64,
                    "http://127.0.0.1:41000",
                    "ses_delete",
                    1_i64,
                    123_i64,
                ],
            )
            .unwrap();
            Ok(())
        })
        .unwrap();
        let agent_log = repo
            .prism_dir()
            .join("logs")
            .join(format!("{}.log", crate::util::safe_branch_filename(branch)));
        fs::create_dir_all(agent_log.parent().unwrap()).unwrap();
        fs::write(&agent_log, "owned Agent Session log\n").unwrap();
        let mut audit =
            crate::auto_flow::AutoLaunch::new(&repo.root, &path, branch, "preserve deletion audit")
                .unwrap()
                .create_run();
        observability::with_writable_db(&repo, |conn| {
            crate::auto_flow::save_auto_run(conn, &mut audit)
        })
        .unwrap();

        crate::session::delete_worktree_session_if_current(&repo, &config, &path, branch, None)
            .unwrap();

        let tmux_commands = fs::read_to_string(&tmux_log).unwrap();
        assert!(tmux_commands.contains("list-sessions -F #{session_name}"));
        assert!(tmux_commands.contains(&format!("kill-session -t {}", runtime.name())));
        assert!(!tmux_commands.contains(&format!("kill-session -t {}", other_runtime.name())));
        let git_commands = fs::read_to_string(&git_log).unwrap();
        assert!(git_commands.contains("worktree remove --force"));
        assert!(git_commands.contains("branch -D feature/delete"));
        for table in [
            "task_metadata",
            "hidden_session",
            "archived_worktree",
            "agent_state",
            "pr_cache",
            "pr_details_cache",
            "opencode_runtime",
        ] {
            assert_eq!(
                count_rows(&repo, table, branch),
                0,
                "retained row in {table}"
            );
        }
        assert!(!agent_log.exists());
        let audit_preserved = observability::with_writable_db(&repo, |conn| {
            crate::auto_flow::load_auto_run(conn, &audit.run.id)
        })
        .unwrap();
        assert!(audit_preserved.is_some());

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn delete_worktree_session_recovers_after_deregistered_remove_failure() {
        let temp = unique_temp_dir("prism-delete-deregistered-failure-test");
        fs::create_dir_all(&temp).unwrap();
        let tmux_log = temp.join("tmux.log");
        let git_log = temp.join("git.log");
        let tmux = temp.join("tmux");
        let git = temp.join("git");
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        let branch = "feature/delete";
        let stale_branch = "feature/old-delete";
        let path = temp.join("worktree");
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("leftover.txt"), "leftover\n").unwrap();
        fs::write(
            &tmux,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$1" in
  list-sessions)
    exit 0
    ;;
  kill-session)
    exit 0
    ;;
esac
exit 1
"#,
                tmux_log.display()
            ),
        )
        .unwrap();
        fs::write(
            &git,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
case "$*" in
  *"rev-parse --verify refs/heads/feature/delete"*)
    echo branch-oid
    exit 0
    ;;
  *"worktree remove --force"*)
    echo "failed to delete '{}': Directory not empty" >&2
    exit 1
    ;;
  *"worktree list --porcelain"*)
    exit 0
    ;;
  *"worktree prune"*)
    exit 0
    ;;
  *"branch -D feature/delete"*)
    exit 0
    ;;
esac
exit 0
"#,
                git_log.display(),
                path.display()
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
        observability::with_writable_db(&repo, |conn| {
            conn.execute(
                "insert into task_metadata (
                    branch, prompt_summary, initial_prompt, worktree, updated_unix_ms
                 ) values (?1, ?2, ?3, ?4, ?5)",
                params![
                    branch,
                    "summary",
                    "prompt",
                    path.display().to_string(),
                    123_i64
                ],
            )
            .unwrap();
            for runtime_branch in [branch, stale_branch] {
                conn.execute(
                    "insert into opencode_runtime (
                        repo_root, branch, worktree_path, server_port, server_url,
                        generation, updated_unix_ms
                     ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        repo.root.display().to_string(),
                        runtime_branch,
                        path.display().to_string(),
                        41000_i64,
                        "http://127.0.0.1:41000",
                        1_i64,
                        123_i64,
                    ],
                )
                .unwrap();
            }
            Ok(())
        })
        .unwrap();
        let log = repo
            .prism_dir()
            .join("logs")
            .join(format!("{}.log", crate::util::safe_branch_filename(branch)));
        fs::create_dir_all(log.parent().unwrap()).unwrap();
        fs::write(&log, "agent log\n").unwrap();

        crate::session::delete_worktree_session_if_current(&repo, &config, &path, branch, None)
            .unwrap();

        let git_commands = fs::read_to_string(&git_log).unwrap();
        assert!(git_commands.contains("worktree remove --force"));
        assert!(git_commands.contains("worktree list --porcelain"));
        assert!(git_commands.contains("worktree prune"));
        assert!(git_commands.contains("branch -D feature/delete"));
        assert!(!path.exists());
        assert_eq!(count_rows(&repo, "task_metadata", branch), 0);
        assert_eq!(count_rows(&repo, "opencode_runtime", branch), 0);
        assert_eq!(
            count_rows(&repo, "opencode_runtime", stale_branch),
            1,
            "old-branch cleanup must not delete another branch's runtime at the same path"
        );
        assert!(!log.exists());

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn phase_1_failed_git_worktree_removal_preserves_prism_state_and_artifacts() {
        let temp = unique_temp_dir("prism-phase-1-preserve-failed-delete-test");
        fs::create_dir_all(&temp).unwrap();
        let tmux = temp.join("tmux");
        write_executable(&tmux, "#!/bin/sh\nexit 0\n");
        let git = temp.join("git");
        let branch = "feature/preserve";
        let path = temp.join("worktree");
        fs::create_dir_all(&path).unwrap();
        let worktree_artifact = path.join("uncommitted.txt");
        fs::write(&worktree_artifact, "local work\n").unwrap();
        write_executable(
            &git,
            &format!(
                r#"#!/bin/sh
case "$*" in
  *"rev-parse --verify refs/heads/feature/preserve"*)
    echo branch-oid
    exit 0
    ;;
  *"worktree remove --force"*)
    echo "failed to remove registered worktree" >&2
    exit 1
    ;;
  *"worktree list --porcelain"*)
    printf 'worktree {}\nbranch refs/heads/{}\n\n'
    exit 0
    ;;
esac
exit 0
"#,
                path.display(),
                branch
            ),
        );

        let mut config = test_config();
        config
            .tools
            .insert("tmux".to_string(), tmux.display().to_string());
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        observability::with_writable_db(&repo, |conn| {
            conn.execute(
                "insert into task_metadata (
                    branch, prompt_summary, initial_prompt, worktree, updated_unix_ms
                 ) values (?1, ?2, ?3, ?4, ?5)",
                params![
                    branch,
                    "summary",
                    "prompt",
                    path.display().to_string(),
                    123_i64
                ],
            )
            .unwrap();
            conn.execute(
                "insert into hidden_session (branch, hidden_unix_ms) values (?1, ?2)",
                params![branch, 123_i64],
            )
            .unwrap();
            conn.execute(
                "insert into archived_worktree (
                    branch, repo_root, worktree_path, archived_unix_ms, classification
                 ) values (?1, ?2, ?3, ?4, ?5)",
                params![
                    branch,
                    repo.root.display().to_string(),
                    path.display().to_string(),
                    123_i64,
                    "work"
                ],
            )
            .unwrap();
            conn.execute(
                "insert into agent_state (branch, state, updated_unix_ms) values (?1, ?2, ?3)",
                params![branch, "running", 123_i64],
            )
            .unwrap();
            conn.execute(
                "insert into pr_cache (
                    branch, number, title, url, state, review_decision, head_ref, base_ref,
                    head_sha, updated_at, check_status, merged, draft, last_refreshed,
                    refreshed_unix_ms
                 ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                params![
                    branch,
                    42_i64,
                    "Preserve state",
                    "https://example.test/pull/42",
                    "OPEN",
                    "",
                    branch,
                    "main",
                    "abc123",
                    "2026-01-01T00:00:00Z",
                    "pending",
                    false,
                    false,
                    "2026-01-01T00:00:00Z",
                    123_i64
                ],
            )
            .unwrap();
            conn.execute(
                "insert into pr_details_cache (
                    branch, comments, reviews, review_comments, files, failing_checks,
                    refreshed_unix_ms
                 ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![branch, "[]", "[]", "[]", "[]", "[]", 123_i64],
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
                    path.display().to_string(),
                    41000_i64,
                    "http://127.0.0.1:41000",
                    1_i64,
                    123_i64
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

        let error =
            crate::session::delete_worktree_session_if_current(&repo, &config, &path, branch, None)
                .unwrap_err();

        assert!(error.contains("failed to remove registered worktree"));
        for table in [
            "task_metadata",
            "hidden_session",
            "archived_worktree",
            "agent_state",
            "pr_cache",
            "pr_details_cache",
            "opencode_runtime",
        ] {
            assert_eq!(count_rows(&repo, table, branch), 1, "lost row from {table}");
        }
        assert!(log.exists(), "lost Prism-owned agent log");
        assert!(worktree_artifact.exists(), "lost worktree artifact");

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn branch_delete_failure_reports_removed_worktree_and_retained_branch() {
        let temp = unique_temp_dir("prism-delete-branch-retained-test");
        fs::create_dir_all(&temp).unwrap();
        let tmux = temp.join("tmux");
        write_executable(&tmux, "#!/bin/sh\nexit 0\n");
        let git = temp.join("git");
        write_executable(
            &git,
            "#!/bin/sh\ncase \"$*\" in\n  *\"rev-parse --verify refs/heads/feature/keep\"*) echo branch-oid; exit 0 ;;\n  *\"branch -D feature/keep\"*) echo retained >&2; exit 1 ;;\n  *\"worktree list --porcelain\"*) exit 0 ;;\nesac\nexit 0\n",
        );
        let mut config = test_config();
        config
            .tools
            .insert("tmux".to_string(), tmux.display().to_string());
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        let path = temp.join("worktree");
        fs::create_dir_all(&path).unwrap();
        observability::with_writable_db(&repo, |conn| {
            conn.execute(
                "insert into task_metadata (
                    branch, prompt_summary, initial_prompt, worktree, updated_unix_ms
                 ) values ('feature/keep', '', '', ?1, 0)",
                params![path.display().to_string()],
            )
            .map_err(|error| error.to_string())?;
            Ok(())
        })
        .unwrap();

        let outcome = crate::session::delete_worktree_session_if_current(
            &repo,
            &config,
            &path,
            "feature/keep",
            None,
        )
        .unwrap();

        assert!(matches!(
            outcome,
            crate::session::DeleteWorktreeOutcome::BranchRetained { .. }
        ));
        assert_eq!(count_rows(&repo, "task_metadata", "feature/keep"), 0);
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn branch_identity_failure_stops_before_worktree_removal() {
        let temp = unique_temp_dir("prism-delete-branch-identity-failure-test");
        fs::create_dir_all(&temp).unwrap();
        let git_log = temp.join("git.log");
        let git = temp.join("git");
        write_executable(
            &git,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncase \"$*\" in\n  *\"rev-parse --verify\"*) exit 1 ;;\nesac\nexit 0\n",
                git_log.display()
            ),
        );
        let mut config = test_config();
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        let path = temp.join("worktree");
        fs::create_dir_all(&path).unwrap();
        observability::with_writable_db(&repo, |conn| {
            conn.execute(
                "insert into task_metadata (
                    branch, prompt_summary, initial_prompt, worktree, updated_unix_ms
                 ) values ('feature/keep', '', '', ?1, 0)",
                params![path.display().to_string()],
            )
            .map_err(|error| error.to_string())?;
            Ok(())
        })
        .unwrap();

        let error = crate::session::delete_worktree_session_if_current(
            &repo,
            &config,
            &path,
            "feature/keep",
            None,
        )
        .unwrap_err();

        assert!(error.contains("rev-parse"));
        assert!(
            !fs::read_to_string(&git_log)
                .unwrap()
                .contains("worktree remove")
        );
        assert_eq!(count_rows(&repo, "task_metadata", "feature/keep"), 1);
        let _ = fs::remove_dir_all(temp);
    }

    fn test_config() -> Config {
        crate::test_support::test_config()
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
