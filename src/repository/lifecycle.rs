use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::Config;
use crate::process::{
    ProcessDescriptor, ProcessPolicy, run_capture, run_capture_named, run_output_allow_failure,
    run_status,
};
use crate::repo::Repository;
pub(crate) use crate::worktrunk::ApprovalStatus as WorktrunkApprovalStatus;

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
        ProcessPolicy::Metadata,
    )?;
    let mut inventory = parse_worktree_inventory(&output);
    for entry in &mut inventory {
        if entry.branch == "(detached)"
            && let Some(branch) = rebase_branch(&entry.path, config)
        {
            entry.branch = branch;
        }
    }
    Ok(inventory)
}

fn rebase_branch(path: &Path, config: &Config) -> Option<String> {
    for head_name in ["rebase-merge/head-name", "rebase-apply/head-name"] {
        let Ok(output) = run_capture(
            Command::new(config.tool("git")).arg("-C").arg(path).args([
                "rev-parse",
                "--git-path",
                head_name,
            ]),
            ProcessPolicy::Metadata,
        ) else {
            continue;
        };
        let Some(git_path) = single_git_line(&output) else {
            continue;
        };
        let git_path = PathBuf::from(git_path);
        let git_path = if git_path.is_absolute() {
            git_path
        } else {
            path.join(git_path)
        };
        let Ok(first_read) = std::fs::read_to_string(&git_path) else {
            continue;
        };
        let Some(full_ref) = single_git_line(&first_read) else {
            continue;
        };
        let Some(branch) = full_ref.strip_prefix("refs/heads/") else {
            continue;
        };
        if branch.is_empty()
            || branch.starts_with('-')
            || !git_succeeds(path, config, &["check-ref-format", full_ref])
            || !git_succeeds(path, config, &["show-ref", "--verify", "--quiet", full_ref])
            || git_exit_code(path, config, &["symbolic-ref", "--quiet", "HEAD"]) != Some(1)
            || std::fs::read_to_string(git_path).ok().as_deref() != Some(first_read.as_str())
        {
            continue;
        }
        return Some(branch.to_string());
    }
    None
}

fn single_git_line(output: &str) -> Option<&str> {
    let output = output.strip_suffix('\n').unwrap_or(output);
    let output = output.strip_suffix('\r').unwrap_or(output);
    (!output.is_empty() && !output.contains(['\n', '\r'])).then_some(output)
}

fn git_succeeds(path: &Path, config: &Config, args: &[&str]) -> bool {
    git_exit_code(path, config, args) == Some(0)
}

fn git_exit_code(path: &Path, config: &Config, args: &[&str]) -> Option<i32> {
    run_output_allow_failure(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(path)
            .args(args),
        ProcessPolicy::Metadata,
    )
    .ok()?
    .status
    .code()
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

pub(crate) fn create_worktree(
    repo: &Repository,
    config: &Config,
    branch: &str,
) -> Result<crate::worktrunk::SwitchOutcome, crate::worktrunk::WorktrunkFailure> {
    crate::worktrunk::switch_worktree(crate::worktrunk::SwitchRequest {
        repo,
        config,
        branch,
        create: true,
        base: config.default_base.as_deref(),
    })
}

pub(crate) fn checkout_worktree(
    repo: &Repository,
    config: &Config,
    branch: &str,
) -> Result<crate::worktrunk::SwitchOutcome, crate::worktrunk::WorktrunkFailure> {
    crate::worktrunk::switch_worktree(crate::worktrunk::SwitchRequest {
        repo,
        config,
        branch,
        create: false,
        base: None,
    })
}

pub(crate) fn verify_switch_outcome(
    repo: &Repository,
    config: &Config,
    requested_branch: &str,
    outcome: &crate::worktrunk::SwitchOutcome,
) -> Result<(), String> {
    if outcome.branch != requested_branch {
        return Err(format!(
            "Worktrunk returned branch {:?} for requested branch {requested_branch:?}",
            outcome.branch
        ));
    }
    if list_worktrees(repo, config)?.into_iter().any(|entry| {
        crate::worktrunk::paths_equivalent(&entry.path, &outcome.path)
            && entry.branch == outcome.branch
    }) {
        Ok(())
    } else {
        Err(format!(
            "Worktrunk reported {} for branch {}, but fresh Git worktree inventory did not contain that exact path and branch",
            outcome.path.display(),
            outcome.branch
        ))
    }
}

pub(crate) fn check_worktrunk_approval_status(
    repo: &Repository,
    config: &Config,
) -> Result<WorktrunkApprovalStatus, String> {
    crate::worktrunk::approval_status(repo, config).map_err(|error| error.to_string())
}

pub(crate) fn run_worktrunk_approval_prompt(
    repo: &Repository,
    config: &Config,
) -> Result<(), String> {
    crate::worktrunk::run_approval_prompt(repo, config)
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
        ProcessPolicy::Metadata,
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
    run_status(
        Command::new(config.tool("git")).args(switch_checkout_args(&repo.root, base)),
        ProcessPolicy::LocalMutation,
    )?;
    let outcome = crate::worktrunk::switch_worktree(crate::worktrunk::SwitchRequest {
        repo,
        config,
        branch,
        create: false,
        base: None,
    })
    .map_err(|failure| failure.to_string())?;
    verify_switch_outcome(repo, config, branch, &outcome)?;
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
        vec!["push", "-u", "origin", branch]
    } else {
        vec!["push"]
    };
    run_capture_named(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(path)
            .args(args),
        ProcessPolicy::NetworkQuery,
        ProcessDescriptor::new("git.push"),
    )?;
    Ok(())
}

