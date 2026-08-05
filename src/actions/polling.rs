use super::*;

pub(super) const DEFAULT_BRANCH_STATUS_POLL_INTERVAL: Duration = Duration::from_secs(60);
pub(super) const BACKGROUND_PR_SUMMARY_POLL_INTERVAL: Duration = Duration::from_secs(60);
pub(super) const ACTIVE_WT_POLL_INTERVAL: Duration = Duration::from_secs(15);
pub(super) const INACTIVE_WT_POLL_INTERVAL: Duration = Duration::from_secs(60);

pub(super) fn pr_poll_key(
    repository: &crate::session::WorktreeRepositoryKey,
    generation: u64,
    session: &crate::session::Session,
) -> PrPollKey {
    PrPollKey::for_repository_session_generation(repository, session, generation)
}

pub(super) fn fetch_wt_observation(
    repo: &crate::repo::Repository,
    config: &crate::config::Config,
) -> Result<crate::worktrunk::WorktrunkSnapshot, crate::worktrunk::WorktrunkFailure> {
    crate::worktrunk::observe_repository(repo, config)
}

pub(super) fn default_branch_status_label(
    path: &Path,
    branch: &str,
    config: &crate::config::Config,
) -> Result<String, String> {
    let behind = branch_behind(path, branch, config)?;
    Ok(status_label_with_behind(
        &git_status_label(path, config),
        behind,
    ))
}

pub(super) fn status_label_with_behind(label: &str, behind: usize) -> String {
    let dirty = status_count(label, "dirty");
    let ahead = status_count(label, "ahead");
    let mut parts = Vec::new();
    if let Some(count) = dirty {
        parts.push(format!("dirty {count}"));
    }
    if let Some(count) = ahead {
        parts.push(format!("ahead {count}"));
    }
    if behind > 0 {
        parts.push(format!("behind {behind}"));
    }
    if !parts.is_empty() {
        return parts.join(" ");
    }
    if label == "clean" || status_count(label, "behind").is_some() {
        "clean".to_string()
    } else {
        label.to_string()
    }
}

