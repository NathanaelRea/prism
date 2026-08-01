use super::*;

pub(super) fn apply_bulk_review_resolution(
    confirmed: bool,
    thread_ids: &[String],
    mut resolve: impl FnMut(&str) -> Result<(), String>,
) -> Result<usize, String> {
    if !confirmed {
        return Ok(0);
    }
    let mut thread_ids = thread_ids.to_vec();
    thread_ids.sort();
    thread_ids.dedup();
    for thread_id in &thread_ids {
        resolve(thread_id)?;
    }
    Ok(thread_ids.len())
}

pub(super) fn unresolved_review_thread_ids(details: &crate::remote::PrDetails) -> Vec<String> {
    crate::review::canonical_review_thread_ids(
        details
            .review_comments
            .iter()
            .filter(|comment| !comment.resolved)
            .map(|comment| comment.thread_id.as_str()),
    )
}

pub(super) fn pr_target_choice_list(origin: &str, upstream: &str) -> crate::view::ChoiceList {
    crate::view::ChoiceList {
        title: "Create Pull Request Target".to_string(),
        choices: vec![
            crate::view::KeyChoice::new("u", format!("upstream ({upstream})")),
            crate::view::KeyChoice::new("o", format!("origin ({origin})")),
        ],
    }
}

pub(super) fn remote_pr_choice_keys() -> Vec<String> {
    ('1'..='9')
        .chain('a'..='z')
        .map(|key| key.to_string())
        .collect()
}

pub(super) fn remote_pr_worktree_branch(head_ref: &str) -> String {
    head_ref.to_string()
}

fn remote_pr_choice_label(summary: &crate::remote::PrSummary) -> String {
    format!(
        "#{}  {}  {} -> {}",
        summary.number, summary.title, summary.head_ref, summary.base_ref
    )
}

fn session_for_remote_action(session: &crate::session::Session) -> crate::session::Session {
    crate::session::Session {
        repo_index: session.repo_index,
        repo_label: session.repo_label.clone(),
        repo_key: session.repo_key,
        path: session.path.clone(),
        incarnation: session.incarnation.clone(),
        path_display: session.path_display.clone(),
        branch: session.branch.clone(),
        prompt_summary: session.prompt_summary.clone(),
        classification: session.classification,
        visibility: session.visibility,
        adopted: session.adopted,
        hidden: session.hidden,
        status_label: session.status_label.clone(),
        agent_state: session.agent_state,
        opencode_status: session.opencode_status.clone(),
        pr: session.pr.clone(),
        wt_columns: session.wt_columns.clone(),
        unseen_comments: session.unseen_comments,
    }
}

fn remote_create_mutation_target(
    guard: &crate::remote::dispatcher::CreateChangeRequestGuard,
) -> crate::tui::RemoteMutationTarget {
    crate::tui::RemoteMutationTarget::Create {
        source_provider: guard.source_repository.provider(),
        source_host: guard.source_repository.host().to_string(),
        source_project: guard.source_repository.project_path().to_string(),
        source_branch: guard.source_branch.clone(),
        expected_head_sha: guard.expected_head_sha.clone(),
        target_provider: Some(guard.target_repository.provider()),
        target_host: guard.target_repository.host().to_string(),
        target_project: guard.target_repository.project_path().to_string(),
        target_branch: guard.target_branch.clone(),
        expected_base_sha: guard.expected_base_sha.clone(),
    }
}

fn remote_push_mutation_target(
    guard: &crate::remote::dispatcher::PushGuard,
) -> crate::tui::RemoteMutationTarget {
    crate::tui::RemoteMutationTarget::Push {
        remote: guard.remote.clone(),
        branch: guard.remote_branch.clone(),
        expected_head_sha: guard.expected_head_sha.clone(),
        repository_provider: Some(guard.repository.provider()),
        repository_host: guard.repository.host().to_string(),
        repository_project: guard.repository.project_path().to_string(),
    }
}

