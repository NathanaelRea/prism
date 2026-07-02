#![allow(dead_code)]

use std::path::Path;
use std::process::Command;

use crate::config::Config;
use crate::process::{run_configured_commands, run_output_allow_failure, run_status};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VerifyMode {
    Normal,
    ReviewFix,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum VerifyCheckKind {
    Configured,
    MergeConflict,
    Warning,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VerifyCheckResult {
    pub kind: VerifyCheckKind,
    pub label: String,
    pub passed: bool,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VerifyResult {
    pub passed: bool,
    pub checks: Vec<VerifyCheckResult>,
}

pub(crate) fn run_auto_verify(config: &Config, path: &Path, mode: VerifyMode) -> VerifyResult {
    let mut checks = Vec::new();

    if mode == VerifyMode::ReviewFix {
        checks.push(run_configured_check(
            path,
            "review_fix",
            &config.checks.review_fix,
        ));
    }

    if config.checks.pre_push.is_empty() && config.checks.pre_pr.is_empty() {
        checks.push(VerifyCheckResult {
            kind: VerifyCheckKind::Warning,
            label: "configured checks".to_string(),
            passed: true,
            message: "no pre_push or pre_pr checks configured".to_string(),
        });
    } else {
        checks.push(run_configured_check(
            path,
            "pre_push",
            &config.checks.pre_push,
        ));
        checks.push(run_configured_check(path, "pre_pr", &config.checks.pre_pr));
    }

    checks.push(run_merge_conflict_check(config, path));

    VerifyResult {
        passed: checks.iter().all(|check| check.passed),
        checks,
    }
}

pub(crate) fn run_merge_conflict_check(config: &Config, path: &Path) -> VerifyCheckResult {
    let Some(base) = config
        .default_base
        .as_deref()
        .map(str::trim)
        .filter(|base| !base.is_empty())
    else {
        return VerifyCheckResult {
            kind: VerifyCheckKind::MergeConflict,
            label: "merge conflict".to_string(),
            passed: true,
            message: "no default_base configured; merge conflict check skipped".to_string(),
        };
    };

    let fetch = run_status(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(path)
            .args(["fetch", "origin", base]),
    );
    if let Err(error) = fetch {
        return VerifyCheckResult {
            kind: VerifyCheckKind::MergeConflict,
            label: "merge conflict".to_string(),
            passed: false,
            message: format!("fetch origin/{base} failed: {error}"),
        };
    }

    match merge_tree_write_tree(config, path, base) {
        MergeTreeOutcome::Clean => VerifyCheckResult {
            kind: VerifyCheckKind::MergeConflict,
            label: "merge conflict".to_string(),
            passed: true,
            message: format!("HEAD merges cleanly with origin/{base}"),
        },
        MergeTreeOutcome::Conflict(message) => VerifyCheckResult {
            kind: VerifyCheckKind::MergeConflict,
            label: "merge conflict".to_string(),
            passed: false,
            message,
        },
        MergeTreeOutcome::Unsupported(message) => fallback_merge_conflict_check(
            config,
            path,
            base,
            &format!("git merge-tree --write-tree unavailable: {message}"),
        ),
    }
}

fn run_configured_check(path: &Path, label: &str, commands: &[String]) -> VerifyCheckResult {
    if commands.is_empty() {
        return VerifyCheckResult {
            kind: VerifyCheckKind::Configured,
            label: label.to_string(),
            passed: true,
            message: "no commands configured".to_string(),
        };
    }

    match run_configured_commands(commands, path, label) {
        Ok(()) => VerifyCheckResult {
            kind: VerifyCheckKind::Configured,
            label: label.to_string(),
            passed: true,
            message: format!("{} command(s) passed", commands.len()),
        },
        Err(error) => VerifyCheckResult {
            kind: VerifyCheckKind::Configured,
            label: label.to_string(),
            passed: false,
            message: error,
        },
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum MergeTreeOutcome {
    Clean,
    Conflict(String),
    Unsupported(String),
}

fn merge_tree_write_tree(config: &Config, path: &Path, base: &str) -> MergeTreeOutcome {
    let output = match run_output_allow_failure(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(path)
            .args(["merge-tree", "--write-tree", "HEAD"])
            .arg(format!("origin/{base}")),
    ) {
        Ok(output) => output,
        Err(error) => return MergeTreeOutcome::Unsupported(error),
    };
    if output.status.success() {
        return MergeTreeOutcome::Clean;
    }

    let combined = format!("{}{}", output.stdout, output.stderr);
    if is_merge_tree_unsupported(&combined) {
        MergeTreeOutcome::Unsupported(combined.trim().to_string())
    } else {
        MergeTreeOutcome::Conflict(format!(
            "HEAD does not merge cleanly with origin/{base}: {}",
            combined.trim()
        ))
    }
}

fn fallback_merge_conflict_check(
    config: &Config,
    path: &Path,
    base: &str,
    reason: &str,
) -> VerifyCheckResult {
    let temp = std::env::temp_dir().join(format!(
        "prism-merge-check-{}-{}",
        std::process::id(),
        current_unix_nanos()
    ));
    let add = run_status(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(path)
            .args(["worktree", "add", "--detach"])
            .arg(&temp)
            .arg("HEAD"),
    );
    if let Err(error) = add {
        let _ = std::fs::remove_dir_all(&temp);
        return VerifyCheckResult {
            kind: VerifyCheckKind::MergeConflict,
            label: "merge conflict".to_string(),
            passed: false,
            message: format!("{reason}; fallback worktree setup failed: {error}"),
        };
    }

    let merge = run_status(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(&temp)
            .args(["merge", "--no-commit", "--no-ff"])
            .arg(format!("origin/{base}")),
    );
    let remove = run_status(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(path)
            .args(["worktree", "remove", "--force"])
            .arg(&temp),
    );
    let _ = std::fs::remove_dir_all(&temp);

    match (merge, remove) {
        (Ok(()), Ok(())) => VerifyCheckResult {
            kind: VerifyCheckKind::MergeConflict,
            label: "merge conflict".to_string(),
            passed: true,
            message: format!("{reason}; fallback found HEAD merges cleanly with origin/{base}"),
        },
        (Ok(()), Err(error)) => VerifyCheckResult {
            kind: VerifyCheckKind::MergeConflict,
            label: "merge conflict".to_string(),
            passed: false,
            message: format!("fallback cleanup failed after clean merge check: {error}"),
        },
        (Err(error), _) => VerifyCheckResult {
            kind: VerifyCheckKind::MergeConflict,
            label: "merge conflict".to_string(),
            passed: false,
            message: format!(
                "{reason}; fallback detected merge conflict with origin/{base}: {error}"
            ),
        },
    }
}

fn is_merge_tree_unsupported(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    lower.contains("unknown option")
        || lower.contains("usage:")
        || lower.contains("not a git command")
}

fn current_unix_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::PromptMode;
    use crate::config::{Checks, EscapeKey, MergeMethod};
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn auto_verify_records_warning_when_no_local_checks_are_configured() {
        let temp = unique_temp_dir("prism-verify-no-checks-test");
        let work = temp.join("work");
        fs::create_dir_all(&work).unwrap();
        run_git(&work, &["init"]);
        configure_user(&work);
        fs::write(work.join("tracked.txt"), "base\n").unwrap();
        run_git(&work, &["add", "tracked.txt"]);
        run_git(&work, &["commit", "-m", "initial"]);

        let result = run_auto_verify(&test_config(None), &work, VerifyMode::Normal);

        assert!(result.passed);
        assert!(result.checks.iter().any(|check| {
            check.kind == VerifyCheckKind::Warning
                && check.message.contains("no pre_push or pre_pr checks")
        }));
        assert!(result.checks.iter().any(|check| {
            check.kind == VerifyCheckKind::MergeConflict
                && check.message.contains("no default_base configured")
        }));

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn auto_verify_runs_review_fix_before_normal_checks() {
        let temp = unique_temp_dir("prism-verify-review-fix-test");
        let work = temp.join("work");
        fs::create_dir_all(&work).unwrap();
        run_git(&work, &["init"]);
        configure_user(&work);
        fs::write(work.join("tracked.txt"), "base\n").unwrap();
        run_git(&work, &["add", "tracked.txt"]);
        run_git(&work, &["commit", "-m", "initial"]);

        let mut config = test_config(None);
        config.checks.review_fix = vec!["git status --short".to_string()];
        config.checks.pre_push = vec!["git status --short".to_string()];
        let result = run_auto_verify(&config, &work, VerifyMode::ReviewFix);

        assert!(result.passed);
        assert_eq!(result.checks[0].label, "review_fix");
        assert_eq!(result.checks[1].label, "pre_push");

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn auto_verify_returns_structured_failure_for_configured_checks() {
        let temp = unique_temp_dir("prism-verify-check-failure-test");
        let work = temp.join("work");
        fs::create_dir_all(&work).unwrap();
        run_git(&work, &["init"]);
        configure_user(&work);
        fs::write(work.join("tracked.txt"), "base\n").unwrap();
        run_git(&work, &["add", "tracked.txt"]);
        run_git(&work, &["commit", "-m", "initial"]);

        let mut config = test_config(None);
        config.checks.pre_push = vec!["git definitely-not-a-command".to_string()];
        let result = run_auto_verify(&config, &work, VerifyMode::Normal);

        assert!(!result.passed);
        let failure = result
            .checks
            .iter()
            .find(|check| check.label == "pre_push")
            .unwrap();
        assert!(!failure.passed);
        assert!(failure.message.contains("pre_push check"));

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn merge_conflict_check_passes_for_clean_merge() {
        let temp = unique_temp_dir("prism-merge-clean-test");
        let (_origin, work, remote) = setup_remote_repo(&temp);
        run_git(&work, &["switch", "-c", "feature"]);
        fs::write(work.join("feature.txt"), "feature\n").unwrap();
        run_git(&work, &["add", "feature.txt"]);
        run_git(&work, &["commit", "-m", "feature"]);
        fs::write(remote.join("main.txt"), "main\n").unwrap();
        run_git(&remote, &["add", "main.txt"]);
        run_git(&remote, &["commit", "-m", "main change"]);
        run_git(&remote, &["push", "origin", "main"]);

        let result = run_merge_conflict_check(&test_config(Some("main")), &work);

        assert!(result.passed, "{}", result.message);
        assert!(result.message.contains("origin/main"));

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn merge_conflict_check_reports_conflicts_without_mutating_worktree() {
        let temp = unique_temp_dir("prism-merge-conflict-test");
        let (_origin, work, remote) = setup_remote_repo(&temp);
        run_git(&work, &["switch", "-c", "feature"]);
        fs::write(work.join("tracked.txt"), "feature\n").unwrap();
        run_git(&work, &["add", "tracked.txt"]);
        run_git(&work, &["commit", "-m", "feature"]);
        fs::write(remote.join("tracked.txt"), "main\n").unwrap();
        run_git(&remote, &["add", "tracked.txt"]);
        run_git(&remote, &["commit", "-m", "main change"]);
        run_git(&remote, &["push", "origin", "main"]);

        let before = git_status(&work);
        let result = run_merge_conflict_check(&test_config(Some("main")), &work);
        let after = git_status(&work);

        assert!(!result.passed);
        assert!(
            result.message.contains("origin/main") || result.message.contains("conflict"),
            "{}",
            result.message
        );
        assert_eq!(after, before);

        let _ = fs::remove_dir_all(temp);
    }

    fn setup_remote_repo(temp: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let origin = temp.join("origin.git");
        let work = temp.join("work");
        let remote = temp.join("remote");
        fs::create_dir_all(temp).unwrap();
        run(Command::new("git").args(["init", "--bare"]).arg(&origin));
        run(Command::new("git").arg("--git-dir").arg(&origin).args([
            "symbolic-ref",
            "HEAD",
            "refs/heads/main",
        ]));
        run(Command::new("git").arg("clone").arg(&origin).arg(&work));
        configure_user(&work);
        fs::write(work.join("tracked.txt"), "base\n").unwrap();
        run_git(&work, &["add", "tracked.txt"]);
        run_git(&work, &["commit", "-m", "initial"]);
        run_git(&work, &["push", "-u", "origin", "main"]);
        run(Command::new("git").arg("clone").arg(&origin).arg(&remote));
        configure_user(&remote);
        (origin, work, remote)
    }

    fn configure_user(path: &Path) {
        run_git(path, &["config", "user.email", "test@example.com"]);
        run_git(path, &["config", "user.name", "Test User"]);
    }

    fn git_status(path: &Path) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["status", "--short"])
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8_lossy(&output.stdout).to_string()
    }

    fn run_git(path: &Path, args: &[&str]) {
        run(Command::new("git").arg("-C").arg(path).args(args));
    }

    fn run(command: &mut Command) {
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "command failed: {:?}\nstdout: {}\nstderr: {}",
            command,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn test_config(default_base: Option<&str>) -> Config {
        let tools = [("git", "git")]
            .into_iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        Config {
            default_agent: "opencode".to_string(),
            default_base: default_base.map(str::to_string),
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
            worktree_columns: vec!["url".to_string()],
            tools,
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
        std::env::temp_dir().join(format!("{prefix}-{nanos}"))
    }
}