impl Tui {
    pub(crate) fn poll_pull_requests(&mut self, force: bool) -> bool {
        let mut changed = self.drain_pr_poll_results();
        for repo_index in 0..self.repos.len() {
            let interval = if repo_index == self.current_repo {
                PR_SUMMARY_POLL_INTERVAL
            } else {
                BACKGROUND_PR_SUMMARY_POLL_INTERVAL
            };
            let (summaries_due, summary_in_flight, config) = {
                let managed = &self.repos[repo_index];
                (
                    managed
                        .pr_summary_last_polled
                        .map(|last| last.elapsed() >= interval)
                        .unwrap_or(true),
                    managed.pr_summary_poll_in_flight,
                    managed.config.clone(),
                )
            };
            let cleared = self
                .sessions
                .iter_mut()
                .enumerate()
                .filter(|(_, session)| session.repo_index == repo_index)
                .filter_map(|(index, session)| {
                    session
                        .pr
                        .enforce_structural_eligibility(&session.branch, &config, session.hidden)
                        .then(|| {
                            session.unseen_comments = false;
                            index
                        })
                })
                .collect::<Vec<_>>();
            for index in cleared {
                self.queue_pr_persistence(index, false, false);
                changed = true;
            }
            if (force || summaries_due) && !summary_in_flight {
                let poll_started_at = std::time::Instant::now();
                let path = self.repos[repo_index].repo.root.clone();
                let repository = self.repos[repo_index].identity.clone();
                let session_snapshots = self
                    .sessions
                    .iter()
                    .filter(|session| session.repo_index == repo_index && !session.hidden)
                    .map(|session| {
                        (
                            session.identity_key(&repository),
                            session.background_job_snapshot(),
                        )
                    })
                    .collect::<Vec<_>>();
                let sessions = session_snapshots
                    .iter()
                    .map(|(key, _)| key.clone())
                    .collect::<Vec<_>>();
                let config = self.repos[repo_index].config.clone();
                let reconciliation_refs = self.remote_push_reconciliation_refs(&repository);
                for session in self
                    .sessions
                    .iter_mut()
                    .filter(|session| session.repo_index == repo_index && !session.hidden)
                {
                    session.pr.begin_summary_poll(poll_started_at);
                }
                if let Some(managed) = self.repos.get_mut(repo_index) {
                    managed.pr_summary_last_polled = Some(poll_started_at);
                    managed.pr_summary_poll_in_flight = true;
                }
                let generation = self.session_inventory_generation;
                let job_repository = repository.clone();
                self.spawn_tui_job(
                    TuiJobKind::PrSummary,
                    TuiJobKey::Repository(repository.clone()),
                    generation,
                    Some(TUI_ACTION_JOB_TIMEOUT),
                    format!("prism-pr-summary-{repo_index}"),
                    move |_| {
                        let adapter = crate::remote::dispatcher::capabilities(&path, &config);
                        let github_remote_configured = adapter.is_ok();
                        let summaries = if github_remote_configured {
                            let _ = refresh_repo_policy_cache(
                                &crate::repo::Repository { root: path.clone() },
                                &path,
                                &config,
                            );
                            fetch_pr_summary_index(&path, &config)
                        } else {
                            Err(adapter.as_ref().unwrap_err().clone())
                        };
                        let capabilities = if summaries.is_ok() {
                            crate::remote::dispatcher::capabilities(&path, &config)
                                .ok()
                                .or_else(|| adapter.ok())
                        } else {
                            adapter.ok()
                        };
                        let observations = match &summaries {
                            Ok(summaries) => Ok(session_snapshots
                                .into_iter()
                                .map(|(key, session)| PrSummarySessionResult {
                                    key,
                                    summary: resolve_pr_summary_for_session(
                                        &session, &config, summaries,
                                    ),
                                })
                                .collect()),
                            Err(error) => Err(error.clone()),
                        };
                        let remote_branch_heads = reconciliation_refs
                            .into_iter()
                            .filter_map(|(remote, branch)| {
                                remote_branch_head(&path, &config, &remote, &branch)
                                    .ok()
                                    .flatten()
                                    .map(|head| ((remote, branch), head))
                            })
                            .collect();
                        Ok(Some(TuiJobPayload::PrPoll(PrPollResult::Summary {
                            repository: job_repository,
                            sessions,
                            github_remote_configured,
                            capabilities,
                            summaries,
                            observations,
                            remote_branch_heads,
                            refreshed: crate::util::timestamp_label(),
                            poll_started_at,
                        })))
                    },
                );
            }
        }

        let selected = self.selected_worktree_index();
        if let Some(index) = selected {
            let Some(session) = self.sessions.get(index) else {
                return changed;
            };
            let Some(managed) = self.repos.get(session.repo_index) else {
                return changed;
            };
            let identity = session.identity_key(&managed.identity);
            let generation = self
                .worktree_generations
                .get(&identity)
                .copied()
                .unwrap_or_default();
            let key = pr_poll_key(&managed.identity, generation, session);
            let config = managed.config.clone();
            let details_pollable = pr_details_pollable(session, &config);
            let session = &mut self.sessions[index];
            if !session.hidden && details_pollable && !self.pr_polls_in_flight.contains(&key) {
                let branch = session.branch.clone();
                let path = session.path.clone();
                let mut cache = session.pr.begin_details_poll();
                self.pr_polls_in_flight.insert(key.clone());
                let job_key = key.clone();
                self.spawn_tui_job(
                    TuiJobKind::PrDetails,
                    TuiJobKey::Pr(key.clone()),
                    generation,
                    Some(TUI_ACTION_JOB_TIMEOUT),
                    format!("prism-pr-details-{index}"),
                    move |_| {
                        refresh_pr_details_cache_state(&branch, &mut cache, &path, &config);
                        Ok(Some(TuiJobPayload::PrPoll(PrPollResult::Details {
                            key: job_key,
                            cache: Box::new(cache),
                        })))
                    },
                );
            }
        }
        self.start_pr_persistence_jobs();
        changed
    }

