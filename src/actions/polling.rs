use super::*;

pub(super) const DEFAULT_BRANCH_STATUS_POLL_INTERVAL: Duration = Duration::from_secs(60);
pub(super) const BACKGROUND_PR_SUMMARY_POLL_INTERVAL: Duration = Duration::from_secs(60);

pub(super) fn pr_poll_key(
    repository: &crate::session::WorktreeRepositoryKey,
    generation: u64,
    session: &crate::session::Session,
) -> PrPollKey {
    PrPollKey::for_repository_session_generation(repository, session, generation)
}

pub(super) fn fetch_wt_columns(
    repo: &crate::repo::Repository,
    config: &crate::config::Config,
) -> Result<BTreeMap<PathBuf, BTreeMap<String, String>>, String> {
    let raw = run_capture(
        Command::new(config.tool(&config.worktree_command))
            .arg("-C")
            .arg(&repo.root)
            .args(["list", "--format=json"]),
        crate::process::ProcessPolicy::Metadata,
    )?;
    let mut by_path = BTreeMap::new();
    for object in json_top_level_objects(&raw) {
        let Some(path) = json_string_field(object, "path") else {
            continue;
        };
        let mut columns = discover_wt_columns(object);
        for column in &config.worktree_columns {
            if let Some(value) = wt_column_value(object, column) {
                columns.insert(column.clone(), value);
            }
        }
        by_path.insert(PathBuf::from(path), columns);
    }
    Ok(by_path)
}

pub(super) fn discover_wt_columns(object: &str) -> BTreeMap<String, String> {
    let Ok(value) = serde_json::from_str::<Value>(object) else {
        return BTreeMap::new();
    };
    let mut columns = BTreeMap::new();
    let Some(fields) = value.as_object() else {
        return columns;
    };
    for (key, value) in fields {
        if key == "path" {
            continue;
        }
        collect_wt_column(&mut columns, key, value);
    }
    columns
}

