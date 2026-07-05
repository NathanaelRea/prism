use super::*;

pub(super) fn pr_target_choice_list(origin: &str, upstream: &str) -> crate::view::ChoiceList {
    crate::view::ChoiceList {
        title: "Create Pull Request Target".to_string(),
        choices: vec![
            crate::view::KeyChoice {
                key: "u".to_string(),
                label: format!("upstream ({upstream})"),
            },
            crate::view::KeyChoice {
                key: "o".to_string(),
                label: format!("origin ({origin})"),
            },
        ],
    }
}

pub(super) fn should_prompt_pr_target_choice(origin: &str, upstream: &str) -> bool {
    origin != upstream
}

pub(super) fn pr_target_repo_for_choice(
    choice: &str,
    origin: &str,
    upstream: &str,
) -> Option<String> {
    match choice {
        "u" => Some(upstream.to_string()),
        "o" => Some(origin.to_string()),
        _ => None,
    }
}

pub(super) fn open_url_in_browser(url: &str) -> Result<(), String> {
    run_browser_opener(&browser_opener_candidates(), url).map(|_| ())
}

pub(super) const NO_BROWSER_ARGS: &[&str] = &[];
pub(super) const GIO_BROWSER_ARGS: &[&str] = &["open"];
pub(super) const WINDOWS_BROWSER_ARGS: &[&str] = &["/C", "start", ""];