    pub(crate) fn drain_pr_poll_results(&mut self) -> bool {
        if !self.tui_tick_active && !self.routing_tui_jobs {
            self.route_tui_job_messages();
        }
        let mut changed = false;
        let selected = self.selected_worktree_index();
        while let Ok(result) = self.pr_poll_rx.try_recv() {
            match result {
                PrPollResult::Summary {
                    repository,
                    sessions,
                    github_remote_configured,
                    capabilities,
                    summaries,
                    observations,
                    remote_branch_heads,
                    refreshed,
                    poll_started_at,
                } => {
                    let summary_evidence = summaries.as_ref().ok().cloned();
                    let Some(repo_index) = self
                        .repos
                        .iter()
                        .position(|managed| managed.identity == repository)
                    else {
                        continue;
                    };
                    let before = self
                        .sessions
                        .iter()
                        .map(|session| pr_cache_render_signature(&session.pr))
                        .collect::<Vec<_>>();
                    let before_comments = self
                        .sessions
                        .iter()
                        .map(|session| pr_cache_comment_count(&session.pr))
                        .collect::<Vec<_>>();
                    let mut persistence = Vec::new();
                    if let Some(repo) = self.repos.get_mut(repo_index) {
                        repo.remote_capabilities = capabilities;
                        repo.remote_capability_error = if github_remote_configured {
                            None
                        } else {
                            summaries.as_ref().err().cloned()
                        };
                    }
                    if !github_remote_configured {
                        let error = summaries
                            .as_ref()
                            .err()
                            .cloned()
                            .unwrap_or_else(|| "remote adapter is unavailable".to_string());
                        for (index, session) in self
                            .sessions
                            .iter_mut()
                            .enumerate()
                            .filter(|(_, session)| session.repo_index == repo_index)
                        {
                            if session.pr.record_remote_unavailable(error.clone()) {
                                persistence.push((index, false));
                            }
                        }
                    } else {
                        if let Ok(summaries) = summaries
                            && let Some(repo) = self.repos.get_mut(repo_index)
                        {
                            repo.pr_summaries = summaries;
                        }
                        if repo_index == self.current_repo {
                            self.ensure_selected_repo_pr();
                        }
                        let observations = match observations {
                            Ok(observations) => observations
                                .into_iter()
                                .map(|observation| (observation.key, Ok(observation.summary)))
                                .collect::<Vec<_>>(),
                            Err(error) => sessions
                                .into_iter()
                                .map(|key| (key, Err(error.clone())))
                                .collect::<Vec<_>>(),
                        };
                        for (key, observation) in observations {
                            let Some(index) = self.sessions.iter().position(|session| {
                                self.repos.get(session.repo_index).is_some_and(|managed| {
                                    session.identity_key(&managed.identity) == key
                                })
                            }) else {
                                continue;
                            };
                            let before_summary = self.sessions[index].pr.summary().cloned();
                            if apply_pr_summary_poll_result(
                                &mut self.sessions[index].pr,
                                poll_started_at,
                                observation,
                                &refreshed,
                            ) {
                                persistence.push((
                                    index,
                                    before_summary != self.sessions[index].pr.summary().cloned(),
                                ));
                            }
                        }
                    }
                    for (index, remote_update) in persistence {
                        self.queue_pr_persistence(index, false, remote_update);
                    }
                    if let Some(summaries) = summary_evidence {
                        self.reconcile_remote_mutation_summaries(
                            &repository,
                            &summaries,
                            &remote_branch_heads,
                        );
                    }
                    let after = self
                        .sessions
                        .iter()
                        .map(|session| pr_cache_render_signature(&session.pr))
                        .collect::<Vec<_>>();
                    for (index, session) in self.sessions.iter_mut().enumerate() {
                        let before = before_comments.get(index).copied().unwrap_or(0);
                        let after = pr_cache_comment_count(&session.pr);
                        if after > before && Some(index) != selected {
                            session.unseen_comments = true;
                        }
                    }
                    changed |= before != after;
                }
                PrPollResult::Details { key, cache } => {
                    let key_for_index = |index: usize| {
                        let session = self.sessions.get(index)?;
                        let repo = self.repos.get(session.repo_index)?;
                        let identity = session.identity_key(&repo.identity);
                        let generation = self
                            .worktree_generations
                            .get(&identity)
                            .copied()
                            .unwrap_or_default();
                        Some(pr_poll_key(&repo.identity, generation, session))
                    };
                    let selected_key = selected.and_then(key_for_index);
                    let session_index = (0..self.sessions.len())
                        .find(|index| key_for_index(*index).as_ref() == Some(&key));
                    if let Some(session_index) = session_index {
                        let (applied, remote_update) = {
                            let session = &mut self.sessions[session_index];
                            let before = pr_cache_render_signature(&session.pr);
                            let before_comments = pr_cache_comment_count(&session.pr);
                            let before_details = session.pr.details().cloned();
                            let applied = apply_pr_details_poll_result(&mut session.pr, *cache);
                            if applied
                                && pr_cache_comment_count(&session.pr) > before_comments
                                && selected_key.as_ref() != Some(&key)
                            {
                                session.unseen_comments = true;
                            }
                            changed |= before != pr_cache_render_signature(&session.pr);
                            let remote_update =
                                applied && before_details != session.pr.details().cloned();
                            (applied, remote_update)
                        };
                        if applied {
                            self.queue_pr_persistence(session_index, true, remote_update);
                            let repository = self.repos[self.sessions[session_index].repo_index]
                                .identity
                                .clone();
                            let cache = self.sessions[session_index].pr.clone();
                            self.reconcile_remote_mutation_details(&repository, &cache);
                        }
                    }
                }
                PrPollResult::Persistence {
                    key,
                    version,
                    details,
                    result,
                    remote_update,
                    status_label,
                    auto_run,
                } => {
                    if self.pr_persistence_versions.get(&key).copied() != Some(version) {
                        if remote_update
                            && let Some(request) = self.pr_persistence_pending.get_mut(&key)
                        {
                            request.remote_update = true;
                        }
                        continue;
                    }
                    let session_index = self.sessions.iter().position(|session| {
                        self.repos.get(session.repo_index).is_some_and(|managed| {
                            session.identity_key(&managed.identity) == key.worktree
                        })
                    });
                    if let Some(index) = session_index {
                        let before = pr_cache_render_signature(&self.sessions[index].pr);
                        self.sessions[index]
                            .pr
                            .record_background_persistence_result(details, result);
                        if remote_update && let Some(status_label) = status_label {
                            changed |= self.sessions[index].status_label != status_label;
                            self.sessions[index].status_label = status_label;
                        }
                        changed |= before != pr_cache_render_signature(&self.sessions[index].pr);
                    } else if !self.pr_persistence_pending.contains_key(&key) {
                        self.pr_persistence_versions.remove(&key);
                    }
                    if let Ok(Some(run)) = auto_run {
                        changed |= self.remember_auto_run(*run);
                    }
                }
            }
        }
        self.start_pr_persistence_jobs();
        changed
    }