fn switch_checkout_args(repo_root: &Path, branch: &str) -> Vec<String> {
    vec![
        "-C".to_string(),
        repo_root.display().to_string(),
        "switch".to_string(),
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
            .args(["branch", "-D", "--", branch]),
        ProcessPolicy::LocalMutation,
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
        ProcessPolicy::Metadata,
    )?;
    let oid = oid.trim();
    if oid.is_empty() {
        Err(format!("branch {branch} identity was empty; retained it"))
    } else {
        Ok(oid.to_string())
    }
}

pub(crate) fn branch_exists(
    repo: &Repository,
    config: &Config,
    branch: &str,
) -> Result<bool, String> {
    let output = run_output_allow_failure(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(&repo.root)
            .args([
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ]),
        ProcessPolicy::Metadata,
    )?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(format!(
            "could not determine whether branch {branch} exists"
        )),
    }
}

pub(crate) fn remove_worktree(
    repo: &Repository,
    config: &Config,
    path: &Path,
) -> Result<crate::worktrunk::RemoveOutcome, crate::worktrunk::WorktrunkFailure> {
    crate::worktrunk::remove_worktree(crate::worktrunk::RemoveRequest { repo, config, path })
}

pub(crate) fn prune_worktrees(repo: &Repository, config: &Config) -> Result<(), String> {
    run_status(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(&repo.root)
            .args(["worktree", "prune"]),
        ProcessPolicy::LocalMutation,
    )
}