pub(super) fn browser_opener_candidates() -> Vec<(&'static str, &'static [&'static str])> {
    if cfg!(target_os = "macos") {
        vec![("open", NO_BROWSER_ARGS)]
    } else if cfg!(target_os = "windows") {
        vec![("cmd", WINDOWS_BROWSER_ARGS)]
    } else {
        vec![
            ("xdg-open", NO_BROWSER_ARGS),
            ("gio", GIO_BROWSER_ARGS),
            ("wslview", NO_BROWSER_ARGS),
        ]
    }
}

pub(super) fn run_browser_opener(
    candidates: &[(&str, &[&str])],
    url: &str,
) -> Result<String, String> {
    let mut errors = Vec::new();
    for (program, args) in candidates {
        if !command_exists(program) {
            continue;
        }
        match Command::new(program)
            .args(*args)
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        {
            Ok(status) if status.success() => return Ok((*program).to_string()),
            Ok(status) => errors.push(format!("{program}: exited with {status}")),
            Err(error) => errors.push(format!("{program}: {error}")),
        }
    }
    if errors.is_empty() {
        let names = candidates
            .iter()
            .map(|(program, _)| *program)
            .collect::<Vec<_>>()
            .join(", ");
        Err(format!("no browser opener found; tried {names}"))
    } else {
        Err(format!("browser open failed: {}", errors.join("; ")))
    }
}

impl Tui {
    pub(crate) fn start_review_fix(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        self.show_loading_dialog(
            raw,
            "Review Fix Prompt",
            "Refreshing pull request review details",
        )?;
        self.send_review_fix_prompt()
    }

    pub(super) fn send_review_fix_prompt(&mut self) -> Result<(), String> {
        let Some(context) = self.selected_worktree_context() else {
            return Ok(());
        };
        let selected = context.session_index;
        if self.sessions[selected].is_default_branch(&context.config) {
            self.show_message("default branch has no PR review comments")?;
            return Ok(());
        }
        {
            let session = &mut self.sessions[selected];
            refresh_branch_pr_cache(
                &context.repo,
                &context.config,
                &session.branch,
                &session.path,
                &mut session.pr,
                true,
            );
        }
        let prompt = build_review_fix_prompt(&self.sessions[selected], &context.config)?;
        self.submit_action_prompt_to_agent(selected, &context.repo, "review fix", &prompt)?;
        self.show_message("review-fix prompt sent to new agent session")?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn start_review_fix_for_test(&mut self) -> Result<(), String> {
        self.send_review_fix_prompt()
    }

    pub(crate) fn start_ci_fix(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let Some(context) = self.selected_worktree_context() else {
            return Ok(());
        };
        let selected = context.session_index;
        if self.sessions[selected].is_default_branch(&context.config) {
            self.show_message("default branch has no PR CI failures")?;
            return Ok(());
        }
        self.show_loading_dialog(
            raw,
            "CI Failure Prompt",
            "Refreshing pull request CI details",
        )?;
        self.send_ci_fix_prompt()
    }

    pub(super) fn send_ci_fix_prompt(&mut self) -> Result<(), String> {
        let Some(context) = self.selected_worktree_context() else {
            return Ok(());
        };
        let selected = context.session_index;
        if self.sessions[selected].is_default_branch(&context.config) {
            self.show_message("default branch has no PR CI failures")?;
            return Ok(());
        }
        {
            let session = &mut self.sessions[selected];
            refresh_branch_pr_cache(
                &context.repo,
                &context.config,
                &session.branch,
                &session.path,
                &mut session.pr,
                true,
            );
        }
        let prompt = build_ci_failure_prompt(&self.sessions[selected], &context.config)?;
        self.submit_action_prompt_to_agent(selected, &context.repo, "ci fix", &prompt)?;
        self.show_message("CI-failure prompt sent to new agent session")?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn start_ci_fix_for_test(&mut self) -> Result<(), String> {
        self.send_ci_fix_prompt()
    }

    pub(crate) fn open_selected_pr(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let Some(context) = self.selected_worktree_context() else {
            return Ok(());
        };
        let selected = context.session_index;
        if self.sessions[selected].is_default_branch(&context.config) {
            self.show_message("default branch is not treated as a PR branch")?;
            return Ok(());
        }
        if self.sessions[selected].is_detached() {
            self.show_message("cannot open a PR for a detached worktree")?;
            return Ok(());
        }
        if self.sessions[selected].pr.summary.is_none() {
            self.show_loading_dialog(raw, "Open Pull Request", "Refreshing pull request")?;
            let session = &mut self.sessions[selected];
            refresh_pr_cache(
                &context.repo,
                &session.branch,
                &mut session.pr,
                &session.path,
                &context.config,
                false,
            );
        }
        let Some(summary) = pr_summary_or_error(&self.sessions[selected].pr)? else {
            self.show_message("no pull request found for selected branch")?;
            return Ok(());
        };
        let url = summary.url.trim();
        if url.is_empty() {
            return Err(format!("PR #{} has no URL", summary.number));
        }
        open_url_in_browser(url)?;
        self.show_message(&format!("opened PR #{} in browser", summary.number))?;
        Ok(())
    }

    pub(crate) fn push_selected_branch(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let Some(context) = self.selected_worktree_context() else {
            return Ok(());
        };
        let selected = context.session_index;
        let path = self.sessions[selected].path.clone();
        let branch = self.sessions[selected].branch.clone();
        if self.sessions[selected].is_default_branch(&context.config) {
            self.show_message("default branch is not treated as a PR branch")?;
            return Ok(());
        }
        if self.sessions[selected].is_detached() {
            self.show_message("cannot push a detached worktree")?;
            return Ok(());
        }
        run_pre_push_checks(&context.config, &path)?;
        let set_upstream = !has_upstream(&path, &context.config)?;
        self.show_loading_dialog(raw, "Push Branch", "Pushing selected branch")?;
        push_branch(&context.config, &path, &branch, set_upstream)?;
        {
            let session = &mut self.sessions[selected];
            refresh_branch_pr_cache(
                &context.repo,
                &context.config,
                &session.branch,
                &session.path,
                &mut session.pr,
                true,
            );
        }
        if self.sessions[selected].pr.summary.is_none()
            && !self.sessions[selected].is_default_branch(&context.config)
        {
            run_pre_pr_checks(&context.config, &path)?;
            let target_repo =
                if let Ok(upstream) = github_remote_repo(&path, &context.config, "upstream") {
                    let origin = github_remote_repo(&path, &context.config, "origin")?;
                    if !should_prompt_pr_target_choice(&origin, &upstream) {
                        None
                    } else {
                        let Some(choice) = self
                            .prompt_choice_dialog(raw, pr_target_choice_list(&origin, &upstream))?
                        else {
                            return Ok(());
                        };
                        pr_target_repo_for_choice(&choice, &origin, &upstream)
                    }
                } else {
                    None
                };
            let Some(pr_body) = self.prompt_pr_description(raw)? else {
                return Ok(());
            };
            self.show_loading_dialog(raw, "Create Pull Request", "Creating pull request")?;
            let session = &mut self.sessions[selected];
            create_pull_request(
                &context.repo,
                &context.config,
                &session.branch,
                &session.path,
                &pr_body,
                target_repo.as_deref(),
                &mut session.pr,
            )?;
            self.show_message("push complete; pull request created")?;
        } else {
            self.show_message("push complete")?;
        }
        Ok(())
    }

    pub(super) fn prompt_pr_description(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<Option<String>, String> {
        self.prompt_line_dialog(raw, "Create Pull Request", "Description: ", "")
    }

    pub(crate) fn merge_selected_pr(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let Some(context) = self.selected_worktree_context() else {
            return Ok(());
        };
        let selected = context.session_index;
        if self.sessions[selected].is_default_branch(&context.config) {
            self.show_message("default branch is not treated as a PR branch")?;
            return Ok(());
        }
        let path = self.sessions[selected].path.clone();
        let branch = self.sessions[selected].branch.clone();
        {
            let session = &mut self.sessions[selected];
            refresh_branch_pr_cache(
                &context.repo,
                &context.config,
                &session.branch,
                &session.path,
                &mut session.pr,
                false,
            );
        }
        let Some(summary) = self.sessions[selected].pr.summary.clone() else {
            self.show_message("no pull request found for selected branch")?;
            return Ok(());
        };
        if summary.merged {
            self.show_message("pull request is already merged")?;
            return Ok(());
        }
        if selected_dirty(&path, &context.config)? {
            self.show_message("working tree is dirty; commit or stash before merging")?;
            return Ok(());
        }
        run_pre_push_checks(&context.config, &path)?;
        self.show_loading_dialog(
            raw,
            "Merge Pull Request",
            &format!("Merging PR #{}", summary.number),
        )?;
        merge_pull_request(&context.config, &path, summary.number)?;
        self.show_loading_dialog(
            raw,
            "Merge Pull Request",
            &format!("Verifying PR #{} is merged", summary.number),
        )?;
        let merged = match wait_for_pr_merged(&path, summary.number, &context.config) {
            Ok(merged) => merged,
            Err(error) => {
                self.refresh_sessions()?;
                self.show_message(&format!(
                    "merge complete; could not verify PR merged: {error}"
                ))?;
                return Ok(());
            }
        };
        if !merged {
            self.refresh_sessions()?;
            self.show_message("merge complete; GitHub has not marked the PR merged yet")?;
            return Ok(());
        }

        if let Some(summary) = self.sessions[selected].pr.summary.as_mut() {
            summary.merged = true;
            summary.state = "MERGED".to_string();
        }
        let path_display = self.sessions[selected].path_display.clone();
        let warnings = self.sessions[selected].deletion_warnings();
        if self.confirm_delete_dialog(raw, &branch, &path_display, &warnings)? {
            delete_worktree_session(&context.repo, &context.config, &path, &branch)?;
            if self.selected_worktree_by_repo.get(&context.repo.root) == Some(&path) {
                self.selected_worktree_by_repo.remove(&context.repo.root);
            }
            self.refresh_sessions()?;
            self.show_message("merge complete; deleted local session data, worktree, and branch")?;
        } else {
            self.refresh_sessions()?;
            self.show_message("merge complete")?;
        }
        Ok(())
    }
}
