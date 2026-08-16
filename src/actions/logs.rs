use super::*;

impl Tui {
    pub(crate) fn request_wt_hook_log_refresh(&mut self, repo_index: usize) {
        if let Some(managed) = self.repos.get_mut(repo_index) {
            managed.wt_hook_logs.refresh_pending = true;
        }
        self.start_pending_wt_hook_log_refreshes();
    }

    pub(crate) fn request_worktrunk_refreshes(&mut self, repo_index: usize) {
        self.request_wt_poll(repo_index);
        self.request_wt_hook_log_refresh(repo_index);
    }

    pub(crate) fn start_pending_wt_hook_log_refreshes(&mut self) {
        for repo_index in 0..self.repos.len() {
            let Some(managed) = self.repos.get(repo_index) else {
                continue;
            };
            if !managed.wt_hook_logs.refresh_pending || managed.wt_hook_logs.refresh_in_flight {
                continue;
            }
            let repo = managed.repo.clone();
            let config = managed.config.clone();
            let repository = managed.identity.clone();
            if let Some(managed) = self.repos.get_mut(repo_index) {
                managed.wt_hook_logs.refresh_pending = false;
                managed.wt_hook_logs.refresh_in_flight = true;
                managed.wt_hook_logs.quality = crate::worktrunk::ObservationQuality::Refreshing;
            }
            let result_repository = repository.clone();
            self.spawn_tui_job(
                TuiJobKind::WorktrunkHookLogs,
                TuiJobKey::WorktrunkHookLogs(repository),
                0,
                Some(TUI_ACTION_JOB_TIMEOUT),
                format!("prism-worktrunk-logs-{repo_index}"),
                move |_| {
                    let observation =
                        crate::worktrunk::observe_hook_logs(&repo, &config).map(|entries| {
                            crate::tui::WtHookLogObservation {
                                entries,
                                observed_at: std::time::Instant::now(),
                            }
                        });
                    Ok(Some(TuiJobPayload::WorktrunkHookLogs(
                        crate::tui::WtHookLogPollResult {
                            repository: result_repository,
                            observation,
                        },
                    )))
                },
            );
        }
    }

    pub(crate) fn poll_wt_hook_logs(&mut self) -> bool {
        let mut changed = false;
        while let Ok(result) = self.wt_hook_log_poll_rx.try_recv() {
            let Some(repo_index) = self
                .repos
                .iter()
                .position(|repo| repo.identity == result.repository)
            else {
                continue;
            };
            match result.observation {
                Ok(observation) => {
                    let managed = &mut self.repos[repo_index];
                    changed |= managed.wt_hook_logs.entries != observation.entries
                        || managed.wt_hook_logs.quality
                            != crate::worktrunk::ObservationQuality::Fresh;
                    managed.wt_hook_logs.entries = observation.entries;
                    managed.wt_hook_logs.last_success = Some(observation.observed_at);
                    managed.wt_hook_logs.last_error = None;
                    managed.wt_hook_logs.quality = crate::worktrunk::ObservationQuality::Fresh;
                }
                Err(error) => {
                    changed |= self.mark_wt_hook_logs_stale(repo_index, error.safe_summary());
                }
            }
        }
        changed
    }

    pub(crate) fn mark_wt_hook_logs_stale(&mut self, repo_index: usize, error: String) -> bool {
        let Some(managed) = self.repos.get_mut(repo_index) else {
            return false;
        };
        let previous = managed.wt_hook_logs.quality.clone();
        managed.wt_hook_logs.last_error = Some(error.clone());
        managed.wt_hook_logs.quality = managed
            .wt_hook_logs
            .last_success
            .map(|last_success| crate::worktrunk::ObservationQuality::Stale {
                last_success,
                error,
            })
            .unwrap_or(crate::worktrunk::ObservationQuality::NeverLoaded);
        managed.wt_hook_logs.quality != previous
    }