pub(super) fn collect_wt_column(columns: &mut BTreeMap<String, String>, key: &str, value: &Value) {
    match value {
        Value::String(value) => {
            if !value.is_empty() {
                columns.insert(key.to_string(), value.clone());
            }
        }
        Value::Bool(value) => {
            columns.insert(key.to_string(), value.to_string());
        }
        Value::Number(value) => {
            columns.insert(key.to_string(), value.to_string());
        }
        Value::Object(fields) => {
            for (field, value) in fields {
                collect_wt_column(columns, &format!("{key}.{field}"), value);
            }
        }
        Value::Array(_) | Value::Null => {}
    }
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

pub(super) fn wt_column_value(object: &str, column: &str) -> Option<String> {
    if let Some(key) = column.strip_prefix("vars.") {
        return json_object_field(object, "vars").and_then(|vars| json_string_field(vars, key));
    }
    if let Some((object_key, field_key)) = column.split_once('.') {
        return json_object_field(object, object_key)
            .and_then(|inner| json_string_field(inner, field_key));
    }
    json_string_field(object, column)
        .or_else(|| json_bool_field(object, column).map(|value| value.to_string()))
        .or_else(|| {
            if column == "ci" {
                json_object_field(object, "ci").map(|ci| {
                    let status = json_string_field(ci, "status").unwrap_or_default();
                    let number = crate::json::json_u64_field(ci, "number")
                        .map(|number| format!("#{number}"))
                        .unwrap_or_else(|| "ci".to_string());
                    if status.is_empty() {
                        number
                    } else {
                        format!("{number}:{status}")
                    }
                })
            } else {
                None
            }
        })
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
                self.queue_pr_persistence(index, false);
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
                                persistence.push(index);
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
                            if apply_pr_summary_poll_result(
                                &mut self.sessions[index].pr,
                                poll_started_at,
                                observation,
                                &refreshed,
                            ) {
                                persistence.push(index);
                            }
                        }
                    }
                    for index in persistence {
                        self.queue_pr_persistence(index, false);
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
                        let applied = {
                            let session = &mut self.sessions[session_index];
                            let before = pr_cache_render_signature(&session.pr);
                            let before_comments = pr_cache_comment_count(&session.pr);
                            let applied = apply_pr_details_poll_result(&mut session.pr, *cache);
                            if applied
                                && pr_cache_comment_count(&session.pr) > before_comments
                                && selected_key.as_ref() != Some(&key)
                            {
                                session.unseen_comments = true;
                            }
                            changed |= before != pr_cache_render_signature(&session.pr);
                            applied
                        };
                        if applied {
                            self.queue_pr_persistence(session_index, true);
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
                } => {
                    if self.pr_persistence_versions.get(&key).copied() != Some(version) {
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
                        changed |= before != pr_cache_render_signature(&self.sessions[index].pr);
                    } else if !self.pr_persistence_pending.contains_key(&key) {
                        self.pr_persistence_versions.remove(&key);
                    }
                }
            }
        }
        self.start_pr_persistence_jobs();
        changed
    }

    pub(super) fn queue_pr_persistence(&mut self, session_index: usize, details: bool) {
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
        self.pr_persistence_pending.insert(
            key.clone(),
            PrPersistenceRequest {
                key,
                version: *version,
                details,
                repo: managed.repo.clone(),
                branch: session.branch.clone(),
                cache: session.pr.clone(),
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
            self.queue_pr_persistence(session_index, details);
            self.start_pr_persistence_jobs();
        }
    }

    pub(crate) fn queue_pr_cache_removal(&mut self, session_index: usize) {
        let Some(session) = self.sessions.get_mut(session_index) else {
            return;
        };
        session.pr = crate::remote::PrCache::default();
        session.unseen_comments = false;
        self.queue_pr_persistence(session_index, false);
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
                    Ok(Some(TuiJobPayload::PrPoll(PrPollResult::Persistence {
                        key: request.key,
                        version: request.version,
                        details: request.details,
                        result,
                    })))
                },
            );
        }
    }

    pub(crate) fn start_wt_column_poll(&mut self) {
        self.poll_wt_columns();
        for repo_index in 0..self.repos.len() {
            let Some(managed) = self.repos.get(repo_index) else {
                continue;
            };
            if managed.wt_poll_in_flight || managed.config.worktree_columns.is_empty() {
                continue;
            }
            let repo = managed.repo.clone();
            let repository = managed.identity.clone();
            let requested = self
                .sessions
                .iter()
                .filter(|session| session.repo_index == repo_index)
                .map(|session| session.identity_key(&repository))
                .collect::<Vec<_>>();
            let config = managed.config.clone();
            if let Some(managed) = self.repos.get_mut(repo_index) {
                managed.wt_poll_in_flight = true;
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
                    let columns = fetch_wt_columns(&repo, &config);
                    let columns = columns.map(|columns| {
                        requested
                            .into_iter()
                            .map(|key| {
                                let values = columns.get(&key.path).cloned().unwrap_or_default();
                                (key, values)
                            })
                            .collect()
                    });
                    Ok(Some(TuiJobPayload::WorktreeColumns(WtPollResult {
                        repository: job_repository,
                        columns,
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
            match result.columns {
                Ok(columns_by_path) => {
                    for session in &mut self.sessions {
                        if session.repo_index != repo_index {
                            continue;
                        }
                        let next = columns_by_path
                            .get(&session.identity_key(&result.repository))
                            .cloned()
                            .unwrap_or_default();
                        if session.wt_columns != next {
                            session.wt_columns = next;
                            changed = true;
                        }
                    }
                }
                Err(error) => {
                    if let Some(repo) = self.repos.get(repo_index) {
                        let _ = append_runtime_message(
                            &repo.repo,
                            &format!("wt column refresh failed: {error}"),
                        );
                    }
                }
            }
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