    pub(super) fn queue_pr_persistence(
        &mut self,
        session_index: usize,
        details: bool,
        mut remote_update: bool,
    ) {
        let Some(session) = self.sessions.get(session_index) else {
            return;
        };
        let Some(managed) = self.repos.get(session.repo_index) else {
            return;
        };
        let identity = session.identity_key(&managed.identity);
        let generation = self
            .worktree_generations
            .get(&identity)
            .copied()
            .unwrap_or_default();
        let key = pr_poll_key(&managed.identity, generation, session);
        let version = self
            .pr_persistence_versions
            .entry(key.clone())
            .and_modify(|version| *version = version.saturating_add(1))
            .or_insert(1);
        remote_update |= self
            .pr_persistence_pending
            .get(&key)
            .is_some_and(|request| request.remote_update);
        self.pr_persistence_pending.insert(
            key.clone(),
            PrPersistenceRequest {
                key,
                version: *version,
                details,
                repo: managed.repo.clone(),
                branch: session.branch.clone(),
                cache: session.pr.clone(),
                remote_update,
                session: session.background_job_snapshot(),
                config: managed.config.clone(),
                auto_run_id: self.active_auto_runs.get(&session.path).cloned(),
            },
        );
    }

    pub(crate) fn supersede_pr_persistence(&mut self, session_index: usize, details: bool) {
        let Some(session) = self.sessions.get(session_index) else {
            return;
        };
        let Some(managed) = self.repos.get(session.repo_index) else {
            return;
        };
        let identity = session.identity_key(&managed.identity);
        let generation = self
            .worktree_generations
            .get(&identity)
            .copied()
            .unwrap_or_default();
        let key = pr_poll_key(&managed.identity, generation, session);
        if self.pr_persistence_versions.contains_key(&key) {
            self.queue_pr_persistence(session_index, details, false);
            self.start_pr_persistence_jobs();
        }
    }

