use super::*;

impl Tui {
    pub(crate) fn show_selected_worktrunk_logs(
        &mut self,
        runtime: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        if self.focused_panel != crate::tui::PanelFocus::Worktrees {
            return Err("focus worktrees to inspect hook logs".to_string());
        }
        let context = self
            .selected_worktree_context()
            .ok_or_else(|| "no worktree selected".to_string())?;
        let selected_branch = self.sessions[context.session_index].branch.clone();
        let repo = context.repo.clone();
        let config = context.config.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("prism-worktrunk-logs".to_string())
            .spawn(move || {
                let result = crate::worktrunk::observe_hook_logs(&repo, &config)
                    .map_err(|error| error.to_string());
                let _ = sender.send(result);
            })
            .map_err(|error| format!("start Worktrunk log refresh: {error}"))?;
        let Some(result) = self.wait_for_dialog_job(
            runtime,
            "Worktrunk Hook Logs",
            "Refreshing log inventory (Esc cancels)",
            receiver,
        )?
        else {
            return Ok(());
        };
        let mut entries = result?;
        entries.sort_by_key(|entry| entry.branch != selected_branch);
        if entries.is_empty() {
            return self.notice_dialog(
                runtime,
                "Worktrunk Hook Logs",
                vec![crate::view::DialogLine {
                    text: "No Worktrunk hook logs were reported.".to_string(),
                    attention: false,
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
                    title: format!("Worktrunk Hook Logs - page {}", page + 1),
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