pub(super) fn validate_push_target_after_checks(
    selected_branch: &str,
    current_branch: &str,
    expected: &crate::tui::RemoteMutationTarget,
    current: &crate::tui::RemoteMutationTarget,
) -> Result<(), String> {
    if current_branch != selected_branch {
        return Err(format!(
            "selected branch changed from {selected_branch} to {current_branch} during pre-push checks"
        ));
    }
    if current != expected {
        return Err("push remote, branch, or HEAD changed during pre-push checks".to_string());
    }
    Ok(())
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
        match run_output_allow_failure(
            Command::new(program).args(*args).arg(url),
            ProcessPolicy::LocalMutation,
        ) {
            Ok(output) if output.status.success() => return Ok((*program).to_string()),
            Ok(output) => errors.push(format!("{program}: exited with {}", output.status)),
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
    pub(crate) fn apply_remote_cache_result(&mut self, session_index: usize, cache: PrCache) {
        if let Some(session) = self.sessions.get_mut(session_index) {
            session.pr = cache;
        }
    }

    fn apply_remote_summary_result(
        &mut self,
        session_index: usize,
        summary: crate::remote::PrSummary,
    ) {
        let started_at = std::time::Instant::now();
        self.sessions[session_index]
            .pr
            .begin_summary_poll(started_at);
        apply_pr_summary_poll_result(
            &mut self.sessions[session_index].pr,
            started_at,
            Ok(Some(summary)),
            &crate::util::timestamp_label(),
        );
        self.queue_pr_persistence(session_index, false);
    }

    pub(crate) fn resolve_review_comments(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let context = self
            .selected_worktree_context()
            .ok_or_else(|| "no worktree selected".to_string())?;
        let path = self.sessions[context.session_index].path.clone();
        let summary = self.sessions[context.session_index]
            .pr
            .trusted_summary()?
            .cloned()
            .ok_or_else(|| "pull request summary is unavailable".to_string())?;
        let details = self.sessions[context.session_index]
            .pr
            .trusted_details()?
            .ok_or_else(|| "pull request review details are unavailable".to_string())?;
        let thread_ids = unresolved_review_thread_ids(details);
        if thread_ids.is_empty() {
            return self.show_message("no unresolved review conversations");
        }

        let repo = context.repo;
        let config = context.config;
        let branch = self.sessions[context.session_index].branch.clone();
        let mut cache = self.sessions[context.session_index].pr.clone();
        let worktree = self.sessions[context.session_index]
            .identity_key(&self.repos[self.sessions[context.session_index].repo_index].identity);
        let generation = self
            .worktree_generations
            .get(&worktree)
            .copied()
            .unwrap_or_default();
        let RemoteActionValue::Resolved { cache, count } = self.run_remote_action(
            raw,
            crate::tui::RemoteActionRequest {
                key: TuiJobKey::Worktree(worktree),
                generation,
                name: "prism-resolve-review-threads",
                title: "Resolve Review Conversations",
                message: "Resolving observed review conversations",
                abandon_cancelable: false,
                mutation: Some(crate::tui::RemoteMutationTarget::Resolve {
                    change_request: summary
                        .change_request_identity
                        .clone()
                        .ok_or_else(|| "pull request identity is unavailable".to_string())?,
                    thread_ids: thread_ids.clone(),
                }),
            },
            move || {
                let count = apply_bulk_review_resolution(true, &thread_ids, |thread_id| {
                    crate::remote::dispatcher::resolve_review_thread(
                        &path, &config, &summary, thread_id,
                    )
                })?;
                refresh_pr_cache(&repo, &branch, &mut cache, &path, &config, true)?;
                Ok(RemoteActionValue::Resolved {
                    cache: Box::new(cache),
                    count,
                })
            },
        )?
        else {
            return Err("review resolution returned an unexpected result".to_string());
        };
        self.apply_remote_cache_result(context.session_index, *cache);
        self.show_message(&format!(
            "resolved {count} review conversation{}",
            if count == 1 { "" } else { "s" }
        ))
    }

    pub(crate) fn open_remote_pr_worktree(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let context = self
            .selected_repo_context()
            .ok_or_else(|| "no selected repository".to_string())?;
        let path = context.repo.root.clone();
        let config = context.config.clone();
        let RemoteActionValue::ChangeRequests(mut prs) = self.run_remote_action(
            raw,
            crate::tui::RemoteActionRequest {
                key: TuiJobKey::Repository(self.repos[context.repo_index].identity.clone()),
                generation: self.session_inventory_generation,
                name: "prism-list-change-requests",
                title: "Remote Pull Requests",
                message: "Loading open pull requests",
                abandon_cancelable: true,
                mutation: None,
            },
            move || fetch_pr_summary_index(&path, &config).map(RemoteActionValue::ChangeRequests),
        )?
        else {
            return Err("remote list returned an unexpected result".to_string());
        };
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
            .map(|(summary, key)| crate::view::KeyChoice::new(key, remote_pr_choice_label(summary)))
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

        self.open_repo_pr_worktree(raw, &context, summary)?;
        Ok(())
    }

    pub(crate) fn open_selected_repo_pr_agent(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let context = self
            .selected_repo_context()
            .ok_or_else(|| "no selected repository".to_string())?;
        let summary = self
            .selected_repo_pr_summary()
            .ok_or_else(|| "selected repository has no open pull requests".to_string())?;
        if summary.merged || !summary.state.eq_ignore_ascii_case("OPEN") {
            self.show_message("review blocked: change request lifecycle is unknown or not open")?;
            return Ok(());
        }
        let Some(index) = self.open_repo_pr_worktree(raw, &context, summary)? else {
            return Ok(());
        };
        self.enter_agent_mode_for_index(raw, index)
    }

    fn open_repo_pr_worktree(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
        context: &crate::tui::SelectedRepoContext,
        summary: crate::remote::PrSummary,
    ) -> Result<Option<usize>, String> {
        if let Some(index) = self.existing_pr_worktree_index(context.repo_index, &summary) {
            self.select_worktree(index);
            self.focus_worktrees();
            self.show_message(&format!(
                "selected existing worktree for PR #{}",
                summary.number
            ))?;
            return Ok(Some(index));
        }

        let branch = remote_pr_worktree_branch(&summary.head_ref);
        let path = context.repo.root.clone();
        let config = context.config.clone();
        let job_summary = summary.clone();
        let job_branch = branch.clone();
        let RemoteActionValue::Complete = self.run_remote_action(
            raw,
            crate::tui::RemoteActionRequest {
                key: TuiJobKey::Repository(self.repos[context.repo_index].identity.clone()),
                generation: self.session_inventory_generation,
                name: "prism-fetch-change-request",
                title: "Remote Pull Requests",
                message: &format!("Fetching PR #{}", summary.number),
                abandon_cancelable: true,
                mutation: None,
            },
            move || {
                fetch_pull_request_branch(&path, &config, &job_summary, &job_branch)?;
                Ok(RemoteActionValue::Complete)
            },
        )?
        else {
            return Err("remote fetch returned an unexpected result".to_string());
        };
        self.show_loading_dialog(
            raw,
            "Remote Pull Requests",
            &format!("Opening worktree for PR #{}", summary.number),
        )?;
        let creation = match checkout_worktree_session(&context.repo, &context.config, &branch) {
            Ok(outcome) => outcome,
            Err(error) => {
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
                checkout_worktree_session(&context.repo, &context.config, &branch)?
            }
        };
        if let CreateWorktreeOutcome::CreatedMetadataFailed { error } = creation {
            self.refresh_sessions()?;
            self.show_message(&format!(
                "worktree opened, but restoring Prism metadata failed: {error}"
            ))?;
            return Ok(None);
        }

        self.refresh_sessions()?;
        self.start_tmux_agent_warmup();
        self.start_wt_column_poll();
        self.select_pr_worktree_by_branch(context.repo_index, &branch, Some(summary.clone()));
        self.focus_worktrees();
        self.show_message(&format!("opened worktree for PR #{}", summary.number))?;
        Ok(self.selected_worktree_index())
    }

    fn existing_pr_worktree_index(
        &mut self,
        repo_index: usize,
        summary: &crate::remote::PrSummary,
    ) -> Option<usize> {
        if let Some(index) = self.sessions.iter().position(|session| {
            !session.hidden
                && session.repo_index == repo_index
                && session.pr.summary().is_some_and(|existing| {
                    match (
                        existing.change_request_identity.as_ref(),
                        summary.change_request_identity.as_ref(),
                    ) {
                        (Some(existing), Some(selected)) => existing == selected,
                        (None, None) => existing.number == summary.number,
                        _ => false,
                    }
                })
        }) {
            return Some(index);
        }
        let branch = remote_pr_worktree_branch(&summary.head_ref);
        let index = self.sessions.iter().position(|session| {
            !session.hidden && session.repo_index == repo_index && session.branch == branch
        })?;
        let repo = self
            .repos
            .get(repo_index)
            .map(|managed| managed.repo.clone());
        if repo.is_some() {
            self.apply_remote_summary_result(index, summary.clone());
        }
        Some(index)
    }

    fn select_pr_worktree_by_branch(
        &mut self,
        repo_index: usize,
        branch: &str,
        summary: Option<crate::remote::PrSummary>,
    ) {
        if let Some(index) = self
            .sessions
            .iter()
            .position(|session| session.repo_index == repo_index && session.branch == branch)
        {
            let repo = self
                .repos
                .get(repo_index)
                .map(|managed| managed.repo.clone());
            if let Some(summary) = summary
                && repo.is_some()
            {
                self.apply_remote_summary_result(index, summary);
            }
            if !self.visible_session_indices().contains(&index) {
                self.worktree_filter.clear();
            }
            self.select_worktree(index);
        }
    }

    pub(crate) fn submit_selected_repo_pr_review(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let context = self
            .selected_repo_context()
            .ok_or_else(|| "no selected repository".to_string())?;
        let summary = self
            .selected_repo_pr_summary()
            .ok_or_else(|| "selected repository has no open pull requests".to_string())?;
        if summary.merged || !summary.state.eq_ignore_ascii_case("OPEN") {
            self.show_message("review blocked: change request lifecycle is unknown or not open")?;
            return Ok(());
        }
        let Some(choice) = self.prompt_choice_dialog(
            raw,
            crate::view::ChoiceList {
                title: format!("Review PR #{}", summary.number),
                choices: vec![
                    crate::view::KeyChoice::new("a", "approve"),
                    crate::view::KeyChoice::new("c", "comment"),
                    crate::view::KeyChoice::new("r", "request changes"),
                ],
            },
        )?
        else {
            return Ok(());
        };
        let (flag, label, body_required) = match choice.as_str() {
            "a" => ("--approve", "approved", false),
            "c" => ("--comment", "commented on", true),
            "r" => ("--request-changes", "requested changes on", true),
            _ => return Ok(()),
        };
        let prompt = if body_required {
            "Review body: "
        } else {
            "Review body (optional): "
        };
        let Some(body) = self.prompt_line_dialog(raw, "Submit Review", prompt, "")? else {
            return Ok(());
        };
        if body_required && body.trim().is_empty() {
            self.show_message("review body is required for this review type")?;
            return Ok(());
        }

        let path = context.repo.root.clone();
        let config = context.config.clone();
        let body = body.trim().to_string();
        let selected_summary = summary.clone();
        let prior_review_ids = self
            .sessions
            .iter()
            .filter(|session| {
                session.pr.summary().is_some_and(|existing| {
                    existing.change_request_identity == summary.change_request_identity
                })
            })
            .filter_map(|session| session.pr.trusted_details().ok().flatten())
            .flat_map(|details| details.reviews.iter().map(|review| review.id.clone()))
            .collect();
        let RemoteActionValue::Complete = self.run_remote_action(
            raw,
            crate::tui::RemoteActionRequest {
                key: TuiJobKey::Repository(self.repos[context.repo_index].identity.clone()),
                generation: self.session_inventory_generation,
                name: "prism-submit-review",
                title: "Submit Review",
                message: &format!("Submitting review for PR #{}", summary.number),
                abandon_cancelable: false,
                mutation: Some(crate::tui::RemoteMutationTarget::Review {
                    change_request: summary
                        .change_request_identity
                        .clone()
                        .ok_or_else(|| "pull request identity is unavailable".to_string())?,
                    expected_state: match flag {
                        "--approve" => "APPROVED",
                        "--comment" => "COMMENTED",
                        "--request-changes" => "CHANGES_REQUESTED",
                        _ => unreachable!(),
                    }
                    .to_string(),
                    expected_body: body.clone(),
                    prior_review_ids,
                }),
            },
            move || {
                crate::remote::dispatcher::submit_review(
                    &path,
                    &config,
                    &selected_summary,
                    flag,
                    &body,
                )?;
                Ok(RemoteActionValue::Complete)
            },
        )?
        else {
            return Err("review submission returned an unexpected result".to_string());
        };
        self.show_message(&format!("{label} PR #{}", summary.number))
    }

    pub(crate) fn start_review_fix(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        self.refresh_selected_pr_for_remote_action(
            raw,
            "Review Fix Prompt",
            "Refreshing pull request review details",
        )?;
        self.send_review_fix_prompt(false)
    }

    pub(super) fn send_review_fix_prompt(&mut self, refresh: bool) -> Result<(), String> {
        let Some(context) = self.selected_worktree_context() else {
            return Ok(());
        };
        let selected = context.session_index;
        if refresh {
            let session = &mut self.sessions[selected];
            refresh_pr_cache(
                &context.repo,
                &session.branch,
                &mut session.pr,
                &session.path,
                &context.config,
                true,
            )?;
        }
        self.supersede_pr_persistence(selected, true);
        let repair = crate::auto_flow::stabilization_execute::prepare_standalone_repair(
            &self.sessions[selected],
            &context.config,
            crate::auto_flow::stabilization_model::RepairKind::Review,
        )?;
        self.start_managed_repair(selected, &context.repo, &context.config, repair)?;
        self.show_message("started managed review repair; commit will wait for guarded push")?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn start_review_fix_for_test(&mut self) -> Result<(), String> {
        self.send_review_fix_prompt(true)
    }

    pub(crate) fn start_ci_fix(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        if self.selected_worktree_context().is_none() {
            return Ok(());
        }
        self.refresh_selected_pr_for_remote_action(
            raw,
            "CI Failure Prompt",
            "Refreshing pull request CI details",
        )?;
        self.send_ci_fix_prompt(false)
    }

    pub(super) fn send_ci_fix_prompt(&mut self, refresh: bool) -> Result<(), String> {
        let Some(context) = self.selected_worktree_context() else {
            return Ok(());
        };
        let selected = context.session_index;
        if refresh {
            let session = &mut self.sessions[selected];
            refresh_pr_cache(
                &context.repo,
                &session.branch,
                &mut session.pr,
                &session.path,
                &context.config,
                true,
            )?;
        }
        self.supersede_pr_persistence(selected, true);
        let repair = crate::auto_flow::stabilization_execute::prepare_standalone_repair(
            &self.sessions[selected],
            &context.config,
            crate::auto_flow::stabilization_model::RepairKind::Ci,
        )?;
        self.start_managed_repair(selected, &context.repo, &context.config, repair)?;
        self.show_message("started managed CI repair; commit will wait for guarded push")?;
        Ok(())
    }

    fn start_managed_repair(
        &mut self,
        selected: usize,
        repo: &crate::repo::Repository,
        config: &crate::config::Config,
        repair: crate::auto_flow::stabilization_execute::StandaloneRepair,
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
            )?
            .with_harness(
                config.default_harness.clone(),
                config.harness_adapter(&config.default_harness)?,
            );
            let mut run = launch.create_run();
            run.steps.clear();
            run.run.pr_number = self.sessions[selected]
                .pr
                .summary()
                .map(|summary| summary.number);
            run.run.pr_url = self.sessions[selected]
                .pr
                .summary()
                .map(|summary| summary.url.clone());
            run.run.current_head_sha = crate::git::current_head_sha(&session_path, config).ok();
            run
        };

        crate::observability::with_writable_db(repo, |conn| {
            crate::auto_flow::stabilization_execute::queue_standalone_repair(
                conn,
                &mut persisted,
                repair,
            )?;
            Ok(())
        })?;
        self.remember_auto_run(persisted.clone());
        self.selected_auto_run = Some(persisted.run.id.clone());
        #[cfg(test)]
        if self.prompt_submissions.is_some() {
            return Ok(());
        }
        self.spawn_auto_run_executor(repo.clone(), config.clone(), persisted)?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn start_ci_fix_for_test(&mut self) -> Result<(), String> {
        self.send_ci_fix_prompt(true)
    }

    fn refresh_selected_pr_for_remote_action(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
        title: &str,
        message: &str,
    ) -> Result<(), String> {
        let context = self
            .selected_worktree_context()
            .ok_or_else(|| "no worktree selected".to_string())?;
        let selected = context.session_index;
        let repo = context.repo;
        let config = context.config;
        let branch = self.sessions[selected].branch.clone();
        let path = self.sessions[selected].path.clone();
        let mut cache = self.sessions[selected].pr.clone();
        let worktree = self.sessions[selected]
            .identity_key(&self.repos[self.sessions[selected].repo_index].identity);
        let generation = self
            .worktree_generations
            .get(&worktree)
            .copied()
            .unwrap_or_default();
        let RemoteActionValue::Cache(cache) = self.run_remote_action(
            raw,
            crate::tui::RemoteActionRequest {
                key: TuiJobKey::Worktree(worktree),
                generation,
                name: "prism-refresh-change-request-action",
                title,
                message,
                abandon_cancelable: true,
                mutation: None,
            },
            move || {
                refresh_pr_cache(&repo, &branch, &mut cache, &path, &config, true)?;
                Ok(RemoteActionValue::Cache(Box::new(cache)))
            },
        )?
        else {
            return Err("pull request refresh returned an unexpected result".to_string());
        };
        self.apply_remote_cache_result(selected, *cache);
        Ok(())
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
        if !self.sessions[selected].pr.has_summary() {
            let repo = context.repo.clone();
            let config = context.config.clone();
            let branch = self.sessions[selected].branch.clone();
            let path = self.sessions[selected].path.clone();
            let mut cache = self.sessions[selected].pr.clone();
            let repo_index = self.sessions[selected].repo_index;
            let worktree = self.sessions[selected].identity_key(&self.repos[repo_index].identity);
            let generation = self
                .worktree_generations
                .get(&worktree)
                .copied()
                .unwrap_or_default();
            let RemoteActionValue::Cache(cache) = self.run_remote_action(
                raw,
                crate::tui::RemoteActionRequest {
                    key: TuiJobKey::Worktree(worktree),
                    generation,
                    name: "prism-refresh-change-request",
                    title: "Open Pull Request",
                    message: "Refreshing pull request",
                    abandon_cancelable: true,
                    mutation: None,
                },
                move || {
                    refresh_pr_cache(&repo, &branch, &mut cache, &path, &config, false)?;
                    Ok(RemoteActionValue::Cache(Box::new(cache)))
                },
            )?
            else {
                return Err("pull request refresh returned an unexpected result".to_string());
            };
            self.apply_remote_cache_result(selected, *cache);
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
        if self.resolve_blocking_review_threads(raw, selected, &context.repo, &context.config)? {
            return Ok(());
        }

        let repo = context.repo.clone();
        let config = context.config.clone();
        let mut cache = self.sessions[selected].pr.clone();
        let worktree = self.sessions[selected]
            .identity_key(&self.repos[self.sessions[selected].repo_index].identity);
        let generation = self
            .worktree_generations
            .get(&worktree)
            .copied()
            .unwrap_or_default();
        let job_path = path.clone();
        let job_branch = branch.clone();
        let expected_push_guard =
            crate::remote::dispatcher::prepare_push(&path, &context.config, &branch)?;
        let expected_push_target = remote_push_mutation_target(&expected_push_guard);
        let reconciliation_target = expected_push_target.clone();
        let RemoteActionValue::PushPrepared(prepared) = self.run_remote_action(
            raw,
            crate::tui::RemoteActionRequest {
                key: TuiJobKey::Worktree(worktree.clone()),
                generation,
                name: "prism-push-branch",
                title: "Push Branch",
                message: "Pushing selected branch",
                abandon_cancelable: false,
                mutation: Some(reconciliation_target),
            },
            move || {
                run_pre_push_checks(&config, &job_path)?;
                let current_branch = crate::git::current_branch_name(&job_path, &config)?
                    .ok_or_else(|| "cannot push detached HEAD".to_string())?;
                let current_push_guard =
                    crate::remote::dispatcher::prepare_push(&job_path, &config, &job_branch)?;
                let current_push_target = remote_push_mutation_target(&current_push_guard);
                validate_push_target_after_checks(
                    &job_branch,
                    &current_branch,
                    &expected_push_target,
                    &current_push_target,
                )?;
                push_branch(
                    &config,
                    &job_path,
                    &job_branch,
                    current_push_guard.set_upstream,
                )?;
                let pushed_source_guard =
                    crate::remote::dispatcher::prepare_push(&job_path, &config, &job_branch)?;
                if !crate::remote::dispatcher::same_push_target(
                    &current_push_guard,
                    &pushed_source_guard,
                ) {
                    return Err("push destination changed while pushing".to_string());
                }
                refresh_pr_cache(&repo, &job_branch, &mut cache, &job_path, &config, true)?;
                let (origin_repository, upstream_repository) = if cache.has_summary() {
                    (None, None)
                } else {
                    let (origin, upstream) =
                        crate::remote::dispatcher::create_change_request_targets(
                            &job_path, &config,
                        )?;
                    (Some(origin), upstream)
                };
                let push_guard = origin_repository
                    .as_ref()
                    .map(|_| pushed_source_guard.clone());
                Ok(RemoteActionValue::PushPrepared(Box::new(
                    RemotePushPrepared {
                        cache,
                        origin_repository,
                        upstream_repository,
                        push_guard,
                    },
                )))
            },
        )?
        else {
            return Err("push returned an unexpected result".to_string());
        };
        self.apply_remote_cache_result(selected, prepared.cache);
        if !self.sessions[selected].pr.has_summary() {
            let source_push = prepared
                .push_guard
                .ok_or_else(|| "change request push source is unavailable".to_string())?;
            let target_repository = match (prepared.origin_repository, prepared.upstream_repository)
            {
                (Some(origin), Some(upstream)) => {
                    let origin_project = origin.project_path();
                    let upstream_project = upstream.project_path();
                    let Some(choice) = self.prompt_choice_dialog(
                        raw,
                        pr_target_choice_list(origin_project, upstream_project),
                    )?
                    else {
                        return Ok(());
                    };
                    match choice.as_str() {
                        "u" => upstream,
                        "o" => origin,
                        _ => return Ok(()),
                    }
                }
                (Some(origin), None) => origin,
                _ => return Err("change request target is unavailable".to_string()),
            };
            let Some(pr_body) = self.prompt_pr_description(raw)? else {
                return Ok(());
            };
            let repo = context.repo;
            let config = context.config;
            let branch = self.sessions[selected].branch.clone();
            let path = self.sessions[selected].path.clone();
            let mut cache = self.sessions[selected].pr.clone();
            let prepare_path = path.clone();
            let prepare_config = config.clone();
            let RemoteActionValue::CreatePrepared(create_guard) = self.run_remote_action(
                raw,
                crate::tui::RemoteActionRequest {
                    key: TuiJobKey::Worktree(worktree.clone()),
                    generation,
                    name: "prism-prepare-change-request",
                    title: "Create Pull Request",
                    message: "Running pre-PR checks",
                    abandon_cancelable: true,
                    mutation: None,
                },
                move || {
                    run_pre_pr_checks(&prepare_config, &prepare_path)?;
                    let guard = crate::remote::dispatcher::prepare_create_change_request(
                        &prepare_path,
                        &prepare_config,
                        &branch,
                        &target_repository,
                        &source_push,
                    )?;
                    Ok(RemoteActionValue::CreatePrepared(Box::new(guard)))
                },
            )?
            else {
                return Err("pull request preparation returned an unexpected result".to_string());
            };
            let create_mutation = remote_create_mutation_target(&create_guard);
            let RemoteActionValue::Cache(cache) = self.run_remote_action(
                raw,
                crate::tui::RemoteActionRequest {
                    key: TuiJobKey::Worktree(worktree),
                    generation,
                    name: "prism-create-change-request",
                    title: "Create Pull Request",
                    message: "Creating pull request",
                    abandon_cancelable: false,
                    mutation: Some(create_mutation),
                },
                move || {
                    create_pull_request(
                        &repo,
                        &config,
                        &path,
                        &pr_body,
                        &create_guard,
                        &mut cache,
                    )?;
                    Ok(RemoteActionValue::Cache(Box::new(cache)))
                },
            )?
            else {
                return Err("pull request creation returned an unexpected result".to_string());
            };
            self.apply_remote_cache_result(selected, *cache);
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
        let Some(run_id) = self.active_auto_runs.get(&path).cloned() else {
            return Ok(false);
        };

        let repo = repo.clone();
        let config = config.clone();
        let mut cache = self.sessions[selected].pr.clone();
        let worktree = self.sessions[selected]
            .identity_key(&self.repos[self.sessions[selected].repo_index].identity);
        let generation = self
            .worktree_generations
            .get(&worktree)
            .copied()
            .unwrap_or_default();
        let RemoteActionValue::GuardedPush {
            persisted,
            cache,
            progress,
        } = self.run_remote_action(
            raw,
            crate::tui::RemoteActionRequest {
                key: TuiJobKey::Worktree(worktree),
                generation,
                name: "prism-guarded-push",
                title: "Guarded Push",
                message: "Reobserving guarded repair push",
                abandon_cancelable: false,
                mutation: Some(remote_push_mutation_target(
                    &crate::remote::dispatcher::prepare_push(
                        &self.sessions[selected].path,
                        &config,
                        &self.sessions[selected].branch,
                    )?,
                )),
            },
            move || {
                let mut persisted = crate::observability::with_writable_db(&repo, |conn| {
                    load_auto_run(conn, &run_id)
                })?
                .ok_or_else(|| format!("active Auto Flow run not found: {run_id}"))?;
                let progress = if persisted.run.pending_push.is_some() {
                    Some(crate::observability::with_writable_db(&repo, |conn| {
                        crate::auto_flow::stabilization_execute::progress_pending_push(
                            conn,
                            &repo,
                            &config,
                            &mut persisted,
                            &mut cache,
                            || Ok(()),
                        )
                    })?)
                } else {
                    None
                };
                Ok(RemoteActionValue::GuardedPush {
                    persisted: Box::new(persisted),
                    cache: Box::new(cache),
                    progress,
                })
            },
        )?
        else {
            return Err("guarded push returned an unexpected result".to_string());
        };
        let Some(progress) = progress else {
            return Ok(false);
        };
        self.apply_remote_cache_result(selected, *cache);
        self.remember_auto_run(*persisted);
        match progress {
            crate::auto_flow::stabilization_execute::GuardedPushProgress::AlreadySatisfied => {
                self.show_message(
                    "guarded repair push already satisfied; reobserved PR Stabilization",
                )?;
            }
            crate::auto_flow::stabilization_execute::GuardedPushProgress::Invalidated {
                reason,
            } => {
                self.show_message(&format!("guarded repair push invalidated: {reason}"))?;
            }
            crate::auto_flow::stabilization_execute::GuardedPushProgress::Pushed => {
                self.show_message("guarded repair pushed; reobserved PR Stabilization")?;
            }
        }
        Ok(true)
    }

    pub(super) fn resolve_blocking_review_threads(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
        selected: usize,
        repo: &crate::repo::Repository,
        config: &crate::config::Config,
    ) -> Result<bool, String> {
        let path = self.sessions[selected].path.clone();
        let Some(run_id) = self.active_auto_runs.get(&path).cloned() else {
            return Ok(false);
        };
        let repo = repo.clone();
        let config = config.clone();
        let branch = self.sessions[selected].branch.clone();
        let mut cache = self.sessions[selected].pr.clone();
        let worktree = self.sessions[selected]
            .identity_key(&self.repos[self.sessions[selected].repo_index].identity);
        let generation = self
            .worktree_generations
            .get(&worktree)
            .copied()
            .unwrap_or_default();
        let prepared = self.run_remote_action(
            raw,
            crate::tui::RemoteActionRequest {
                key: TuiJobKey::Worktree(worktree.clone()),
                generation,
                name: "prism-prepare-review-resolution",
                title: "Review Feedback",
                message: "Refreshing review conversations",
                abandon_cancelable: true,
                mutation: None,
            },
            {
                let repo = repo.clone();
                let config = config.clone();
                let path = path.clone();
                let branch = branch.clone();
                move || {
                    let mut persisted =
                        crate::observability::with_writable_db(&repo, |conn| {
                            load_auto_run(conn, &run_id)
                        })?
                        .ok_or_else(|| format!("active Auto Flow run not found: {run_id}"))?;
                    if persisted.run.stabilization_blocker
                        != Some(
                            crate::auto_flow::stabilization_model::StabilizationBlocker::ReviewFeedbackFound,
                        )
                    {
                        return Ok(RemoteActionValue::NotApplicable);
                    }
                    refresh_pr_cache(
                        &repo, &branch, &mut cache, &path, &config, true,
                    )?;
                    let feedback =
                        crate::auto_flow::stabilization_observe::stabilization_review_feedback(
                            cache.trusted_details()?.ok_or_else(|| {
                                "pull request review details are unavailable".to_string()
                            })?,
                            persisted.run.review_baseline_json.as_deref(),
                        );
                    let thread_ids = crate::review::review_thread_ids(&feedback);
                    let summary = cache
                        .trusted_summary()?
                        .cloned()
                        .ok_or_else(|| "pull request summary is unavailable".to_string())?;
                    if thread_ids.is_empty() {
                        crate::observability::with_writable_db(&repo, |conn| {
                            crate::auto_flow::stabilization_execute::observe_plan_and_save(
                                conn,
                                &repo,
                                &config,
                                &mut persisted,
                            )
                        })?;
                    }
                    Ok(RemoteActionValue::ReviewResolutionPrepared {
                        persisted: Box::new(persisted),
                        cache: Box::new(cache),
                        thread_ids,
                        summary: Box::new(summary),
                    })
                }
            },
        )?;
        let RemoteActionValue::ReviewResolutionPrepared {
            persisted,
            cache,
            thread_ids,
            summary,
        } = prepared
        else {
            return if matches!(prepared, RemoteActionValue::NotApplicable) {
                Ok(false)
            } else {
                Err("review resolution preparation returned an unexpected result".to_string())
            };
        };
        self.apply_remote_cache_result(selected, *cache);
        self.remember_auto_run((*persisted).clone());
        if thread_ids.is_empty() {
            self.show_message(
                "no unresolved actionable review conversations; reobserved PR Stabilization",
            )?;
            return Ok(true);
        }
        if self.remote_support_for_action(GitAction::ResolveAllComments, Some(summary.as_ref()))
            != Some(crate::remote::SupportLevel::Supported)
        {
            self.show_message("review conversation resolution is unavailable for this provider")?;
            return Ok(true);
        }

        let count = thread_ids.len();
        let confirmed = self.confirm_action_dialog(
            raw,
            "Resolve Review Conversations",
            &format!("Mark all {count} unresolved review conversation(s) as resolved?"),
            false,
        )?;
        if !confirmed {
            self.show_message("review conversations left unresolved")?;
            return Ok(true);
        }

        let mut cache = self.sessions[selected].pr.clone();
        let RemoteActionValue::ReviewResolutionFinished {
            persisted,
            cache,
            resolved,
        } = self.run_remote_action(
            raw,
            crate::tui::RemoteActionRequest {
                key: TuiJobKey::Worktree(worktree),
                generation,
                name: "prism-resolve-blocking-review-threads",
                title: "Resolve Review Conversations",
                message: "Resolving review conversations",
                abandon_cancelable: false,
                mutation: Some(crate::tui::RemoteMutationTarget::Resolve {
                    change_request: summary
                        .change_request_identity
                        .clone()
                        .ok_or_else(|| "pull request identity is unavailable".to_string())?,
                    thread_ids: thread_ids.clone(),
                }),
            },
            move || {
                let mut persisted = persisted;
                let resolved = apply_bulk_review_resolution(true, &thread_ids, |thread_id| {
                    crate::remote::dispatcher::resolve_review_thread(
                        &path, &config, &summary, thread_id,
                    )
                })?;
                refresh_pr_cache(&repo, &branch, &mut cache, &path, &config, true)?;
                crate::observability::with_writable_db(&repo, |conn| {
                    crate::auto_flow::stabilization_execute::observe_plan_and_save(
                        conn,
                        &repo,
                        &config,
                        &mut persisted,
                    )
                })?;
                Ok(RemoteActionValue::ReviewResolutionFinished {
                    persisted,
                    cache: Box::new(cache),
                    resolved,
                })
            },
        )?
        else {
            return Err("review resolution returned an unexpected result".to_string());
        };
        self.apply_remote_cache_result(selected, *cache);
        self.remember_auto_run(*persisted);
        self.show_message(&format!(
            "resolved {resolved} review conversation(s); reobserved PR Stabilization"
        ))?;
        Ok(true)
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
        let worktree = self.sessions[selected]
            .identity_key(&self.repos[self.sessions[selected].repo_index].identity);
        let generation = self
            .worktree_generations
            .get(&worktree)
            .copied()
            .unwrap_or_default();
        let repo = context.repo.clone();
        let config = context.config.clone();
        let mut session = Box::new(session_for_remote_action(&self.sessions[selected]));
        let authorization_path = path.clone();
        let initial = self.run_remote_action(
            raw,
            crate::tui::RemoteActionRequest {
                key: TuiJobKey::Worktree(worktree.clone()),
                generation,
                name: "prism-authorize-merge",
                title: "Merge Pull Request",
                message: "Checking pull request gates",
                abandon_cancelable: true,
                mutation: None,
            },
            move || {
                if let Err(error) = run_pre_push_checks(&config, &authorization_path) {
                    return Ok(RemoteActionValue::MergeExecution {
                        session,
                        result: Err(error),
                    });
                }
                let authorization =
                    crate::auto_flow::stabilization_execute::observe_manual_merge_authorization(
                        &repo,
                        &config,
                        &mut session,
                    );
                Ok(RemoteActionValue::MergeAuthorization {
                    session,
                    authorization: Box::new(authorization),
                })
            },
        );
        let (session, initial_authorization) = match initial {
            Ok(RemoteActionValue::MergeAuthorization {
                session,
                authorization,
            }) => (session, authorization),
            Ok(RemoteActionValue::MergeExecution { session, result }) => {
                self.apply_remote_cache_result(selected, session.pr);
                return result.map(|_| ());
            }
            Ok(_) => return Err("merge authorization returned an unexpected result".to_string()),
            Err(error) => return Err(error),
        };
        let (initially_observed_pr_number, review_thread_ids) = match initial_authorization.as_ref() {
            crate::auto_flow::stabilization_execute::MergeAuthorization::Authorized(token) => {
                (token.pr_number(), Vec::new())
            }
            crate::auto_flow::stabilization_execute::MergeAuthorization::ReviewResolutionRequired {
                candidate,
                thread_ids,
                ..
            } => {
                (candidate.pr_number(), thread_ids.clone())
            }
            crate::auto_flow::stabilization_execute::MergeAuthorization::Blocked(state) => {
                self.apply_remote_cache_result(selected, session.pr.clone());
                if state.blocker
                    == crate::auto_flow::stabilization_model::StabilizationBlocker::Merged
                {
                    self.show_message("pull request is already merged")?;
                } else if state.blocker
                    == crate::auto_flow::stabilization_model::StabilizationBlocker::NeedsPullRequest
                {
                    self.show_message("no pull request found for selected branch")?;
                } else {
                    self.show_message(&format!("merge blocked: {}", state.reason))?;
                }
                return Ok(());
            }
        };
        let initially_observed_summary = session
            .pr
            .trusted_summary()?
            .cloned()
            .ok_or_else(|| "pull request summary is unavailable".to_string())?;
        if !review_thread_ids.is_empty()
            && self.remote_support_for_action(
                GitAction::ResolveAllComments,
                Some(&initially_observed_summary),
            ) != Some(crate::remote::SupportLevel::Supported)
        {
            self.apply_remote_cache_result(selected, session.pr);
            self.show_message(
                "merge requires review conversation resolution, which is unavailable",
            )?;
            return Ok(());
        }
        let repo = context.repo.clone();
        let config = context.config.clone();
        let job_path = session.path.clone();
        let job_branch = branch.clone();
        let message = if review_thread_ids.is_empty() {
            format!("Merging PR #{initially_observed_pr_number}")
        } else {
            format!("Resolving conversations and merging PR #{initially_observed_pr_number}")
        };
        let RemoteActionValue::MergeExecution { session, result } = self.run_remote_action(
            raw,
            crate::tui::RemoteActionRequest {
                key: TuiJobKey::Worktree(worktree),
                generation,
                name: "prism-merge-change-request",
                title: "Merge Pull Request",
                message: &message,
                abandon_cancelable: false,
                mutation: Some(crate::tui::RemoteMutationTarget::Merge {
                    change_request: initially_observed_summary
                        .change_request_identity
                        .clone()
                        .ok_or_else(|| "pull request identity is unavailable".to_string())?,
                    expected_head_sha: initially_observed_summary.head_sha.clone(),
                }),
            },
            move || {
                let mut session = session;
                let result = (|| {
                    let execution = if review_thread_ids.is_empty() {
                        crate::auto_flow::stabilization_execute::reobserve_and_execute_manual_merge(
                            &repo,
                            &config,
                            &mut session,
                            *initial_authorization,
                        )?
                    } else {
                        apply_bulk_review_resolution(true, &review_thread_ids, |thread_id| {
                            crate::remote::dispatcher::resolve_review_thread(
                                &job_path,
                                &config,
                                &initially_observed_summary,
                                thread_id,
                            )
                        })?;
                        crate::auto_flow::stabilization_execute::reobserve_and_execute_manual_merge(
                            &repo,
                            &config,
                            &mut session,
                            *initial_authorization,
                        )?
                    };
                    let verification = match &execution {
                        crate::auto_flow::stabilization_execute::ManualMergeExecution::Merged {
                            result,
                        } => {
                            crate::remote::dispatcher::record_change_request_summary(
                                &repo,
                                &job_branch,
                                &mut session.pr,
                                result.summary.clone(),
                            )?;
                            let verification = wait_for_pr_merged(
                                &job_path,
                                &result.summary.change_request,
                                &config,
                            )
                            .and_then(|summary| {
                                let merged = summary.lifecycle
                                    == crate::remote::LifecycleState::Merged;
                                crate::remote::dispatcher::record_change_request_summary(
                                    &repo,
                                    &job_branch,
                                    &mut session.pr,
                                    summary,
                                )?;
                                Ok(merged)
                            });
                            Some(verification)
                        }
                        crate::auto_flow::stabilization_execute::ManualMergeExecution::Pending {
                            result,
                        } => {
                            crate::remote::dispatcher::record_change_request_summary(
                                &repo,
                                &job_branch,
                                &mut session.pr,
                                result.summary.clone(),
                            )?;
                            None
                        }
                        crate::auto_flow::stabilization_execute::ManualMergeExecution::Uncertain {
                            result,
                        } => {
                            crate::remote::dispatcher::record_change_request_summary(
                                &repo,
                                &job_branch,
                                &mut session.pr,
                                result.summary.clone(),
                            )?;
                            None
                        }
                        crate::auto_flow::stabilization_execute::ManualMergeExecution::Blocked(
                            _,
                        ) => None,
                    };
                    Ok(RemoteMergeOutcome {
                        execution,
                        verification,
                    })
                })();
                Ok(RemoteActionValue::MergeExecution { session, result })
            },
        )?
        else {
            return Err("merge returned an unexpected result".to_string());
        };
        self.apply_remote_cache_result(selected, session.pr);
        let outcome = result?;
        match outcome.execution {
            crate::auto_flow::stabilization_execute::ManualMergeExecution::Merged { .. } => {}
            crate::auto_flow::stabilization_execute::ManualMergeExecution::Pending { result } => {
                self.refresh_sessions_after_tmux()?;
                self.show_message(&format!(
                    "merge accepted and pending (provider state: {})",
                    result.native_state
                ))?;
                return Ok(());
            }
            crate::auto_flow::stabilization_execute::ManualMergeExecution::Uncertain { result } => {
                self.refresh_sessions_after_tmux()?;
                self.show_message(&format!(
                    "merge outcome uncertain; refresh to reconcile (provider state: {})",
                    result.native_state
                ))?;
                return Ok(());
            }
            crate::auto_flow::stabilization_execute::ManualMergeExecution::Blocked(state) => {
                self.show_message(&format!(
                    "merge blocked after pre-push checks: {}",
                    state.reason
                ))?;
                return Ok(());
            }
        }
        let merged = match outcome
            .verification
            .expect("merged result has verification")
        {
            Ok(merged) => merged,
            Err(error) => {
                self.refresh_sessions_after_tmux()?;
                self.show_message(&format!(
                    "merge complete; could not verify PR merged: {error}"
                ))?;
                return Ok(());
            }
        };
        if !merged {
            self.refresh_sessions_after_tmux()?;
            self.show_message("merge complete; GitHub has not marked the PR merged yet")?;
            return Ok(());
        }
        let path_display = self.sessions[selected].path_display.clone();
        let warnings = self.sessions[selected].deletion_warnings();
        if self.confirm_delete_dialog(raw, &branch, &path_display, &warnings, true)? {
            self.start_delete_worktree_session(context.repo, context.config, path, branch)?;
            self.show_message("merge complete; deleting local session data, worktree, and branch")?;
        } else {
            self.refresh_sessions_after_tmux()?;
            self.show_message("merge complete")?;
        }
        Ok(())
    }
}