    pub(crate) fn queue_pr_cache_removal(&mut self, session_index: usize) {
        let Some(session) = self.sessions.get_mut(session_index) else {
            return;
        };
        session.pr = crate::remote::PrCache::default();
        session.unseen_comments = false;
        self.queue_pr_persistence(session_index, false, false);
        self.start_pr_persistence_jobs();
    }

    fn start_pr_persistence_jobs(&mut self) {
        let keys = self
            .pr_persistence_pending
            .keys()
            .filter(|key| !self.pr_persistence_in_flight.contains(*key))
            .cloned()
            .collect::<Vec<_>>();
        for key in keys {
            let Some(request) = self.pr_persistence_pending.remove(&key) else {
                continue;
            };
            self.pr_persistence_in_flight.insert(key.clone());
            let generation = key.generation;
            self.spawn_tui_job(
                TuiJobKind::PrPersistence,
                TuiJobKey::PrPersistence(key),
                generation,
                Some(TUI_ACTION_JOB_TIMEOUT),
                "prism-pr-persistence".to_string(),
                move |_| {
                    let result =
                        persist_pr_cache_snapshot(&request.repo, &request.branch, &request.cache);
                    let (status_label, auto_run) = if result.is_ok() && request.remote_update {
                        let status_label = Some(crate::git::git_status_label(
                            &request.session.path,
                            &request.config,
                        ));
                        let auto_run = request.auto_run_id.as_deref().map_or(Ok(None), |run_id| {
                            crate::observability::with_writable_db(&request.repo, |conn| {
                                let Some(mut run) = crate::auto_flow::load_auto_run(conn, run_id)?
                                else {
                                    return Ok(None);
                                };
                                let mut session = request.session;
                                session.pr = request.cache.clone();
                                crate::auto_flow::stabilization_execute::observe_cached_plan_and_save(
                                    conn,
                                    &request.repo,
                                    &request.config,
                                    &session,
                                    &mut run,
                                )?;
                                Ok(Some(Box::new(run)))
                            })
                        });
                        if let Err(error) = &auto_run {
                            let _ = append_runtime_message(
                                &request.repo,
                                &format!("remote gate state refresh failed: {error}"),
                            );
                        }
                        (status_label, auto_run)
                    } else {
                        (None, Ok(None))
                    };
                    Ok(Some(TuiJobPayload::PrPoll(PrPollResult::Persistence {
                        key: request.key,
                        version: request.version,
                        details: request.details,
                        result,
                        remote_update: request.remote_update,
                        status_label,
                        auto_run,
                    })))
                },
            );
        }
    }

    pub(crate) fn start_wt_column_poll(&mut self) {
        self.request_wt_poll(self.current_repo);
    }

    pub(crate) fn request_wt_poll(&mut self, repo_index: usize) {
        self.poll_wt_columns();
        if let Some(managed) = self.repos.get_mut(repo_index) {
            managed.wt_poll_pending = true;
        }
        self.start_scheduled_wt_polls();
    }

