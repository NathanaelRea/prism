use std::io::{self, Write};
use std::process::Command;

use crate::config::Config;
use crate::git::worktree_dirty;
use crate::process::{run_capture, run_output_allow_failure, run_status};
use crate::repo::Repository;
use crate::session::append_runtime_log;
use crate::terminal::stdin_is_tty;
use crate::util::yes;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct StartupSetup {
    pub current_branch: Option<String>,
    pub default_base: Option<String>,
    pub no_extra_worktrees: bool,
    pub needs_prompt: bool,
    pub can_move_branch: bool,
}

pub(crate) fn maybe_prompt_startup_setup(repo: &Repository, config: &Config) -> Result<(), String> {
    if !stdin_is_tty() {
        return Ok(());
    }

    let setup = inspect_startup_setup(repo, config)?;
    if !setup.needs_prompt {
        return Ok(());
    }

    prompt_setup_loop(repo, config, setup)
}

pub(crate) fn inspect_startup_setup(
    repo: &Repository,
    config: &Config,
) -> Result<StartupSetup, String> {
    let current_branch = current_branch(repo, config)?;
    let default_base = default_base(repo, config);
    let worktree_count = worktree_count(repo, config)?;
    Ok(classify_startup(
        current_branch.as_deref(),
        default_base.as_deref(),
        worktree_count,
    ))
}

pub(crate) fn classify_startup(
    current_branch: Option<&str>,
    default_base: Option<&str>,
    worktree_count: usize,
) -> StartupSetup {
    let on_default_branch = current_branch
        .zip(default_base)
        .map(|(current, base)| current == base)
        .unwrap_or(true);
    let no_extra_worktrees = worktree_count <= 1;
    let can_move_branch = !on_default_branch && current_branch.is_some() && default_base.is_some();
    StartupSetup {
        current_branch: current_branch.map(ToString::to_string),
        default_base: default_base.map(ToString::to_string),
        no_extra_worktrees,
        needs_prompt: !on_default_branch,
        can_move_branch,
    }
}

fn prompt_setup_loop(
    repo: &Repository,
    config: &Config,
    setup: StartupSetup,
) -> Result<(), String> {
    loop {
        println!();
        println!("Prism setup");
        println!();
        if let (Some(current), Some(base)) = (&setup.current_branch, &setup.default_base)
            && current != base
        {
            println!("You are on {current}, not {base}.");
        }
        if setup.no_extra_worktrees {
            println!("No additional worktree sessions are set up yet.");
        }
        println!();
        if setup.can_move_branch {
            let dirty = worktree_dirty(repo, config)?;
            let branch = setup.current_branch.as_deref().unwrap_or("current branch");
            if dirty {
                println!("  w  move {branch} to a worktree (requires clean checkout)");
            } else {
                println!("  w  move {branch} to a worktree");
            }
        }
        println!("  o  open Prism anyway");
        print!("Choice [o]: ");
        io::stdout().flush().map_err(|error| error.to_string())?;

        let choice = read_line()?.trim().to_ascii_lowercase();
        match choice.as_str() {
            "" | "o" => return Ok(()),
            "w" if setup.can_move_branch => {
                if worktree_dirty(repo, config)? {
                    println!("Cannot move branch while this checkout is dirty.");
                    println!("Commit or stash changes, then reopen Prism.");
                    continue;
                }
                move_current_branch_to_worktree(repo, config, &setup)?;
                return Ok(());
            }
            _ => {
                println!("Unknown choice.");
            }
        }
    }
}

fn move_current_branch_to_worktree(
    repo: &Repository,
    config: &Config,
    setup: &StartupSetup,
) -> Result<(), String> {
    let branch = setup
        .current_branch
        .as_deref()
        .ok_or_else(|| "current branch is unknown".to_string())?;
    let base = setup
        .default_base
        .as_deref()
        .ok_or_else(|| "default branch is unknown".to_string())?;
    println!();
    println!("This will:");
    println!("- switch this checkout to {base}");
    println!("- create or switch to a Worktrunk worktree for {branch}");
    println!("- keep your branch and commits intact");
    print!("Proceed? [y/N] ");
    io::stdout().flush().map_err(|error| error.to_string())?;
    if !yes(&read_line()?) {
        return Ok(());
    }

    run_status(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(&repo.root)
            .args(["switch", base]),
    )?;
    run_status(
        Command::new(config.tool(&config.worktree_command))
            .arg("-C")
            .arg(&repo.root)
            .args(["switch", "--no-cd", "--format", "json", branch]),
    )?;
    let _ = append_runtime_log(
        repo,
        &format!("moved {branch} into Worktrunk worktree and switched checkout to {base}"),
    );
    Ok(())
}

fn current_branch(repo: &Repository, config: &Config) -> Result<Option<String>, String> {
    let output = run_capture(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(&repo.root)
            .args(["branch", "--show-current"]),
    )?;
    let branch = output.trim();
    if branch.is_empty() {
        Ok(None)
    } else {
        Ok(Some(branch.to_string()))
    }
}

fn default_base(repo: &Repository, config: &Config) -> Option<String> {
    config
        .default_base
        .clone()
        .or_else(|| local_branch_exists(repo, config, "main").then(|| "main".to_string()))
        .or_else(|| local_branch_exists(repo, config, "master").then(|| "master".to_string()))
}

fn local_branch_exists(repo: &Repository, config: &Config, branch: &str) -> bool {
    run_output_allow_failure(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(&repo.root)
            .args([
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ]),
    )
    .map(|output| output.status.success())
    .unwrap_or(false)
}

fn worktree_count(repo: &Repository, config: &Config) -> Result<usize, String> {
    let output = run_capture(
        Command::new(config.tool("git"))
            .arg("-C")
            .arg(&repo.root)
            .args(["worktree", "list", "--porcelain"]),
    )?;
    Ok(output
        .lines()
        .filter(|line| line.starts_with("worktree "))
        .count())
}

fn read_line() -> Result<String, String> {
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .map_err(|error| error.to_string())?;
    Ok(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_prompts_for_single_branch_worktree() {
        let setup = classify_startup(Some("feature"), Some("main"), 1);

        assert!(setup.needs_prompt);
        assert!(setup.no_extra_worktrees);
        assert!(setup.can_move_branch);
    }

    #[test]
    fn startup_does_not_prompt_for_default_branch_without_extra_worktrees() {
        let setup = classify_startup(Some("main"), Some("main"), 1);

        assert!(!setup.needs_prompt);
        assert!(!setup.can_move_branch);
    }

    #[test]
    fn startup_does_not_prompt_for_configured_multi_worktree_default_checkout() {
        let setup = classify_startup(Some("main"), Some("main"), 2);

        assert!(!setup.needs_prompt);
        assert!(!setup.can_move_branch);
    }
}
