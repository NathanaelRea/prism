use super::*;

pub(super) const DEFAULT_BRANCH_STATUS_POLL_INTERVAL: Duration = Duration::from_secs(60);
pub(super) const BACKGROUND_PR_SUMMARY_POLL_INTERVAL: Duration = Duration::from_secs(60);

pub(super) fn pr_poll_key(session: &crate::session::Session) -> PrPollKey {
    PrPollKey::for_session(session)
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
            let Some(managed) = self.repos.get(repo_index) else {
                continue;
            };
            let interval = if repo_index == self.current_repo {
                PR_SUMMARY_POLL_INTERVAL
            } else {
                BACKGROUND_PR_SUMMARY_POLL_INTERVAL
            };
            let summaries_due = managed
                .pr_summary_last_polled
                .map(|last| last.elapsed() >= interval)
                .unwrap_or(true);
            let has_pr_branches = self.sessions.iter().any(|session| {
                session.repo_index == repo_index
                    && !session.hidden
                    && pr_cache_pollable(&managed.config, &session.branch, &session.pr)
            });
            if has_pr_branches && (force || summaries_due) && !managed.pr_summary_poll_in_flight {
                let poll_started_at = std::time::Instant::now();
                if !github_remote_configured(&managed.repo.root, &managed.config) {
                    if let Some(managed) = self.repos.get_mut(repo_index) {
                        managed.pr_summary_last_polled = Some(poll_started_at);
                    }
                    for session in self
                        .sessions
                        .iter_mut()
                        .filter(|session| session.repo_index == repo_index)
                    {
                        if pr_cache_render_signature(&session.pr)
                            != pr_cache_render_signature(&Default::default())
                        {
                            session.pr = Default::default();
                            session.unseen_comments = false;
                            changed = true;
                        }
                    }
                    continue;
                }
                let path = managed.repo.root.clone();
                let config = managed.config.clone();
                let tx = self.pr_poll_tx.clone();
                if let Some(managed) = self.repos.get_mut(repo_index) {
                    managed.pr_summary_last_polled = Some(poll_started_at);
                    managed.pr_summary_poll_in_flight = true;
                }
                std::thread::spawn(move || {
                    let _ = refresh_repo_policy_cache(
                        &crate::repo::Repository { root: path.clone() },
                        &path,
                        &config,
                    );
                    let summaries = fetch_pr_summary_index(&path, &config);
                    let _ = tx.send(PrPollResult::Summary {
                        repo_index,
                        summaries,
                        poll_started_at,
                    });
                });
            }
        }

        let selected = self.selected_worktree_index();
        if let Some(session) = selected.and_then(|index| self.sessions.get_mut(index)) {
            let Some(managed) = self.repos.get(session.repo_index) else {
                return changed;
            };
            let key = pr_poll_key(session);
            if !session.hidden
                && github_remote_configured(&session.path, &managed.config)
                && pr_details_pollable(&managed.config, &session.branch, &session.pr)
                && !self.pr_polls_in_flight.contains(&key)
            {
                let config = managed.config.clone();
                let branch = session.branch.clone();
                let path = session.path.clone();
                let mut cache = session.pr.clone();
                let tx = self.pr_poll_tx.clone();
                session.pr.details_last_polled = Some(std::time::Instant::now());
                cache.details_last_polled = session.pr.details_last_polled;
                self.pr_polls_in_flight.insert(key.clone());
                std::thread::spawn(move || {
                    refresh_pr_details_cache(&branch, &mut cache, &path, &config);
                    let _ = tx.send(PrPollResult::Details {
                        key,
                        cache: Box::new(cache),
                    });
                });
            }
        }
        changed
    }

    pub(super) fn drain_pr_poll_results(&mut self) -> bool {
        let mut changed = false;
        let selected = self.selected_worktree_index();
        while let Ok(result) = self.pr_poll_rx.try_recv() {
            match result {
                PrPollResult::Summary {
                    repo_index,
                    summaries,
                    poll_started_at,
                } => {
                    if let Some(repo) = self.repos.get_mut(repo_index) {
                        repo.pr_summary_poll_in_flight = false;
                    }
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
                    match summaries {
                        Ok(summaries) => {
                            let repos = self
                                .repos
                                .iter()
                                .map(|managed| PrCacheRepository {
                                    repo: &managed.repo,
                                    config: &managed.config,
                                })
                                .collect::<Vec<_>>();
                            refresh_pr_summary_index_for_sessions(
                                &repos,
                                &mut self.sessions,
                                repo_index,
                                summaries,
                                poll_started_at,
                            );
                        }
                        Err(error) => {
                            for session in &mut self.sessions {
                                if session.repo_index == repo_index && !session.hidden {
                                    session.pr.error = Some(error.clone());
                                }
                            }
                        }
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
                    self.pr_polls_in_flight.remove(&key);
                    let selected_key =
                        selected.and_then(|index| self.sessions.get(index).map(pr_poll_key));
                    if let Some(session) = self
                        .sessions
                        .iter_mut()
                        .find(|session| pr_poll_key(session) == key)
                    {
                        let before = pr_cache_render_signature(&session.pr);
                        let before_comments = pr_cache_comment_count(&session.pr);
                        if let Some(repo) = self.repos.get(session.repo_index)
                            && apply_pr_details_poll_result(
                                &repo.repo,
                                &session.branch,
                                &mut session.pr,
                                *cache,
                            )
                            && pr_cache_comment_count(&session.pr) > before_comments
                            && selected_key.as_ref() != Some(&key)
                        {
                            session.unseen_comments = true;
                        }
                        changed |= before != pr_cache_render_signature(&session.pr);
                    }
                }
            }
        }
        changed
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
            let config = managed.config.clone();
            let tx = self.wt_poll_tx.clone();
            if let Some(managed) = self.repos.get_mut(repo_index) {
                managed.wt_poll_in_flight = true;
            }
            std::thread::spawn(move || {
                let columns = fetch_wt_columns(&repo, &config);
                let _ = tx.send(WtPollResult {
                    repo_index,
                    columns,
                });
            });
        }
    }

    pub(crate) fn poll_wt_columns(&mut self) -> bool {
        let mut changed = false;
        while let Ok(result) = self.wt_poll_rx.try_recv() {
            if let Some(repo) = self.repos.get_mut(result.repo_index) {
                repo.wt_poll_in_flight = false;
            }
            match result.columns {
                Ok(columns_by_path) => {
                    for session in &mut self.sessions {
                        if session.repo_index != result.repo_index {
                            continue;
                        }
                        let next = columns_by_path
                            .get(&session.path)
                            .cloned()
                            .unwrap_or_default();
                        if session.wt_columns != next {
                            session.wt_columns = next;
                            changed = true;
                        }
                    }
                }
                Err(error) => {
                    if let Some(repo) = self.repos.get(result.repo_index) {
                        let _ = append_runtime_log(
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
            let config = managed.config.clone();
            let tx = self.default_branch_poll_tx.clone();
            if let Some(managed) = self.repos.get_mut(repo_index) {
                managed.default_branch_poll_in_flight = true;
                managed.default_branch_last_polled = Some(std::time::Instant::now());
            }
            std::thread::spawn(move || {
                let status_label = default_branch_status_label(&path, &branch, &config);
                let _ = tx.send(DefaultBranchPollResult {
                    repo_index,
                    branch,
                    path,
                    status_label,
                });
            });
        }
    }

    pub(crate) fn poll_default_branch_status(&mut self) -> bool {
        let mut changed = false;
        while let Ok(result) = self.default_branch_poll_rx.try_recv() {
            if let Some(repo) = self.repos.get_mut(result.repo_index) {
                repo.default_branch_poll_in_flight = false;
            }
            match result.status_label {
                Ok(status_label) => {
                    if let Some(session) = self.sessions.iter_mut().find(|session| {
                        session.repo_index == result.repo_index
                            && session.branch == result.branch
                            && session.path == result.path
                    }) && session.status_label != status_label
                    {
                        session.status_label = status_label;
                        changed = true;
                    }
                }
                Err(error) => {
                    if let Some(repo) = self.repos.get(result.repo_index) {
                        let _ = append_runtime_log(
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