    pub(crate) fn start_scheduled_wt_polls(&mut self) {
        for repo_index in 0..self.repos.len() {
            let Some(managed) = self.repos.get(repo_index) else {
                continue;
            };
            if managed.wt_poll_in_flight {
                continue;
            }
            let interval = if repo_index == self.current_repo {
                ACTIVE_WT_POLL_INTERVAL
            } else {
                INACTIVE_WT_POLL_INTERVAL
            };
            let due = wt_poll_due(
                managed.wt_last_polled,
                managed.wt_poll_pending,
                interval,
                std::time::Instant::now(),
            );
            if !due {
                continue;
            }
            let repo = managed.repo.clone();
            let repository = managed.identity.clone();
            let sessions = self
                .sessions
                .iter()
                .filter(|session| session.repo_index == repo_index)
                .map(|session| (session.path.clone(), session.identity_key(&repository)))
                .collect::<Vec<_>>();
            let config = managed.config.clone();
            if let Some(managed) = self.repos.get_mut(repo_index) {
                managed.wt_poll_in_flight = true;
                managed.wt_poll_pending = false;
                managed.wt_last_polled = Some(std::time::Instant::now());
                managed.wt_quality = crate::worktrunk::ObservationQuality::Refreshing;
            }
            let generation = self.session_inventory_generation;
            let job_repository = repository.clone();
            self.spawn_tui_job(
                TuiJobKind::WorktreeColumns,
                TuiJobKey::Repository(repository.clone()),
                generation,
                Some(TUI_ACTION_JOB_TIMEOUT),
                format!("prism-wt-columns-{repo_index}"),
                move |_| {
                    let observation = fetch_wt_observation(&repo, &config).map(|snapshot| {
                        let facts = crate::worktrunk::associate_snapshot(&snapshot, sessions);
                        WtObservation {
                            snapshot,
                            facts,
                            observed_at: std::time::Instant::now(),
                        }
                    });
                    Ok(Some(TuiJobPayload::WorktreeColumns(WtPollResult {
                        repository: job_repository,
                        observation,
                    })))
                },
            );
        }
    }

    pub(crate) fn poll_wt_columns(&mut self) -> bool {
        if !self.tui_tick_active && !self.routing_tui_jobs {
            self.route_tui_job_messages();
        }
        let mut changed = false;
        while let Ok(result) = self.wt_poll_rx.try_recv() {
            let Some(repo_index) = self
                .repos
                .iter()
                .position(|managed| managed.identity == result.repository)
            else {
                continue;
            };
            match result.observation {
                Ok(observation) => {
                    let facts = observation.facts;
                    for session in &mut self.sessions {
                        if session.repo_index != repo_index {
                            continue;
                        }
                        let next = facts
                            .get(&session.identity_key(&result.repository))
                            .map(crate::worktrunk::projected_columns)
                            .unwrap_or_default();
                        if session.wt_columns != next {
                            session.wt_columns = next;
                            changed = true;
                        }
                    }
                    if let Some(managed) = self.repos.get_mut(repo_index) {
                        managed.wt_snapshot = Some(observation.snapshot);
                        managed.wt_facts = facts;
                        managed.wt_last_success = Some(observation.observed_at);
                        managed.wt_last_error = None;
                        managed.wt_quality = crate::worktrunk::ObservationQuality::Fresh;
                    }
                }
                Err(error) => {
                    let summary = error.safe_summary();
                    changed |= self.mark_wt_observation_stale(
                        repo_index,
                        summary,
                        Some(error.to_string()),
                    );
                }
            }
        }
        changed
    }

    pub(crate) fn mark_wt_observation_stale(
        &mut self,
        repo_index: usize,
        summary: String,
        log_error: Option<String>,
    ) -> bool {
        let mut changed = false;
        if let Some(repo) = self.repos.get_mut(repo_index) {
            let previous_quality = repo.wt_quality.clone();
            if repo.wt_last_error.as_deref() != Some(&summary)
                && let Some(error) = log_error
            {
                let _ = append_runtime_message(
                    &repo.repo,
                    &format!("Worktrunk observation refresh failed: {error}"),
                );
            }
            repo.wt_last_error = Some(summary.clone());
            repo.wt_quality = repo
                .wt_last_success
                .map(|last_success| crate::worktrunk::ObservationQuality::Stale {
                    last_success,
                    error: summary,
                })
                .unwrap_or(crate::worktrunk::ObservationQuality::NeverLoaded);
            changed = repo.wt_quality != previous_quality;
        }
        changed
    }