#[cfg(test)]
mod tests {
    use super::{WorktrunkApprovalStatus, check_worktrunk_approval_status, switch_checkout_args};
    use crate::config::Config;
    use crate::observability;
    use crate::persistence::database::TestDatabase;
    use crate::repo::Repository;
    use crate::sqlx_test_params as params;
    use crate::test_support::write_executable;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn list_worktrees_recovers_branch_during_conflicted_rebase() {
        let temp = unique_temp_dir("prism-rebase-worktree-branch-test");
        let repo_root = temp.join("repo");
        let worktree = temp.join("feature-worktree");
        fs::create_dir_all(&repo_root).unwrap();
        run_git(&repo_root, &["init", "-b", "main"]);
        run_git(&repo_root, &["config", "user.email", "test@example.com"]);
        run_git(&repo_root, &["config", "user.name", "Prism Test"]);
        fs::write(repo_root.join("shared.txt"), "base\n").unwrap();
        run_git(&repo_root, &["add", "shared.txt"]);
        run_git(&repo_root, &["commit", "-m", "base"]);
        run_git(&repo_root, &["branch", "feature"]);
        fs::write(repo_root.join("shared.txt"), "main\n").unwrap();
        run_git(&repo_root, &["commit", "-am", "main"]);
        run_git(
            &repo_root,
            &["worktree", "add", worktree.to_str().unwrap(), "feature"],
        );
        fs::write(worktree.join("shared.txt"), "feature\n").unwrap();
        run_git(&worktree, &["commit", "-am", "feature"]);

        let rebase = Command::new("git")
            .arg("-C")
            .arg(&worktree)
            .args(["rebase", "main"])
            .status()
            .unwrap();
        assert!(!rebase.success(), "fixture rebase must pause on a conflict");

        let repo = Repository::with_config_dir_for_test(repo_root, temp.join("config"));
        let mut config = test_config();
        config.tools.insert("git".to_string(), "git".to_string());
        let inventory = super::list_worktrees(&repo, &config).unwrap();
        let feature_worktree_path = inventory
            .iter()
            .find(|entry| entry.branch == "feature")
            .and_then(|entry| fs::canonicalize(&entry.path).ok());
        let expected_worktree_path = fs::canonicalize(&worktree);
        assert!(expected_worktree_path.is_ok());
        assert_eq!(feature_worktree_path, expected_worktree_path.ok());

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn rebase_branch_rejects_invalid_or_stale_head_name() {
        let temp = unique_temp_dir("prism-invalid-rebase-branch-test");
        let head_name = temp.join("head-name");
        fs::create_dir_all(&temp).unwrap();
        let git = temp.join("git");
        write_executable(
            &git,
            &format!(
                "#!/bin/sh\ncase \"$*\" in\n  *\"rev-parse --git-path rebase-merge/head-name\"*) printf '{}\\n' ;;\n  *\"symbolic-ref --quiet HEAD\"*) exit 1 ;;\n  *) exit 0 ;;\nesac\n",
                head_name.display()
            ),
        );
        fs::write(&head_name, "refs/heads/--unsafe\n").unwrap();
        let mut config = test_config();
        config
            .tools
            .insert("git".to_string(), git.display().to_string());

        assert_eq!(super::rebase_branch(&temp, &config), None);

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn create_worktree_session_clears_stale_hidden_marker() {
        let temp = unique_temp_dir("prism-create-clears-hidden-test");
        fs::create_dir_all(&temp).unwrap();
        let wt = temp.join("wt");
        fs::write(
            &wt,
            "#!/bin/sh\nprintf '%s' '{\"action\":\"created\",\"branch\":\"feature\",\"path\":\"/repo/prism.feature\",\"created_branch\":true}'\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&wt).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&wt, permissions).unwrap();
        let git = temp.join("git");
        fs::write(
            &git,
            "#!/bin/sh\nprintf 'worktree /repo/prism.feature\\nHEAD abc\\nbranch refs/heads/feature\\n\\n'\n",
        )
        .unwrap();
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
        with_test_database(&repo, |conn| {
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
    fn create_worktree_session_preserves_metadata_when_switch_result_is_not_in_git_inventory() {
        let temp = unique_temp_dir("prism-create-verification-failure-test");
        fs::create_dir_all(&temp).unwrap();
        let wt = temp.join("wt");
        write_executable(
            &wt,
            "#!/bin/sh\nprintf '%s' '{\"action\":\"created\",\"branch\":\"feature\",\"path\":\"/repo/reported\",\"created_branch\":true}'\n",
        );
        let git = temp.join("git");
        write_executable(
            &git,
            "#!/bin/sh\nprintf 'worktree /repo/actual\\nbranch refs/heads/feature\\n\\n'\n",
        );
        let mut config = test_config();
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        config
            .tools
            .insert("wt".to_string(), wt.display().to_string());
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        with_test_database(&repo, |conn| {
            conn.execute(
                "insert into archived_worktree (
                    branch, repo_root, worktree_path, archived_unix_ms, classification
                 ) values ('feature', '/repo', '/repo/old', 123, 'work')",
                [],
            )
            .map_err(|error| error.to_string())?;
            Ok(())
        })
        .unwrap();

        let outcome = crate::session::create_worktree_session(&repo, &config, "feature").unwrap();

        assert!(matches!(
            outcome,
            crate::session::CreateWorktreeOutcome::CreatedMetadataFailed { .. }
        ));
        assert_eq!(count_rows(&repo, "archived_worktree", "feature"), 1);
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
        with_test_database(&repo, |conn| {
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
        with_test_database(&repo, |conn| {
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

        let error = crate::session::create_worktree_session(&repo, &config, "feature")
            .unwrap_err()
            .to_string();

        assert!(error.contains("repo needs approval to execute 1 command"));
        assert!(error.contains("Cannot prompt for approval in non-interactive environment"));
        assert!(error.contains("Worktrunk project commands"));
        assert!(error.contains("config approvals add"));

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn move_current_branch_args_switches_checkout_first() {
        let repo = PathBuf::from("/repo/prism");

        assert_eq!(
            switch_checkout_args(&repo, "main"),
            vec!["-C", "/repo/prism", "switch", "main"]
        );
    }

    #[test]
    fn move_current_branch_uses_typed_worktrunk_switch() {
        let temp = unique_temp_dir("prism-move-current-branch-test");
        fs::create_dir_all(&temp).unwrap();
        let git_log = temp.join("git.log");
        let wt_log = temp.join("wt.log");
        let git = temp.join("git");
        let wt = temp.join("wt");
        write_executable(
            &git,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\ncase \"$*\" in\n  *\"worktree list --porcelain\"*) printf 'worktree /repo/prism.feat-test\\nbranch refs/heads/feat/test\\n\\n' ;;\nesac\n",
                git_log.display()
            ),
        );
        write_executable(
            &wt,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf '%s' '{{\"action\":\"existing\",\"branch\":\"feat/test\",\"path\":\"/repo/prism.feat-test\"}}'\n",
                wt_log.display()
            ),
        );
        let mut config = test_config();
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        config
            .tools
            .insert("wt".to_string(), wt.display().to_string());
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));

        super::move_current_branch_to_worktree(&repo, &config, "feat/test", "main").unwrap();

        assert!(
            fs::read_to_string(git_log)
                .unwrap()
                .contains("switch\nmain")
        );
        let wt_args = fs::read_to_string(wt_log).unwrap();
        assert!(wt_args.contains("switch\n--no-cd\n--format=json\nfeat/test"));
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn complete_delete_removes_all_owned_session_state() {
        let temp = unique_temp_dir("prism-delete-kills-tmux-test");
        fs::create_dir_all(&temp).unwrap();
        let tmux_log = temp.join("tmux.log");
        let git_log = temp.join("git.log");
        let wt_log = temp.join("wt.log");
        let tmux = temp.join("tmux");
        let git = temp.join("git");
        let wt = temp.join("wt");
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
        write_successful_remove_wt(&wt, &wt_log, &path, branch);
        config
            .tools
            .insert("wt".to_string(), wt.display().to_string());
        with_test_database(&repo, |conn| {
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
                    branch, number, provider, canonical_host, project_path, native_cr_id,
                    display_number, source_provider, source_canonical_host, source_project_path,
                    target_provider, target_canonical_host, target_project_path,
                    title, url, state, review_decision, head_ref, base_ref,
                    head_sha, updated_at, check_status, merged, draft, last_refreshed,
                    refreshed_unix_ms
                 ) values (?1, 42, 'github', 'github.com', 'org/repo', '42', 42,
                           'github', 'github.com', 'org/repo', 'github', 'github.com', 'org/repo',
                           'Delete me', 'https://example.test/pull/42', 'OPEN', '',
                           ?1, 'main', 'abc123', '', 'pending', 0, 0, '', 123)",
                params![branch],
            )
            .unwrap();
            conn.execute(
                "insert into pr_details_cache (
                    branch, pr_number, head_sha, provider, canonical_host, project_path,
                    native_cr_id, display_number, source_provider, source_canonical_host,
                    source_project_path, target_provider, target_canonical_host, target_project_path,
                    comments, reviews, review_comments, files, failing_checks,
                    refreshed_unix_ms
                 ) values (?1, 42, 'abc123', 'github', 'github.com', 'org/repo', '42', 42,
                           'github', 'github.com', 'org/repo', 'github', 'github.com', 'org/repo',
                           '[]', '[]', '[]', '[]', '[]', 123)",
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
        crate::session::delete_worktree_session_if_current(&repo, &config, &path, branch, None)
            .unwrap();

        let tmux_commands = fs::read_to_string(&tmux_log).unwrap();
        assert!(tmux_commands.contains("list-sessions -F #{session_name}"));
        assert!(tmux_commands.contains(&format!("kill-session -t {}", runtime.name())));
        assert!(!tmux_commands.contains(&format!("kill-session -t {}", other_runtime.name())));
        let git_commands = fs::read_to_string(&git_log).unwrap();
        assert!(git_commands.contains("branch -D -- feature/delete"));
        let wt_commands = fs::read_to_string(&wt_log).unwrap();
        assert!(
            wt_commands.contains("remove --foreground --force --no-delete-branch --format=json --")
        );
        assert!(wt_commands.ends_with(&path.display().to_string()));
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
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn delete_worktree_session_cleans_only_the_removed_session_runtime() {
        let temp = unique_temp_dir("prism-delete-deregistered-failure-test");
        fs::create_dir_all(&temp).unwrap();
        let tmux_log = temp.join("tmux.log");
        let git_log = temp.join("git.log");
        let tmux = temp.join("tmux");
        let git = temp.join("git");
        let wt = temp.join("wt");
        let wt_log = temp.join("wt.log");
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
  *"branch -D -- feature/delete"*)
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
        write_successful_remove_wt(&wt, &wt_log, &path, branch);
        config
            .tools
            .insert("wt".to_string(), wt.display().to_string());
        with_test_database(&repo, |conn| {
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
        assert!(git_commands.contains("worktree list --porcelain"));
        assert!(git_commands.contains("branch -D -- feature/delete"));
        assert!(
            fs::read_to_string(&wt_log)
                .unwrap()
                .contains("--no-delete-branch")
        );
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
    fn failed_worktrunk_removal_preserves_prism_state_branch_and_artifacts() {
        let temp = unique_temp_dir("prism-phase-1-preserve-failed-delete-test");
        fs::create_dir_all(&temp).unwrap();
        let tmux = temp.join("tmux");
        write_executable(&tmux, "#!/bin/sh\nexit 0\n");
        let git = temp.join("git");
        let wt = temp.join("wt");
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
        write_executable(
            &wt,
            "#!/bin/sh\necho 'pre-remove hook failed' >&2\nexit 1\n",
        );

        let mut config = test_config();
        config
            .tools
            .insert("tmux".to_string(), tmux.display().to_string());
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        config
            .tools
            .insert("wt".to_string(), wt.display().to_string());
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        with_test_database(&repo, |conn| {
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
                    branch, number, provider, canonical_host, project_path, native_cr_id,
                    display_number, source_provider, source_canonical_host, source_project_path,
                    target_provider, target_canonical_host, target_project_path,
                    title, url, state, review_decision, head_ref, base_ref,
                    head_sha, updated_at, check_status, merged, draft, last_refreshed,
                    refreshed_unix_ms
                 ) values (?1, ?2, 'github', 'github.com', 'org/repo', '42', 42,
                           'github', 'github.com', 'org/repo', 'github', 'github.com', 'org/repo',
                           ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
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
                    branch, pr_number, head_sha, provider, canonical_host, project_path,
                    native_cr_id, display_number, source_provider, source_canonical_host,
                    source_project_path, target_provider, target_canonical_host, target_project_path,
                    comments, reviews, review_comments, files, failing_checks,
                    refreshed_unix_ms
                 ) values (?1, 42, 'abc123', 'github', 'github.com', 'org/repo', '42', 42,
                           'github', 'github.com', 'org/repo', 'github', 'github.com', 'org/repo',
                           ?2, ?3, ?4, ?5, ?6, ?7)",
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

        assert!(error.contains("pre-remove hook failed"));
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
    fn failed_worktrunk_removal_after_path_removal_is_resumable_without_repeating_remove() {
        let temp = unique_temp_dir("prism-wt-failed-after-remove-retry-test");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        let branch = "feature/retry-removed";
        let path = temp.join("worktree");
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join(".git"), "gitdir: /repo/.git/worktrees/retry\n").unwrap();
        let branch_deleted = temp.join("branch-deleted");
        let git = temp.join("git");
        write_executable(
            &git,
            &format!(
                r#"#!/bin/sh
case "$*" in
  *"worktree list --porcelain"*)
    test ! -d '{path}' || printf 'worktree {path}\nbranch refs/heads/{branch}\n\n'
    ;;
  *"rev-parse --verify refs/heads/{branch}"*) printf 'branch-oid\n' ;;
  *"show-ref --verify --quiet refs/heads/{branch}"*) test ! -e '{branch_deleted}' ;;
  *"branch -D -- {branch}"*) touch '{branch_deleted}' ;;
esac
exit 0
"#,
                path = path.display(),
                branch_deleted = branch_deleted.display(),
            ),
        );
        let wt_log = temp.join("wt.log");
        let wt = temp.join("wt");
        write_executable(
            &wt,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nrm -rf '{}'\necho 'post-remove hook failed' >&2\nexit 1\n",
                wt_log.display(),
                path.display(),
            ),
        );
        let tmux = temp.join("tmux");
        write_executable(&tmux, "#!/bin/sh\nexit 0\n");
        let mut config = test_config();
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        config
            .tools
            .insert("wt".to_string(), wt.display().to_string());
        config
            .tools
            .insert("tmux".to_string(), tmux.display().to_string());
        with_test_database(&repo, |conn| {
            conn.execute(
                "insert into task_metadata (
                    branch, prompt_summary, initial_prompt, worktree, updated_unix_ms
                 ) values (?1, 'summary', 'prompt', ?2, 123)",
                params![branch, path.display().to_string()],
            )
            .map_err(|error| error.to_string())?;
            Ok(())
        })
        .unwrap();

        let first =
            crate::session::delete_worktree_session_if_current(&repo, &config, &path, branch, None)
                .unwrap();

        assert!(matches!(
            first,
            crate::session::DeleteWorktreeOutcome::BranchRetained {
                owned_state_removed: false,
                ..
            }
        ));
        assert!(!path.exists());
        assert!(!branch_deleted.exists());
        assert_eq!(count_rows(&repo, "task_metadata", branch), 1);
        let first_wt_log = fs::read_to_string(&wt_log).unwrap();

        let retried =
            crate::session::delete_worktree_session_if_current(&repo, &config, &path, branch, None)
                .unwrap();

        assert_eq!(retried, crate::session::DeleteWorktreeOutcome::Deleted);
        assert!(branch_deleted.exists());
        assert_eq!(count_rows(&repo, "task_metadata", branch), 0);
        assert_eq!(fs::read_to_string(&wt_log).unwrap(), first_wt_log);
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn pending_removed_worktree_with_absent_branch_resumes_owned_state_cleanup() {
        let temp = unique_temp_dir("prism-pending-removed-absent-branch-test");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        let branch = "feature/crash-window";
        let path = temp.join("removed-worktree");
        let git_log = temp.join("git.log");
        let git = temp.join("git");
        write_executable(
            &git,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncase \"$*\" in\n  *\"show-ref --verify --quiet refs/heads/{branch}\"*) exit 1 ;;\nesac\nexit 0\n",
                git_log.display(),
            ),
        );
        let wt_called = temp.join("wt-called");
        let wt = temp.join("wt");
        write_executable(
            &wt,
            &format!("#!/bin/sh\ntouch '{}'\nexit 99\n", wt_called.display()),
        );
        let tmux = temp.join("tmux");
        write_executable(&tmux, "#!/bin/sh\nexit 0\n");
        let mut config = test_config();
        config
            .tools
            .insert("git".to_string(), git.display().to_string());
        config
            .tools
            .insert("wt".to_string(), wt.display().to_string());
        config
            .tools
            .insert("tmux".to_string(), tmux.display().to_string());
        with_test_database(&repo, |conn| {
            conn.execute(
                "insert into task_metadata (
                    branch, prompt_summary, initial_prompt, worktree, updated_unix_ms
                 ) values (?1, 'summary', 'prompt', ?2, 123)",
                params![branch, path.display().to_string()],
            )
            .map_err(|error| error.to_string())?;
            conn.execute(
                "insert into pending_worktree_deletion (
                    branch, worktree_path, worktree_incarnation, branch_oid,
                    worktree_removed, branch_deleted, updated_unix_ms
                 ) values (?1, ?2, 'incarnation', 'branch-oid', 1, 0, 123)",
                params![branch, path.display().to_string()],
            )
            .map_err(|error| error.to_string())?;
            Ok(())
        })
        .unwrap();

        crate::session::reconcile_worktree_state(&repo, &config).unwrap();
        assert_eq!(count_rows(&repo, "task_metadata", branch), 1);
        assert_eq!(
            crate::session::discover_sessions(&repo, &config)
                .unwrap()
                .iter()
                .map(|session| session.status_label.as_str())
                .collect::<Vec<_>>(),
            vec!["deletion pending"]
        );

        let outcome =
            crate::session::delete_worktree_session_if_current(&repo, &config, &path, branch, None)
                .unwrap();

        assert_eq!(outcome, crate::session::DeleteWorktreeOutcome::Deleted);
        assert!(!wt_called.exists(), "resumed cleanup repeated wt remove");
        assert_eq!(count_rows(&repo, "task_metadata", branch), 0);
        assert_eq!(count_rows(&repo, "pending_worktree_deletion", branch), 0);
        let git_commands = fs::read_to_string(git_log).unwrap();
        assert!(git_commands.contains("show-ref --verify --quiet"));
        assert!(!git_commands.contains(&format!("branch -D -- {branch}")));
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn branch_delete_failure_reports_removed_worktree_and_retained_branch() {
        let temp = unique_temp_dir("prism-delete-branch-retained-test");
        fs::create_dir_all(&temp).unwrap();
        let tmux = temp.join("tmux");
        write_executable(&tmux, "#!/bin/sh\nexit 0\n");
        let git = temp.join("git");
        let wt = temp.join("wt");
        let wt_log = temp.join("wt.log");
        let fail_branch_delete = temp.join("fail-branch-delete");
        fs::write(&fail_branch_delete, "fail\n").unwrap();
        write_executable(
            &git,
            &format!(
                "#!/bin/sh\ncase \"$*\" in\n  *\"rev-parse --verify refs/heads/feature/keep\"*) echo branch-oid; exit 0 ;;\n  *\"branch -D -- feature/keep\"*) test ! -e '{}' || exit 1 ;;\n  *\"worktree list --porcelain\"*) exit 0 ;;\nesac\nexit 0\n",
                fail_branch_delete.display()
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
        let path = temp.join("worktree");
        fs::create_dir_all(&path).unwrap();
        write_successful_remove_wt(&wt, &wt_log, &path, "feature/keep");
        config
            .tools
            .insert("wt".to_string(), wt.display().to_string());
        with_test_database(&repo, |conn| {
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
        assert_eq!(count_rows(&repo, "task_metadata", "feature/keep"), 1);
        let first_wt_command = fs::read_to_string(&wt_log).unwrap();

        fs::remove_file(fail_branch_delete).unwrap();
        let retried = crate::session::delete_worktree_session_if_current(
            &repo,
            &config,
            &path,
            "feature/keep",
            None,
        )
        .unwrap();

        assert_eq!(retried, crate::session::DeleteWorktreeOutcome::Deleted);
        assert_eq!(count_rows(&repo, "task_metadata", "feature/keep"), 0);
        assert_eq!(fs::read_to_string(&wt_log).unwrap(), first_wt_command);
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
        with_test_database(&repo, |conn| {
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

    fn run_git(path: &std::path::Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git command failed: git {args:?}");
    }

    fn count_rows(repo: &Repository, table: &str, branch: &str) -> i64 {
        with_test_database(repo, |conn| {
            conn.query_row(
                &format!("select count(*) from {table} where branch = ?1"),
                params![branch],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|error| error.to_string())
        })
        .unwrap()
    }

    fn with_test_database<T>(
        repo: &Repository,
        run: impl FnOnce(&TestDatabase) -> Result<T, String>,
    ) -> Result<T, String> {
        observability::with_writable_db(repo, |path| run(&TestDatabase::open(path)?))
    }

    fn write_successful_remove_wt(
        wt: &std::path::Path,
        log: &std::path::Path,
        path: &std::path::Path,
        branch: &str,
    ) {
        write_executable(
            wt,
            &format!(
                "#!/bin/sh\nprintf '%s' \"$*\" > '{}'\nrm -rf '{}'\nprintf '%s' '[{{\"branch\":\"{}\",\"branch_deleted\":false,\"kind\":\"worktree\",\"path\":\"{}\"}}]'\n",
                log.display(),
                path.display(),
                branch,
                path.display()
            ),
        );
    }
}
