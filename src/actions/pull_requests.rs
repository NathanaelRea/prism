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

pub(super) fn remote_pr_choice_keys() -> Vec<String> {
    ('1'..='9')
        .chain('a'..='z')
        .map(|key| key.to_string())
        .collect()
}

pub(super) fn remote_pr_worktree_branch(number: u64) -> String {
    format!("pr/{number}")
}

fn remote_pr_choice_label(summary: &crate::github::PrSummary) -> String {
    format!(
        "#{}  {}  {} -> {}",
        summary.number, summary.title, summary.head_ref, summary.base_ref
    )
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
    pub(crate) fn open_remote_pr_worktree(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let context = self
            .selected_repo_context()
            .ok_or_else(|| "no selected repository".to_string())?;
        self.show_loading_dialog(raw, "Remote Pull Requests", "Loading open pull requests")?;
        let mut prs = fetch_pr_summary_index(&context.repo.root, &context.config)?;
        prs.retain(|summary| !summary.merged && summary.state.eq_ignore_ascii_case("OPEN"));
        if prs.is_empty() {
            self.show_message("selected repository has no open pull requests")?;
            return Ok(());
        }

        let keys = remote_pr_choice_keys();
        let choices = prs
            .iter()
            .take(keys.len())
            .zip(keys.iter())
            .map(|(summary, key)| crate::view::KeyChoice {
                key: key.clone(),
                label: remote_pr_choice_label(summary),
            })
            .collect::<Vec<_>>();
        let Some(answer) = self.prompt_choice_dialog(
            raw,
            crate::view::ChoiceList {
                title: format!(
                    "Open Pull Request Worktree: {}",
                    context.repo.root.display()
                ),
                choices,
            },
        )?
        else {
            return Ok(());
        };
        let Some(index) = keys.iter().position(|key| *key == answer) else {
            return Ok(());
        };
        let Some(summary) = prs.get(index).cloned() else {
            return Ok(());
        };

        if self.select_existing_pr_worktree(context.repo_index, &summary)? {
            return Ok(());
        }

        let branch = remote_pr_worktree_branch(summary.number);
        self.show_loading_dialog(
            raw,
            "Remote Pull Requests",
            &format!("Fetching PR #{}", summary.number),
        )?;
        fetch_pull_request_branch(&context.repo.root, &context.config, summary.number, &branch)?;
        self.show_loading_dialog(
            raw,
            "Remote Pull Requests",
            &format!("Opening worktree for PR #{}", summary.number),
        )?;
        if let Err(error) = checkout_worktree_session(&context.repo, &context.config, &branch) {
            if !is_worktrunk_approval_failure(&error)
                || !self.offer_worktrunk_approval(raw, &context.repo, &context.config)?
            {
                return Err(error);
            }
            self.show_loading_dialog(
                raw,
                "Remote Pull Requests",
                &format!("Opening worktree for PR #{}", summary.number),
            )?;
            checkout_worktree_session(&context.repo, &context.config, &branch)?;
        }

        self.refresh_sessions()?;
        self.start_tmux_agent_warmup();
        self.start_wt_column_poll();
        self.select_pr_worktree_by_branch(context.repo_index, &branch, Some(summary.clone()));
        self.focus_worktrees();
        self.show_message(&format!("opened worktree for PR #{}", summary.number))?;
        Ok(())
    }

    fn select_existing_pr_worktree(
        &mut self,
        repo_index: usize,
        summary: &crate::github::PrSummary,
    ) -> Result<bool, String> {
        if let Some(index) = self.sessions.iter().position(|session| {
            !session.hidden
                && session.repo_index == repo_index
                && session
                    .pr
                    .summary
                    .as_ref()
                    .is_some_and(|cached| cached.number == summary.number)
        }) {
            self.select_worktree(index);
            self.focus_worktrees();
            self.show_message(&format!(
                "selected existing worktree for PR #{}",
                summary.number
            ))?;
            return Ok(true);
        }
        let branch = remote_pr_worktree_branch(summary.number);
        if let Some(index) = self.sessions.iter().position(|session| {
            !session.hidden && session.repo_index == repo_index && session.branch == branch
        }) {
            if let Some(session) = self.sessions.get_mut(index) {
                session.pr.summary = Some(summary.clone());
            }
            self.select_worktree(index);
            self.focus_worktrees();
            self.show_message(&format!(
                "selected existing worktree for PR #{}",
                summary.number
            ))?;
            return Ok(true);
        }
        Ok(false)
    }

    fn select_pr_worktree_by_branch(
        &mut self,
        repo_index: usize,
        branch: &str,
        summary: Option<crate::github::PrSummary>,
    ) {
        if let Some(index) = self
            .sessions
            .iter()
            .position(|session| session.repo_index == repo_index && session.branch == branch)
        {
            if let Some(summary) = summary
                && let Some(session) = self.sessions.get_mut(index)
            {
                session.pr.summary = Some(summary);
            }
            if !self.visible_session_indices().contains(&index) {
                self.worktree_filter.clear();
            }
            self.select_worktree(index);
        }
    }

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
            crate::github::refresh_pr_details_cache(
                &session.branch,
                &mut session.pr,
                &session.path,
                &context.config,
            );
        }
        let tracked = crate::review::build_tracked_review_fix_prompt(
            &self.sessions[selected],
            &context.config,
        )?;
        self.start_managed_repair(
            selected,
            &context.repo,
            &context.config,
            AutoStepKey::FixReview,
            tracked.prompt,
            Some(crate::auto_flow::stabilization_model::WorkGuard {
                review_thread_ids: tracked.review_thread_ids,
                ..Default::default()
            }),
        )?;
        self.show_message("started managed review repair; commit will wait for guarded push")?;
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
        self.start_managed_repair(
            selected,
            &context.repo,
            &context.config,
            AutoStepKey::FixCi,
            prompt,
            None,
        )?;
        self.show_message("started managed CI repair; commit will wait for guarded push")?;
        Ok(())
    }

    fn start_managed_repair(
        &mut self,
        selected: usize,
        repo: &crate::repo::Repository,
        config: &crate::config::Config,
        step_key: AutoStepKey,
        prompt: String,
        work_guard: Option<crate::auto_flow::stabilization_model::WorkGuard>,
    ) -> Result<(), String> {
        let session_path = self.sessions[selected].path.clone();
        let session_branch = self.sessions[selected].branch.clone();
        let mut persisted = if let Some(run_id) = self.active_auto_runs.get(&session_path).cloned()
        {
            crate::observability::with_writable_db(repo, |conn| load_auto_run(conn, &run_id))?
                .ok_or_else(|| format!("active Auto Flow run not found: {run_id}"))?
        } else {
            let initial_prompt = self.sessions[selected].prompt_summary.trim();
            let initial_prompt = if initial_prompt.is_empty() {
                format!("Repair PR branch {session_branch}")
            } else {
                initial_prompt.to_string()
            };
            let launch = AutoLaunch::with_options(
                &repo.root,
                &session_path,
                AutoLaunchOptions {
                    branch: session_branch.clone(),
                    mode: AutoRunMode::Standard,
                    implementation_source: AutoImplementationSource::Prompt,
                    plan_path: None,
                    plan_run_mode: PlanRunMode::Sequential,
                    variant: "repair".to_string(),
                    agent_profile: None,
                    initial_prompt,
                },
            )?;
            let mut run = launch.create_run();
            run.steps.clear();
            run.run.pr_number = self.sessions[selected]
                .pr
                .summary
                .as_ref()
                .map(|summary| summary.number);
            run.run.pr_url = self.sessions[selected]
                .pr
                .summary
                .as_ref()
                .map(|summary| summary.url.clone());
            run.run.current_head_sha = crate::git::current_head_sha(&session_path, config).ok();
            run
        };

        crate::observability::with_writable_db(repo, |conn| {
            save_auto_run(conn, &mut persisted)?;
            if let Some(work_guard) = work_guard {
                crate::auto_flow::append_step_run_with_work_guard(
                    conn,
                    &mut persisted,
                    step_key,
                    Some(prompt),
                    work_guard,
                )?;
            } else {
                append_step_run(conn, &mut persisted, step_key, Some(prompt))?;
            }
            Ok(())
        })?;
        self.remember_auto_run(persisted.clone());
        self.selected_auto_run = Some(persisted.run.id.clone());
        #[cfg(test)]
        if self.prompt_submissions.is_some() {
            return Ok(());
        }
        self.spawn_auto_run_executor(repo.clone(), config.clone(), persisted);
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

        if self.push_guarded_pending_repair(raw, selected, &context.repo, &context.config)? {
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

    fn push_guarded_pending_repair(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
        selected: usize,
        repo: &crate::repo::Repository,
        config: &crate::config::Config,
    ) -> Result<bool, String> {
        let path = self.sessions[selected].path.clone();
        let branch = self.sessions[selected].branch.clone();
        let Some(run_id) = self.active_auto_runs.get(&path).cloned() else {
            return Ok(false);
        };

        let mut persisted =
            crate::observability::with_writable_db(repo, |conn| load_auto_run(conn, &run_id))?
                .ok_or_else(|| format!("active Auto Flow run not found: {run_id}"))?;
        let Some(guard) = persisted.run.pending_push.clone() else {
            return Ok(false);
        };

        self.show_loading_dialog(raw, "Guarded Push", "Reobserving guarded repair push")?;
        crate::git::fetch_origin(&path, config)?;
        {
            let session = &mut self.sessions[selected];
            refresh_branch_pr_cache(
                repo,
                config,
                &session.branch,
                &session.path,
                &mut session.pr,
                true,
            );
        }
        let local_head = crate::git::current_head_sha(&path, config).ok();
        let remote_head = crate::git::remote_branch_head_sha(&path, &branch, config)
            .ok()
            .flatten();
        let pr_head = self.sessions[selected]
            .pr
            .summary
            .as_ref()
            .map(|summary| summary.head_sha.clone());

        match decide_guarded_push(
            &guard,
            local_head.as_deref(),
            remote_head.as_deref(),
            pr_head.as_deref(),
        ) {
            GuardedPushDecision::AlreadySatisfied => {
                self.finish_guarded_push(repo, config, selected, &mut persisted, true)?;
                self.show_message(
                    "guarded repair push already satisfied; reobserved PR Stabilization",
                )?;
            }
            GuardedPushDecision::Invalidated { reason } => {
                persisted.run.pending_push = None;
                self.update_persisted_stabilization(repo, config, selected, &mut persisted)?;
                self.remember_auto_run(persisted);
                self.show_message(&format!("guarded repair push invalidated: {reason}"))?;
            }
            GuardedPushDecision::ValidToPush => {
                run_pre_push_checks(config, &path)?;
                self.show_loading_dialog(raw, "Guarded Push", "Pushing guarded repair commit")?;
                crate::git::push_current_branch(&path, config)?;
                self.finish_guarded_push(repo, config, selected, &mut persisted, false)?;
                self.show_message("guarded repair pushed; reobserved PR Stabilization")?;
            }
        }
        Ok(true)
    }

    fn finish_guarded_push(
        &mut self,
        repo: &crate::repo::Repository,
        config: &crate::config::Config,
        selected: usize,
        persisted: &mut PersistedAutoRun,
        already_satisfied: bool,
    ) -> Result<(), String> {
        if let Some(guard) = persisted.run.pending_push.as_ref()
            && matches!(
                guard.repair_kind,
                crate::auto_flow::stabilization_model::RepairKind::Review
            )
            && !guard.guarded_review_thread_ids.is_empty()
        {
            let _ = crate::github::resolve_review_threads(
                &persisted.run.worktree_path,
                config,
                &guard.guarded_review_thread_ids,
            )?;
        }
        persisted.run.pending_push = None;
        if !already_satisfied {
            let session = &mut self.sessions[selected];
            refresh_branch_pr_cache(
                repo,
                config,
                &session.branch,
                &session.path,
                &mut session.pr,
                true,
            );
        }
        self.update_persisted_stabilization(repo, config, selected, persisted)?;
        self.remember_auto_run(persisted.clone());
        Ok(())
    }

    fn update_persisted_stabilization(
        &mut self,
        repo: &crate::repo::Repository,
        config: &crate::config::Config,
        selected: usize,
        persisted: &mut PersistedAutoRun,
    ) -> Result<(), String> {
        persisted.run.current_head_sha =
            crate::git::current_head_sha(&persisted.run.worktree_path, config).ok();
        let snapshot = build_stabilization_snapshot(
            repo,
            &self.sessions[selected],
            Some(&persisted.run),
            config,
        );
        let work = plan_stabilization(&snapshot);
        persisted.run.stabilization_status = Some(match work.blocker {
            crate::auto_flow::stabilization_model::StabilizationBlocker::ReadyForManualMerge
            | crate::auto_flow::stabilization_model::StabilizationBlocker::ReadyToAutoMerge => {
                crate::auto_flow::stabilization_model::StabilizationStatus::Ready
            }
            crate::auto_flow::stabilization_model::StabilizationBlocker::Merged => {
                crate::auto_flow::stabilization_model::StabilizationStatus::Done
            }
            crate::auto_flow::stabilization_model::StabilizationBlocker::CiPending => {
                crate::auto_flow::stabilization_model::StabilizationStatus::Waiting
            }
            crate::auto_flow::stabilization_model::StabilizationBlocker::Escalate
            | crate::auto_flow::stabilization_model::StabilizationBlocker::MergeBlocked => {
                crate::auto_flow::stabilization_model::StabilizationStatus::Escalated
            }
            _ => crate::auto_flow::stabilization_model::StabilizationStatus::Blocked,
        });
        persisted.run.stabilization_blocker = Some(work.blocker);
        persisted.run.stabilization_next_work = Some(work.kind);
        persisted.run.updated_unix_ms = crate::auto_flow::unix_ms();
        crate::observability::with_writable_db(repo, |conn| save_auto_run(conn, persisted))
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
            self.start_delete_worktree_session(context.repo, context.config, path, branch)?;
            self.show_message("merge complete; deleting local session data, worktree, and branch")?;
        } else {
            self.refresh_sessions()?;
            self.show_message("merge complete")?;
        }
        Ok(())
    }
}
