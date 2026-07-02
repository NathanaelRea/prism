use std::io::{self, Write};

use crate::config::{Config, IconStyle};
use crate::git::{RepositoryCheckout, inspect_repository_checkout, worktree_dirty};
use crate::lifecycle::move_current_branch_to_worktree;
use crate::repo::Repository;
use crate::terminal::stdin_is_tty;
use crate::util::yes;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct StartupSetup {
    pub current_branch: Option<String>,
    pub default_base: Option<String>,
    pub no_extra_worktrees: bool,
    pub needs_prompt: bool,
    pub can_move_branch: bool,
    pub branch_move: BranchMoveDecision,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BranchMoveDecision {
    Ready,
    NotNeeded,
    Refused(BranchMoveRefusal),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BranchMoveRefusal {
    UnknownCurrentBranch,
    UnknownDefaultBranch,
    DirtyCheckout,
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

pub(crate) fn maybe_prompt_icon_style(config: &Config) -> Result<Option<IconStyle>, String> {
    if config.icon_style_configured || !stdin_is_tty() {
        return Ok(None);
    }

    println!();
    println!("Prism setup");
    println!();
    println!("Choose icon style:");
    println!();
    println!("  u  Unicode icons");
    println!("     Works in most terminals.");
    println!();
    println!("  n  Nerd Font icons");
    println!("     Looks better, but requires a Nerd Font in your terminal.");
    println!();
    print!("Choice [u]: ");
    io::stdout().flush().map_err(|error| error.to_string())?;

    let style = match read_line()?.trim().to_ascii_lowercase().as_str() {
        "n" => IconStyle::NerdFont,
        _ => IconStyle::Unicode,
    };
    config.save_user_icon_style(style)?;
    Ok(Some(style))
}

pub(crate) fn inspect_startup_setup(
    repo: &Repository,
    config: &Config,
) -> Result<StartupSetup, String> {
    Ok(classify_startup(&inspect_repository_checkout(
        repo, config,
    )?))
}

pub(crate) fn classify_startup(checkout: &RepositoryCheckout) -> StartupSetup {
    let on_default_branch = checkout
        .current_branch
        .as_deref()
        .zip(checkout.default_base.as_deref())
        .map(|(current, base)| current == base)
        .unwrap_or(true);
    let no_extra_worktrees = checkout.worktree_count <= 1;
    let branch_move = classify_branch_move(checkout, on_default_branch);
    let can_move_branch = branch_move == BranchMoveDecision::Ready;
    StartupSetup {
        current_branch: checkout.current_branch.clone(),
        default_base: checkout.default_base.clone(),
        no_extra_worktrees,
        needs_prompt: !on_default_branch,
        can_move_branch,
        branch_move,
    }
}

fn classify_branch_move(
    checkout: &RepositoryCheckout,
    on_default_branch: bool,
) -> BranchMoveDecision {
    if on_default_branch {
        return BranchMoveDecision::NotNeeded;
    }
    if checkout.current_branch.is_none() {
        return BranchMoveDecision::Refused(BranchMoveRefusal::UnknownCurrentBranch);
    }
    if checkout.default_base.is_none() {
        return BranchMoveDecision::Refused(BranchMoveRefusal::UnknownDefaultBranch);
    }
    if checkout.dirty {
        return BranchMoveDecision::Refused(BranchMoveRefusal::DirtyCheckout);
    }
    BranchMoveDecision::Ready
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
        match setup.branch_move {
            BranchMoveDecision::Ready => {
                let branch = setup.current_branch.as_deref().unwrap_or("current branch");
                println!("  w  move {branch} to a worktree");
            }
            BranchMoveDecision::Refused(BranchMoveRefusal::DirtyCheckout) => {
                let branch = setup.current_branch.as_deref().unwrap_or("current branch");
                println!("  w  move {branch} to a worktree (requires clean checkout)");
            }
            BranchMoveDecision::NotNeeded
            | BranchMoveDecision::Refused(BranchMoveRefusal::UnknownCurrentBranch)
            | BranchMoveDecision::Refused(BranchMoveRefusal::UnknownDefaultBranch) => {}
        }
        println!("  o  open Prism anyway");
        print!("Choice [o]: ");
        io::stdout().flush().map_err(|error| error.to_string())?;

        let choice = read_line()?.trim().to_ascii_lowercase();
        match choice.as_str() {
            "" | "o" => return Ok(()),
            "w" if setup.can_move_branch => {
                // Re-check immediately before moving; the prompt decision may be stale.
                if worktree_dirty(repo, config)? {
                    println!("Cannot move branch while this checkout is dirty.");
                    println!("Commit or stash changes, then reopen Prism.");
                    continue;
                }
                move_current_branch_to_worktree_from_setup(repo, config, &setup)?;
                return Ok(());
            }
            "w" if setup.branch_move
                == BranchMoveDecision::Refused(BranchMoveRefusal::DirtyCheckout) =>
            {
                if worktree_dirty(repo, config)? {
                    println!("Cannot move branch while this checkout is dirty.");
                    println!("Commit or stash changes, then reopen Prism.");
                    continue;
                }
                move_current_branch_to_worktree_from_setup(repo, config, &setup)?;
                return Ok(());
            }
            _ => {
                println!("Unknown choice.");
            }
        }
    }
}

fn move_current_branch_to_worktree_from_setup(
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

    move_current_branch_to_worktree(repo, config, branch, base)
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
        let setup = classify_startup(&checkout(Some("feature"), Some("main"), 1, false));

        assert!(setup.needs_prompt);
        assert!(setup.no_extra_worktrees);
        assert!(setup.can_move_branch);
        assert_eq!(setup.branch_move, BranchMoveDecision::Ready);
    }

    #[test]
    fn startup_does_not_prompt_for_default_branch_without_extra_worktrees() {
        let setup = classify_startup(&checkout(Some("main"), Some("main"), 1, false));

        assert!(!setup.needs_prompt);
        assert!(!setup.can_move_branch);
        assert_eq!(setup.branch_move, BranchMoveDecision::NotNeeded);
    }

    #[test]
    fn startup_does_not_prompt_for_configured_multi_worktree_default_checkout() {
        let setup = classify_startup(&checkout(Some("main"), Some("main"), 2, false));

        assert!(!setup.needs_prompt);
        assert!(!setup.no_extra_worktrees);
        assert!(!setup.can_move_branch);
        assert_eq!(setup.branch_move, BranchMoveDecision::NotNeeded);
    }

    #[test]
    fn startup_prompts_but_refuses_dirty_branch_move() {
        let setup = classify_startup(&checkout(Some("feature"), Some("main"), 1, true));

        assert!(setup.needs_prompt);
        assert!(!setup.can_move_branch);
        assert_eq!(
            setup.branch_move,
            BranchMoveDecision::Refused(BranchMoveRefusal::DirtyCheckout)
        );
    }

    #[test]
    fn startup_without_default_branch_is_not_actionable() {
        let setup = classify_startup(&checkout(Some("feature"), None, 1, false));

        assert!(!setup.needs_prompt);
        assert!(!setup.can_move_branch);
        assert_eq!(setup.branch_move, BranchMoveDecision::NotNeeded);
    }

    #[test]
    fn startup_without_current_branch_is_not_actionable() {
        let setup = classify_startup(&checkout(None, Some("main"), 1, false));

        assert!(!setup.needs_prompt);
        assert!(!setup.can_move_branch);
        assert_eq!(setup.branch_move, BranchMoveDecision::NotNeeded);
    }

    fn checkout(
        current_branch: Option<&str>,
        default_base: Option<&str>,
        worktree_count: usize,
        dirty: bool,
    ) -> RepositoryCheckout {
        RepositoryCheckout {
            current_branch: current_branch.map(ToString::to_string),
            default_base: default_base.map(ToString::to_string),
            worktree_count,
            dirty,
        }
    }
}