    pub(crate) fn start_default_branch_status_poll(&mut self, force: bool) {
        self.poll_default_branch_status();
        for repo_index in 0..self.repos.len() {
            let Some(managed) = self.repos.get(repo_index) else {
                continue;
            };
            if managed.default_branch_poll_in_flight {
                continue;
            }
            let due = managed
                .default_branch_last_polled
                .map(|last| last.elapsed() >= DEFAULT_BRANCH_STATUS_POLL_INTERVAL)
                .unwrap_or(true);
            if !force && !due {
                continue;
            }
            let Some(branch) = managed
                .config
                .default_base
                .as_deref()
                .map(str::trim)
                .filter(|branch| !branch.is_empty())
                .map(str::to_string)
            else {
                continue;
            };
            let path = self.default_branch_path_for_repo(repo_index, &branch);
            let Some(session) = self.sessions.iter().find(|session| {
                session.repo_index == repo_index && session.branch == branch && session.path == path
            }) else {
                continue;
            };
            let key = session.identity_key(&managed.identity);
            let generation = self
                .worktree_generations
                .get(&key)
                .copied()
                .unwrap_or_default();
            let config = managed.config.clone();
            if let Some(managed) = self.repos.get_mut(repo_index) {
                managed.default_branch_poll_in_flight = true;
                managed.default_branch_last_polled = Some(std::time::Instant::now());
            }
            let job_key = key.clone();
            self.spawn_tui_job(
                TuiJobKind::DefaultBranch,
                TuiJobKey::Worktree(key),
                generation,
                Some(TUI_ACTION_JOB_TIMEOUT),
                format!("prism-default-branch-{repo_index}"),
                move |_| {
                    let status_label = default_branch_status_label(&path, &branch, &config);
                    Ok(Some(TuiJobPayload::DefaultBranch(
                        DefaultBranchPollResult {
                            key: job_key,
                            status_label,
                        },
                    )))
                },
            );
        }
    }

    pub(crate) fn poll_default_branch_status(&mut self) -> bool {
        if !self.tui_tick_active && !self.routing_tui_jobs {
            self.route_tui_job_messages();
        }
        let mut changed = false;
        while let Ok(result) = self.default_branch_poll_rx.try_recv() {
            let Some(repo_index) = self
                .repos
                .iter()
                .position(|managed| managed.identity == result.key.repository)
            else {
                continue;
            };
            match result.status_label {
                Ok(status_label) => {
                    if let Some(session) = self.sessions.iter_mut().find(|session| {
                        session.repo_index == repo_index
                            && self.repos[repo_index]
                                .config
                                .is_default_branch(&session.branch)
                            && session.identity_key(&result.key.repository) == result.key
                    }) && session.status_label != status_label
                    {
                        session.status_label = status_label;
                        changed = true;
                    }
                }
                Err(error) => {
                    if let Some(repo) = self.repos.get(repo_index) {
                        let _ = append_runtime_message(
                            &repo.repo,
                            &format!("default branch status refresh failed: {error}"),
                        );
                    }
                }
            }
        }
        changed
    }
}

fn remote_branch_head(
    path: &Path,
    config: &crate::config::Config,
    remote: &str,
    branch: &str,
) -> Result<Option<String>, String> {
    crate::git::push_remote_branch_head_sha(path, remote, branch, config)
}

fn wt_poll_due(
    last_polled: Option<std::time::Instant>,
    pending: bool,
    interval: Duration,
    now: std::time::Instant,
) -> bool {
    pending
        || last_polled
            .map(|last| now.saturating_duration_since(last) >= interval)
            .unwrap_or(true)
}

#[cfg(test)]
mod wt_poll_tests {
    use super::*;

    #[test]
    fn polling_cadence_distinguishes_active_inactive_and_pending_requests() {
        let now = std::time::Instant::now();
        let sixteen_seconds_ago = now - Duration::from_secs(16);
        assert!(wt_poll_due(
            Some(sixteen_seconds_ago),
            false,
            ACTIVE_WT_POLL_INTERVAL,
            now
        ));
        assert!(!wt_poll_due(
            Some(sixteen_seconds_ago),
            false,
            INACTIVE_WT_POLL_INTERVAL,
            now
        ));
        assert!(wt_poll_due(Some(now), true, INACTIVE_WT_POLL_INTERVAL, now));
        assert!(wt_poll_due(None, false, INACTIVE_WT_POLL_INTERVAL, now));
    }
}