    pub(crate) fn show_selected_worktrunk_logs(
        &mut self,
        runtime: &mut dyn crate::tui_runtime::TerminalDriver,
    ) -> Result<(), String> {
        if !self.is_worktree_session_panel() {
            return Err("focus worktrees or merges to inspect hook logs".to_string());
        }
        let context = self
            .selected_worktree_context()
            .ok_or_else(|| "no worktree selected".to_string())?;
        let selected_branch = self.sessions[context.session_index].branch.clone();
        let repo_index = self.sessions[context.session_index].repo_index;
        self.request_wt_hook_log_refresh(repo_index);
        self.dialog = Some(crate::view::DialogModel::Progress {
            title: "Worktrunk Hook Logs".to_string(),
            message: "Refreshing log inventory (Esc closes)".to_string(),
        });
        self.draw(runtime)?;
        loop {
            self.tick_tui_action_jobs();
            let refreshing = self.repos.get(repo_index).is_some_and(|repo| {
                repo.wt_hook_logs.refresh_in_flight
                    || repo.wt_hook_logs.refresh_pending
                    || matches!(
                        repo.wt_hook_logs.quality,
                        crate::worktrunk::ObservationQuality::Refreshing
                    )
            });
            if !refreshing {
                break;
            }
            if let Some(crate::tui_runtime::RuntimeEvent::Key(event)) =
                runtime.poll_event(std::time::Duration::from_millis(50))?
                && event.kind == crossterm::event::KeyEventKind::Press
                && matches!(event.code, crossterm::event::KeyCode::Esc)
            {
                self.dialog = None;
                self.draw(runtime)?;
                return Ok(());
            }
        }
        self.dialog = None;
        self.draw(runtime)?;
        let inventory = self
            .repos
            .get(repo_index)
            .map(|repo| repo.wt_hook_logs.clone())
            .ok_or_else(|| "repository is no longer available".to_string())?;
        let mut entries = inventory.entries;
        entries.sort_by_key(|entry| entry.branch != selected_branch);
        if entries.is_empty() {
            let refresh_failed = inventory.last_error.is_some();
            let text = inventory
                .last_error
                .as_ref()
                .map(|error| format!("Worktrunk hook-log refresh failed: {error}"))
                .unwrap_or_else(|| "No Worktrunk hook logs were reported.".to_string());
            return self.notice_dialog(
                runtime,
                "Worktrunk Hook Logs",
                vec![crate::view::DialogLine {
                    text,
                    attention: refresh_failed,
                }],
            );
        }
        let keys = super::worktrees::archive_choice_keys()
            .into_iter()
            .filter(|key| key != "n" && key != "p")
            .collect::<Vec<_>>();
        let mut page = 0usize;
        let index = loop {
            let start = page * keys.len();
            let mut choices = entries
                .iter()
                .skip(start)
                .take(keys.len())
                .enumerate()
                .map(|(index, entry)| {
                    crate::view::KeyChoice::new(
                        keys[index].clone(),
                        format!(
                            "{}  {} {}  {}  {} bytes  {}",
                            entry.branch,
                            entry.source,
                            entry.hook_type.as_deref().unwrap_or("hook"),
                            entry.name,
                            entry.size,
                            entry.modified_at
                        ),
                    )
                })
                .collect::<Vec<_>>();
            if start > 0 {
                choices.push(crate::view::KeyChoice::new("p", "previous page"));
            }
            if start + keys.len() < entries.len() {
                choices.push(crate::view::KeyChoice::new("n", "next page"));
            }
            let Some(choice) = self.prompt_choice_dialog(
                runtime,
                crate::view::ChoiceList {
                    title: if matches!(
                        inventory.quality,
                        crate::worktrunk::ObservationQuality::Stale { .. }
                    ) {
                        format!("Worktrunk Hook Logs (stale) - page {}", page + 1)
                    } else {
                        format!("Worktrunk Hook Logs - page {}", page + 1)
                    },
                    choices,
                },
            )?
            else {
                return Ok(());
            };
            match choice.as_str() {
                "n" => page += 1,
                "p" => page = page.saturating_sub(1),
                _ => {
                    let offset = keys
                        .iter()
                        .position(|key| key == &choice)
                        .ok_or_else(|| "invalid hook log selection".to_string())?;
                    break start + offset;
                }
            }
        };
        let entry = entries
            .get(index)
            .ok_or_else(|| "hook log selection is no longer available".to_string())?;
        let repo = context.repo.clone();
        let path = entry.path.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("prism-worktrunk-log-tail".to_string())
            .spawn(move || {
                let _ = sender.send(crate::worktrunk::read_hook_log_tail(&repo, &path));
            })
            .map_err(|error| format!("start Worktrunk log read: {error}"))?;
        let Some(tail) = self.wait_for_dialog_job(
            runtime,
            "Worktrunk Hook Log",
            "Reading bounded log tail (Esc cancels)",
            receiver,
        )?
        else {
            return Ok(());
        };
        let tail = tail?;
        let mut lines = vec![crate::view::DialogLine {
            text: format!(
                "branch: {}\nsource: {}\nhook: {} {}\nmodified: {}\nsize: {} bytes\n",
                entry.branch,
                entry.source,
                entry.hook_type.as_deref().unwrap_or("hook"),
                entry.name,
                entry.modified_at,
                entry.size
            ),
            attention: false,
        }];
        lines.push(crate::view::DialogLine {
            text: "j/k or arrows scroll; Enter/Esc/q closes".to_string(),
            attention: false,
        });
        lines.extend(tail.into_iter().map(|text| crate::view::DialogLine {
            text,
            attention: false,
        }));
        self.notice_dialog(runtime, "Worktrunk Hook Log", lines)
    }
}
