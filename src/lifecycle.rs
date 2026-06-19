use std::path::Path;
use std::process::Command;
use std::time::Instant;

use crate::config::{Config, MergeMethod};
use crate::github::{
    PrCache, PrSummary, refresh_pr_cache, remove_pr_cache, remove_pr_details_cache, save_pr_cache,
    save_pr_details_cache,
};
use crate::process::{run_capture, run_configured_commands, run_status};
use crate::repo::Repository;
use crate::session::{remove_logs, remove_session_db_records};

pub(crate) struct PrSummaryRepository<'a> {
    pub repo: &'a Repository,
    pub config: &'a Config,
}

pub(crate) fn create_worktree_session(
    repo: &Repository,
    config: &Config,
    branch: &str,
) -> Result<(), String> {
    run_capture(
        Command::new(config.tool(&config.worktree_command)).args(create_worktree_args(
            &repo.root,
            branch,
            config.default_base.as_deref(),
        )),
    )?;
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
    repo: &Repository,
    config: &Config,
    path: &Path,
    branch: &str,
    pr_number: u64,
) -> Result<(), String> {
    run_status(
        Command::new(config.tool("gh"))
            .args(merge_pr_args(&pr_number.to_string(), config.merge_method))
            .current_dir(path),
    )?;
    delete_branch_if_attached(repo, config, branch)?;
    Ok(())
}

pub(crate) fn delete_session_local_data(repo: &Repository, branch: &str) -> Result<(), String> {
    remove_session_db_records(repo, branch)?;
    remove_logs(repo, branch)?;
    Ok(())
}

pub(crate) fn delete_worktree_session(
    repo: &Repository,
    config: &Config,
    path: &Path,
    branch: &str,
) -> Result<(), String> {
    delete_session_local_data(repo, branch)?;
    remove_worktree(repo, config, path)?;
    delete_branch_if_attached(repo, config, branch)?;
    Ok(())
}

pub(crate) fn refresh_pr_summary_index_for_repo(
    repos: &[PrSummaryRepository<'_>],
    sessions: &mut [crate::session::Session],
    repo_index: usize,
    summaries: Vec<PrSummary>,
) {
    let Some(managed) = repos.get(repo_index) else {
        return;
    };
    let now = Instant::now();
    let refreshed = crate::util::timestamp_label();
    for session in sessions
        .iter_mut()
        .filter(|session| session.repo_index == repo_index)
    {
        session.pr.last_polled = Some(now);
        if session.branch == "(detached)" || managed.config.is_default_branch(&session.branch) {
            session.pr.summary = None;
            session.pr.details = None;
            session.pr.signature = None;
            session.pr.error = None;
            session.pr.last_refreshed = Some(refreshed.clone());
            let _ = remove_pr_cache(managed.repo, &session.branch);
            continue;
        }
        let summary = summaries
            .iter()
            .find(|summary| summary.head_ref == session.branch)
            .cloned();
        if let Some(summary) = summary {
            let signature = summary.signature();
            if session.pr.signature.as_deref() != Some(signature.as_str()) {
                session.pr.details = None;
                session.pr.details_last_polled = None;
            }
            session.pr.summary = Some(summary);
            session.pr.signature = Some(signature);
            session.pr.error = None;
            session.pr.last_refreshed = Some(refreshed.clone());
            let _ = save_pr_cache(managed.repo, &session.branch, &session.pr);
            if let Some(details) = &session.pr.details {
                let _ = save_pr_details_cache(managed.repo, &session.branch, details);
            } else {
                let _ = remove_pr_details_cache(managed.repo, &session.branch);
            }
        } else {
            session.pr.summary = None;
            session.pr.details = None;
            session.pr.signature = None;
            session.pr.error = None;
            session.pr.last_refreshed = Some(refreshed.clone());
            let _ = remove_pr_cache(managed.repo, &session.branch);
        }
    }
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
        "--delete-branch".to_string(),
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

#[cfg(test)]
mod tests {
    use super::{create_pr_args, create_worktree_args, merge_pr_args, remove_worktree};
    use crate::config::{Checks, Config, EscapeKey, MergeMethod};
    use crate::repo::Repository;
    use std::collections::BTreeMap;
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
            vec!["pr", "merge", "42", "--squash", "--delete-branch"]
        );
        assert_eq!(
            merge_pr_args("42", MergeMethod::Merge),
            vec!["pr", "merge", "42", "--merge", "--delete-branch"]
        );
        assert_eq!(
            merge_pr_args("42", MergeMethod::Rebase),
            vec!["pr", "merge", "42", "--rebase", "--delete-branch"]
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
}
