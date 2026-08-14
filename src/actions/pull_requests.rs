use super::*;
use crate::workflow::remote_operation;

type RemoteJobContext = crate::tui_jobs::JobContext<TuiJobKind, TuiJobKey, TuiJobPayload>;

fn report_remote_wait(
    context: &RemoteJobContext,
    wait: crate::remote::request_coordinator::RemoteWait,
) {
    let _ = context.send(TuiJobPayload::RemoteActionProgress {
        id: context.id(),
        message: wait.summary.clone(),
    });
}

#[cfg(test)]
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
    let mut ids = details
        .review_comments
        .iter()
        .filter(|comment| !comment.resolved)
        .map(|comment| comment.thread_id.trim())
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    ids.sort();
    ids.dedup();
    ids
}

pub(super) fn resolve_review_request_id(
    operation: &remote_operation::RemoteMutationOperation,
    subject: &str,
) -> Result<String, String> {
    let mut canonical_operation = operation.clone();
    let remote_operation::RemoteMutationOperation::TuiResolveReviewThreads(payload) =
        &mut canonical_operation
    else {
        return Err("review resolution request ID requires a resolve operation".to_string());
    };
    payload.thread_ids.sort();
    payload.thread_ids.dedup();
    let number = payload.summary.number;
    let head_sha = payload.summary.head_sha.clone();
    let bytes = serde_json::to_vec(&(&canonical_operation, subject))
        .map_err(|error| format!("encode review resolution request identity: {error}"))?;
    use sha2::Digest as _;
    Ok(format!(
        "resolve:{number}:{head_sha}:{:x}",
        sha2::Sha256::digest(bytes)
    ))
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

pub(super) fn open_url_in_browser(url: &str) -> Result<(), String> {
    run_browser_opener(
        crate::platform::browser_candidates(crate::platform::current_os()),
        url,
    )
    .map(|_| ())
}

pub(super) fn open_http_url_in_browser(url: &str) -> Result<(), String> {
    let scheme = url.split_once(':').map(|(scheme, _)| scheme);
    if !scheme.is_some_and(|scheme| {
        scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
    }) {
        return Err("development URL must use http or https".to_string());
    }
    run_browser_opener_private(
        crate::platform::browser_candidates(crate::platform::current_os()),
        url,
    )
    .map(|_| ())
}

fn run_browser_opener_private(
    candidates: &[crate::platform::CommandCandidate<'_>],
    url: &str,
) -> Result<String, String> {
    for candidate in candidates {
        if !command_exists(candidate.program) {
            continue;
        }
        let mut command = Command::new(candidate.program);
        command
            .args(candidate.args)
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        match command.spawn() {
            Ok(mut child) => {
                let program = candidate.program.to_string();
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                return Ok(program);
            }
            Err(_) => continue,
        }
    }
    Err("no usable browser opener found".to_string())
}

pub(super) fn run_browser_opener(
    candidates: &[crate::platform::CommandCandidate<'_>],
    url: &str,
) -> Result<String, String> {
    let mut errors = Vec::new();
    for candidate in candidates {
        if !command_exists(candidate.program) {
            continue;
        }
        match run_output_allow_failure(
            Command::new(candidate.program)
                .args(candidate.args)
                .arg(url),
            ProcessPolicy::LocalMutation,
        ) {
            Ok(output) if output.status.success() => return Ok(candidate.program.to_string()),
            Ok(output) => errors.push(format!(
                "{}: exited with {}",
                candidate.program, output.status
            )),
            Err(error) => errors.push(format!("{}: {error}", candidate.program)),
        }
    }
    if errors.is_empty() {
        let names = candidates
            .iter()
            .map(|candidate| candidate.program)
            .collect::<Vec<_>>()
            .join(", ");
        Err(format!("no browser opener found; tried {names}"))
    } else {
        Err(format!("browser open failed: {}", errors.join("; ")))
    }
}

impl Tui {
    pub(crate) fn apply_remote_cache_result(&mut self, session_index: usize, cache: PrCache) {
        let remote_update = self.sessions.get(session_index).is_some_and(|session| {
            session.pr.summary() != cache.summary() || session.pr.details() != cache.details()
        });
        if let Some(session) = self.sessions.get_mut(session_index) {
            session.pr = cache;
        }
        if remote_update {
            self.queue_pr_persistence(session_index, true, true);
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
        self.queue_pr_persistence(session_index, false, true);
    }

    pub(crate) fn merge_selected_change_request(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let context = self
            .selected_worktree_context()
            .ok_or_else(|| "no worktree selected".to_string())?;
        let selected = context.session_index;
        let path = self.sessions[selected].path.clone();
        let summary = self.sessions[selected]
            .pr
            .trusted_summary()?
            .cloned()
            .ok_or_else(|| "pull request summary is unavailable".to_string())?;
        let change_request = summary
            .change_request_identity
            .clone()
            .ok_or_else(|| "pull request identity is unavailable".to_string())?;
        let expected_head_sha = summary.head_sha.clone();
        if expected_head_sha.trim().is_empty() {
            return Err("pull request head is unavailable".to_string());
        }

        let repo = context.repo;
        let mut cache = self.sessions[selected].pr.clone();
        let worktree = self.sessions[selected]
            .identity_key(&self.repos[self.sessions[selected].repo_index].identity);
        let generation = self
            .worktree_generations
            .get(&worktree)
            .copied()
            .unwrap_or_default();
        let mutation = crate::tui::RemoteMutationTarget::Merge {
            change_request: change_request.clone(),
            expected_head_sha: expected_head_sha.clone(),
        };
        let merge_request_id = format!("merge:{}:{}", summary.number, expected_head_sha);
        let merge_subject = format!("{}#{}", path.display(), summary.number);
        let merge_operation = remote_operation::RemoteMutationOperation::TuiMergeChangeRequest(
            remote_operation::TuiRemoteMergePayload {
                repository: repo.root.clone(),
                worktree: path.clone(),
                change_request,
                display_number: summary.number,
                expected_head_sha: expected_head_sha.clone(),
            },
        );
        let result = self.run_remote_action(
            raw,
            crate::tui::RemoteActionRequest {
                key: TuiJobKey::Worktree(worktree),
                generation,
                name: "prism-merge-change-request",
                title: "Merge Pull Request",
                message: "Requesting guarded merge from the provider",
                abandon_cancelable: false,
                effect: crate::tui::RemoteActionEffect::CoordinatedMutation {
                    target: Box::new(mutation),
                    ledger: Box::new(crate::tui::RemoteMutationLedgerContext {
                        repository: repo.root.clone(),
                        worktree: path.clone(),
                        request_id: merge_request_id.clone(),
                        operation: merge_operation.clone(),
                        subject: merge_subject.clone(),
                    }),
                },
            },
            move |context| {
                let progress = context.clone();
                let cancellation = context;
                let result: remote_operation::TuiRemoteMergeResult =
                    crate::worker::mutate_remote_with_progress(
                        &repo.root,
                        &path,
                        &merge_request_id,
                        merge_operation,
                        &merge_subject,
                        crate::worker::RemoteRequestProgress::new(
                            move |wait| report_remote_wait(&progress, wait),
                            move || cancellation.is_canceled(),
                        ),
                    )?;
                match result {
                    remote_operation::TuiRemoteMergeResult::Accepted {
                        outcome,
                        summary,
                    } => {
                        cache.apply_worker_summary(*summary);
                        if outcome
                            == remote_operation::TuiRemoteMergeOutcome::Uncertain
                        {
                            cache.require_reconciliation(
                                "provider merge outcome is uncertain; authoritative re-observation required",
                            );
                        }
                        Ok(RemoteActionValue::Merge {
                            cache: Box::new(cache),
                            outcome,
                        })
                    }
                    remote_operation::TuiRemoteMergeResult::Rejected { reason } => {
                        Ok(RemoteActionValue::MergeRejected(reason))
                    }
                }
            },
        )?;
        let (cache, outcome) = match result {
            RemoteActionValue::Merge { cache, outcome } => (cache, outcome),
            RemoteActionValue::MergeRejected(reason) => return Err(reason),
            _ => return Err("merge returned an unexpected result".to_string()),
        };
        self.apply_remote_cache_result(selected, *cache);
        match outcome {
            remote_operation::TuiRemoteMergeOutcome::Merged => {
                self.show_message("pull request merged")
            }
            remote_operation::TuiRemoteMergeOutcome::Pending => {
                self.show_message("pull request accepted by the provider and is pending merge")
            }
            remote_operation::TuiRemoteMergeOutcome::Uncertain => Err(
                "provider merge outcome is uncertain; authoritative re-observation required"
                    .to_string(),
            ),
        }
    }

    pub(crate) fn push_selected_branch(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let context = self
            .selected_worktree_context()
            .ok_or_else(|| "no worktree selected".to_string())?;
        let selected = context.session_index;
        let path = self.sessions[selected].path.clone();
        let branch = self.sessions[selected].branch.clone();
        if self.sessions[selected].is_default_branch(&context.config) {
            return self.show_message("default branch is not treated as a PR branch");
        }
        if self.sessions[selected].is_detached() {
            return self.show_message("cannot push a detached worktree");
        }

        let repo = context.repo;
        let config = context.config;
        let mut cache = self.sessions[selected].pr.clone();
        let worktree = self.sessions[selected]
            .identity_key(&self.repos[self.sessions[selected].repo_index].identity);
        let generation = self
            .worktree_generations
            .get(&worktree)
            .copied()
            .unwrap_or_default();
        let expected = crate::remote::dispatcher::prepare_push(&path, &config, &branch)?;
        let mutation = remote_push_mutation_target(&expected);
        let push_request_id = format!("push:{}:{}", branch, expected.expected_head_sha);
        let push_subject = format!("{}:{}", path.display(), branch);
        let push_operation = remote_operation::RemoteMutationOperation::TuiPushBranch(
            remote_operation::TuiRemotePushPayload {
                repository: repo.root.clone(),
                worktree: path.clone(),
                branch: branch.clone(),
                expected,
            },
        );
        let RemoteActionValue::Cache(cache) = self.run_remote_action(
            raw,
            crate::tui::RemoteActionRequest {
                key: TuiJobKey::Worktree(worktree),
                generation,
                name: "prism-push-branch",
                title: "Push Branch",
                message: "Verifying and pushing selected branch",
                abandon_cancelable: false,
                effect: crate::tui::RemoteActionEffect::CoordinatedMutation {
                    target: Box::new(mutation),
                    ledger: Box::new(crate::tui::RemoteMutationLedgerContext {
                        repository: repo.root.clone(),
                        worktree: path.clone(),
                        request_id: push_request_id.clone(),
                        operation: push_operation.clone(),
                        subject: push_subject.clone(),
                    }),
                },
            },
            move |context| {
                let progress = context.clone();
                let cancellation = context.clone();
                let snapshot = crate::worker::mutate_remote_with_progress(
                    &repo.root,
                    &path,
                    &push_request_id,
                    push_operation,
                    &push_subject,
                    crate::worker::RemoteRequestProgress::new(
                        move |wait| report_remote_wait(&progress, wait),
                        move || cancellation.is_canceled(),
                    ),
                )?;
                cache.apply_worker_snapshot(snapshot);
                Ok(RemoteActionValue::Cache(Box::new(cache)))
            },
        )?
        else {
            return Err("push returned an unexpected result".to_string());
        };
        self.apply_remote_cache_result(selected, *cache);
        self.show_message("push complete")
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
        let branch = self.sessions[context.session_index].branch.clone();
        let mut cache = self.sessions[context.session_index].pr.clone();
        let worktree = self.sessions[context.session_index]
            .identity_key(&self.repos[self.sessions[context.session_index].repo_index].identity);
        let generation = self
            .worktree_generations
            .get(&worktree)
            .copied()
            .unwrap_or_default();
        let resolve_subject = format!("{}#{}", path.display(), summary.number);
        let resolve_operation = remote_operation::RemoteMutationOperation::TuiResolveReviewThreads(
            remote_operation::TuiRemoteResolvePayload {
                repository: repo.root.clone(),
                worktree: path.clone(),
                summary: summary.clone(),
                thread_ids: thread_ids.clone(),
            },
        );
        let resolve_request_id = resolve_review_request_id(&resolve_operation, &resolve_subject)?;
        let RemoteActionValue::Resolved { cache, count } = self.run_remote_action(
            raw,
            crate::tui::RemoteActionRequest {
                key: TuiJobKey::Worktree(worktree),
                generation,
                name: "prism-resolve-review-threads",
                title: "Resolve Review Conversations",
                message: "Resolving observed review conversations",
                abandon_cancelable: false,
                effect: crate::tui::RemoteActionEffect::CoordinatedMutation {
                    target: Box::new(crate::tui::RemoteMutationTarget::Resolve {
                        change_request: summary
                            .change_request_identity
                            .clone()
                            .ok_or_else(|| "pull request identity is unavailable".to_string())?,
                        thread_ids: thread_ids.clone(),
                    }),
                    ledger: Box::new(crate::tui::RemoteMutationLedgerContext {
                        repository: repo.root.clone(),
                        worktree: path.clone(),
                        request_id: resolve_request_id.clone(),
                        operation: resolve_operation.clone(),
                        subject: resolve_subject.clone(),
                    }),
                },
            },
            move |context| {
                let mutation_progress = context.clone();
                let mutation_cancellation = context.clone();
                let count = crate::worker::mutate_remote_with_progress(
                    &repo.root,
                    &path,
                    &resolve_request_id,
                    resolve_operation,
                    &resolve_subject,
                    crate::worker::RemoteRequestProgress::new(
                        move |wait| report_remote_wait(&mutation_progress, wait),
                        move || mutation_cancellation.is_canceled(),
                    ),
                )?;
                let observation_progress = context.clone();
                let observation_cancellation = context;
                let snapshot = crate::worker::observe_remote_with_progress(
                    &repo.root,
                    &path,
                    remote_operation::RemoteObservationOperation::TuiChangeRequestCache(
                        remote_operation::TuiRemoteCachePayload {
                            repository: repo.root.clone(),
                            worktree: path.clone(),
                            branch: branch.clone(),
                            force_details: true,
                        },
                    ),
                    &format!("{}:{}:details", path.display(), branch),
                    move |wait| report_remote_wait(&observation_progress, wait),
                    move || observation_cancellation.is_canceled(),
                )?;
                cache.apply_worker_snapshot(snapshot);
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
        let RemoteActionValue::ChangeRequests(mut prs) = self.run_remote_action(
            raw,
            crate::tui::RemoteActionRequest {
                key: TuiJobKey::Repository(self.repos[context.repo_index].identity.clone()),
                generation: self.session_inventory_generation,
                name: "prism-list-change-requests",
                title: "Remote Pull Requests",
                message: "Loading open pull requests",
                abandon_cancelable: true,
                effect: crate::tui::RemoteActionEffect::ReadOnly,
            },
            move |context| {
                let progress = context.clone();
                let cancellation = context;
                crate::worker::observe_remote_with_progress(
                    &path,
                    &path,
                    remote_operation::RemoteObservationOperation::TuiChangeRequests(
                        remote_operation::TuiRemoteListPayload {
                            repository: path.clone(),
                            worktree: path.clone(),
                        },
                    ),
                    &path.to_string_lossy(),
                    move |wait| report_remote_wait(&progress, wait),
                    move || cancellation.is_canceled(),
                )
                .map(RemoteActionValue::ChangeRequests)
            },
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
        let job_summary = summary.clone();
        let job_branch = branch.clone();
        let fetch_request_id = format!("fetch:{}:{}", summary.number, summary.head_sha);
        let fetch_subject = format!("{}#{}", path.display(), summary.number);
        let fetch_operation = remote_operation::RemoteMutationOperation::TuiFetchChangeRequest(
            remote_operation::TuiRemoteFetchPayload {
                repository: path.clone(),
                worktree: path.clone(),
                branch: job_branch.clone(),
                summary: job_summary.clone(),
            },
        );
        let RemoteActionValue::Complete = self.run_remote_action(
            raw,
            crate::tui::RemoteActionRequest {
                key: TuiJobKey::Repository(self.repos[context.repo_index].identity.clone()),
                generation: self.session_inventory_generation,
                name: "prism-fetch-change-request",
                title: "Remote Pull Requests",
                message: &format!("Fetching PR #{}", summary.number),
                abandon_cancelable: true,
                effect: crate::tui::RemoteActionEffect::CoordinatedMutation {
                    target: Box::new(crate::tui::RemoteMutationTarget::Fetch {
                        change_request: summary
                            .change_request_identity
                            .clone()
                            .ok_or_else(|| "pull request identity is unavailable".to_string())?,
                        branch: branch.clone(),
                        expected_head_sha: summary.head_sha.clone(),
                    }),
                    ledger: Box::new(crate::tui::RemoteMutationLedgerContext {
                        repository: path.clone(),
                        worktree: path.clone(),
                        request_id: fetch_request_id.clone(),
                        operation: fetch_operation.clone(),
                        subject: fetch_subject.clone(),
                    }),
                },
            },
            move |context| {
                let progress = context.clone();
                let cancellation = context;
                crate::worker::mutate_remote_with_progress::<bool, _, _>(
                    &path,
                    &path,
                    &fetch_request_id,
                    fetch_operation,
                    &fetch_subject,
                    crate::worker::RemoteRequestProgress::new(
                        move |wait| report_remote_wait(&progress, wait),
                        move || cancellation.is_canceled(),
                    ),
                )?;
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
        let first_attempt = checkout_worktree_session(&context.repo, &context.config, &branch);
        self.request_wt_hook_log_refresh(context.repo_index);
        let creation = match first_attempt {
            Ok(outcome) => outcome,
            Err(error) => {
                if !error.approval_required()
                    || !self.offer_worktrunk_approval(raw, &context.repo, &context.config)?
                {
                    self.request_wt_poll(context.repo_index);
                    return Err(error.to_string());
                }
                self.show_loading_dialog(
                    raw,
                    "Remote Pull Requests",
                    &format!("Opening worktree for PR #{}", summary.number),
                )?;
                let retry = checkout_worktree_session(&context.repo, &context.config, &branch);
                self.request_wt_hook_log_refresh(context.repo_index);
                match retry {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        self.request_wt_poll(context.repo_index);
                        return Err(error.to_string());
                    }
                }
            }
        };
        if let CreateWorktreeOutcome::CreatedMetadataFailed { error } = creation {
            self.refresh_sessions()?;
            self.request_wt_poll(context.repo_index);
            self.show_message(&format!(
                "worktree opened, but restoring Prism metadata failed: {error}"
            ))?;
            return Ok(None);
        }

        self.refresh_sessions()?;
        self.request_wt_poll(context.repo_index);
        self.start_tmux_agent_warmup();
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
        let (kind, label, body_required) = match choice.as_str() {
            "a" => (
                crate::remote::ReviewSubmissionKind::Approve,
                "approved",
                false,
            ),
            "c" => (
                crate::remote::ReviewSubmissionKind::Comment,
                "commented on",
                true,
            ),
            "r" => (
                crate::remote::ReviewSubmissionKind::RequestChanges,
                "requested changes on",
                true,
            ),
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
        let expected_state = match kind {
            crate::remote::ReviewSubmissionKind::Approve => "APPROVED",
            crate::remote::ReviewSubmissionKind::Comment => "COMMENTED",
            crate::remote::ReviewSubmissionKind::RequestChanges => "CHANGES_REQUESTED",
        }
        .to_string();
        let review_request_id = format!(
            "review:{}:{}:{kind:?}:{:016x}",
            summary.number,
            summary.head_sha,
            crate::util::stable_hash(std::path::Path::new(&body))
        );
        let review_subject = format!("{}#{}", path.display(), summary.number);
        let review_operation = remote_operation::RemoteMutationOperation::TuiSubmitReview(
            remote_operation::TuiRemoteReviewPayload {
                repository: path.clone(),
                worktree: path.clone(),
                summary: selected_summary.clone(),
                kind,
                body: body.clone(),
            },
        );
        let review_target = crate::tui::RemoteMutationTarget::Review {
            change_request: summary
                .change_request_identity
                .clone()
                .ok_or_else(|| "pull request identity is unavailable".to_string())?,
            expected_state,
            expected_body: body,
            prior_review_ids,
        };
        let RemoteActionValue::Complete = self.run_remote_action(
            raw,
            crate::tui::RemoteActionRequest {
                key: TuiJobKey::Repository(self.repos[context.repo_index].identity.clone()),
                generation: self.session_inventory_generation,
                name: "prism-submit-review",
                title: "Submit Review",
                message: &format!("Submitting review for PR #{}", summary.number),
                abandon_cancelable: false,
                effect: crate::tui::RemoteActionEffect::CoordinatedMutation {
                    target: Box::new(review_target),
                    ledger: Box::new(crate::tui::RemoteMutationLedgerContext {
                        repository: path.clone(),
                        worktree: path.clone(),
                        request_id: review_request_id.clone(),
                        operation: review_operation.clone(),
                        subject: review_subject.clone(),
                    }),
                },
            },
            move |context| {
                let progress = context.clone();
                let cancellation = context;
                crate::worker::mutate_remote_with_progress::<serde_json::Value, _, _>(
                    &path,
                    &path,
                    &review_request_id,
                    review_operation,
                    &review_subject,
                    crate::worker::RemoteRequestProgress::new(
                        move |wait| report_remote_wait(&progress, wait),
                        move || cancellation.is_canceled(),
                    ),
                )?;
                Ok(RemoteActionValue::Complete)
            },
        )?
        else {
            return Err("review submission returned an unexpected result".to_string());
        };
        self.show_message(&format!("{label} PR #{}", summary.number))
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
                    effect: crate::tui::RemoteActionEffect::ReadOnly,
                },
                move |context| {
                    let progress = context.clone();
                    let cancellation = context;
                    let snapshot = crate::worker::observe_remote_with_progress(
                        &repo.root,
                        &path,
                        remote_operation::RemoteObservationOperation::TuiChangeRequestCache(
                            remote_operation::TuiRemoteCachePayload {
                                repository: repo.root.clone(),
                                worktree: path.clone(),
                                branch: branch.clone(),
                                force_details: false,
                            },
                        ),
                        &format!("{}:{}:summary", path.display(), branch),
                        move |wait| report_remote_wait(&progress, wait),
                        move || cancellation.is_canceled(),
                    )?;
                    cache.apply_worker_snapshot(snapshot);
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
}
