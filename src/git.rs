use std::process::{Command, Stdio};

use crate::config::Config;
use crate::process::run_capture;
use crate::repo::Repository;

pub fn git_status_label(path: &std::path::Path, config: &Config) -> String {
    match run_capture(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(path)
            .args(["status", "--short", "--branch"]),
    ) {
        Ok(output) => parse_git_status_label(&output),
        Err(_) => "status error".to_string(),
    }
}

pub fn parse_git_status_label(output: &str) -> String {
    let mut branch = "";
    let mut dirty_count = 0_usize;
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            branch = rest;
        } else if !line.trim().is_empty() {
            dirty_count += 1;
        }
    }
    let ahead_count = parse_branch_count(branch, "ahead").unwrap_or(0);
    let behind_count = parse_branch_count(branch, "behind").unwrap_or(0);

    let mut parts = Vec::new();
    if dirty_count > 0 {
        parts.push(format!("dirty {dirty_count}"));
    }
    if ahead_count > 0 {
        parts.push(format!("ahead {ahead_count}"));
    }
    if behind_count > 0 {
        parts.push(format!("behind {behind_count}"));
    }
    if parts.is_empty() {
        "clean".to_string()
    } else {
        parts.join(" ")
    }
}

fn parse_branch_count(branch: &str, key: &str) -> Option<usize> {
    let start = branch.find(key)?;
    let rest = branch[start + key.len()..].trim_start();
    let digits = rest
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>();
    digits.parse().ok()
}

pub fn worktree_dirty(repo: &Repository, config: &Config) -> Result<bool, String> {
    let status = run_capture(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(&repo.root)
            .args(["status", "--short"]),
    )?;
    Ok(!status.trim().is_empty())
}

pub fn branch_behind(
    path: &std::path::Path,
    branch: &str,
    config: &Config,
) -> Result<usize, String> {
    fetch_origin(path, config)?;
    let upstream = format!("origin/{branch}");
    let count = run_capture(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(path)
            .args(["rev-list", "--count"])
            .arg(format!("{branch}..{upstream}")),
    )?;
    Ok(count.trim().parse().unwrap_or(0))
}

pub fn pull_branch(path: &std::path::Path, branch: &str, config: &Config) -> Result<(), String> {
    fetch_origin(path, config)?;
    crate::process::run_status(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(path)
            .args(["switch", branch]),
    )?;
    crate::process::run_status(Command::new(config.tool("git")).arg("-C").arg(path).args([
        "pull",
        "--ff-only",
        "origin",
        branch,
    ]))
}

fn fetch_origin(path: &std::path::Path, config: &Config) -> Result<(), String> {
    crate::process::run_status(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(path)
            .args(["fetch", "origin"]),
    )
}

pub fn selected_dirty(path: &std::path::Path, config: &Config) -> Result<bool, String> {
    let status = run_capture(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(path)
            .args(["status", "--short"]),
    )?;
    Ok(!status.trim().is_empty())
}

pub fn has_upstream(path: &std::path::Path, config: &Config) -> Result<bool, String> {
    let upstream = Command::new(config.tool("git"))
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"])
        .stderr(Stdio::null())
        .output()
        .map_err(|error| format!("git upstream check: {error}"))?;
    Ok(upstream.status.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::PromptMode;
    use crate::config::{Checks, EscapeKey, MergeMethod};

    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn git_status_label_reports_clean_ahead_and_dirty() {
        assert_eq!(parse_git_status_label("## main...origin/main\n"), "clean");
        assert_eq!(
            parse_git_status_label("## main...origin/main [ahead 1]\n"),
            "ahead 1"
        );
        assert_eq!(
            parse_git_status_label("## main...origin/main [behind 1]\n M src/main.rs\n"),
            "dirty 1 behind 1"
        );
        assert_eq!(
            parse_git_status_label(
                "## main...origin/main [ahead 3, behind 2]\n M src/main.rs\n?? new.rs\n"
            ),
            "dirty 2 ahead 3 behind 2"
        );
    }

    #[test]
    fn branch_behind_fetches_origin_even_when_worktree_is_dirty() {
        let temp = unique_temp_dir("prism-dirty-behind-test");
        let origin = temp.join("origin.git");
        let work = temp.join("work");
        let remote = temp.join("remote");
        fs::create_dir_all(&temp).unwrap();

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

        let config = test_config();
        assert_eq!(branch_behind(&work, "main", &config).unwrap(), 0);

        fs::write(work.join("tracked.txt"), "dirty\n").unwrap();
        run(Command::new("git").arg("clone").arg(&origin).arg(&remote));
        configure_user(&remote);
        fs::write(remote.join("remote.txt"), "remote\n").unwrap();
        run_git(&remote, &["add", "remote.txt"]);
        run_git(&remote, &["commit", "-m", "remote change"]);
        run_git(&remote, &["push", "origin", "main"]);

        assert_eq!(branch_behind(&work, "main", &config).unwrap(), 1);

        let _ = fs::remove_dir_all(temp);
    }

    fn configure_user(path: &Path) {
        run_git(path, &["config", "user.email", "test@example.com"]);
        run_git(path, &["config", "user.name", "Test User"]);
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

    fn test_config() -> Config {
        let tools = [("git", "git")]
            .into_iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        Config {
            default_agent: "opencode".to_string(),
            default_base: Some("main".to_string()),
            plan_dir: "plans".to_string(),
            review_packet_dir: ".agent/review".to_string(),
            worktree_command: "wt".to_string(),
            escape_key: EscapeKey::EscEsc,
            merge_method: MergeMethod::Squash,
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
