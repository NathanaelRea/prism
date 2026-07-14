use super::*;

pub(super) fn ensure_repo_config_file(
    path: &Path,
    include_worktree_columns: bool,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("create config dir: {error}"))?;
    }
    if path.exists() {
        if include_worktree_columns {
            let mut text =
                fs::read_to_string(path).map_err(|error| format!("read config file: {error}"))?;
            if !text.contains("[worktrees]") {
                if !text.ends_with('\n') && !text.is_empty() {
                    text.push('\n');
                }
                text.push_str("\n[worktrees]\ncolumns = []\n");
                fs::write(path, text).map_err(|error| format!("update config file: {error}"))?;
            }
        }
        return Ok(());
    }
    let text = crate::config::repo_config_template(include_worktree_columns);
    fs::write(path, text).map_err(|error| format!("create config file: {error}"))
}

pub(super) fn ensure_user_config_file(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("create config dir: {error}"))?;
    }
    if path.exists() {
        return Ok(());
    }
    let text = crate::config::user_config_template();
    fs::write(path, text).map_err(|error| format!("create user config file: {error}"))
}

pub(super) fn editor_command() -> Option<String> {
    std::env::var("VISUAL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var("EDITOR")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .or_else(|| {
            ["nvim", "vim", "vi"]
                .into_iter()
                .find(|editor| command_exists(editor))
                .map(str::to_string)
        })
}

impl Tui {
    pub(crate) fn edit_config(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let context = self
            .selected_repo_context()
            .ok_or_else(|| "no selected repository".to_string())?;
        ensure_repo_config_file(&context.config.repo_config_path, false)?;
        let editor =
            editor_command().ok_or_else(|| "no editor found; set VISUAL or EDITOR".to_string())?;
        raw.suspend()?;
        let result = Command::new(&editor)
            .arg(&context.config.repo_config_path)
            .status();
        let resume_result = raw.resume();
        resume_result?;
        let status = result.map_err(|error| format!("{editor}: {error}"))?;
        if !status.success() {
            return Err(format!("{editor} exited with {status}"));
        }
        let config = crate::config::Config::load(&context.repo);
        if let Some(repo) = self.repos.get_mut(context.repo_index) {
            repo.config = config.clone();
        }
        self.sync_selected_repo_context();
        self.refresh_sessions()?;
        self.start_tmux_agent_warmup();
        self.start_wt_column_poll();
        self.show_message("config reloaded")?;
        Ok(())
    }

    pub(crate) fn edit_user_config(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let path = self
            .repos
            .get(self.current_repo)
            .map(|repo| repo.config.user_path.clone())
            .ok_or_else(|| "no selected repository".to_string())?;
        ensure_user_config_file(&path)?;
        let editor =
            editor_command().ok_or_else(|| "no editor found; set VISUAL or EDITOR".to_string())?;
        raw.suspend()?;
        let result = Command::new(&editor).arg(&path).status();
        let resume_result = raw.resume();
        resume_result?;
        let status = result.map_err(|error| format!("{editor}: {error}"))?;
        if !status.success() {
            return Err(format!("{editor} exited with {status}"));
        }
        for repo in &mut self.repos {
            repo.config = Config::load(&repo.repo);
        }
        self.sync_selected_repo_context();
        self.refresh_sessions()?;
        self.start_tmux_agent_warmup();
        self.start_wt_column_poll();
        self.show_message("user config reloaded")?;
        Ok(())
    }

    pub(crate) fn edit_worktree_columns(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let context = self
            .selected_repo_context()
            .ok_or_else(|| "no selected repository".to_string())?;
        ensure_repo_config_file(&context.config.repo_config_path, true)?;
        let (repo_label, configured_columns) = self
            .repos
            .get(context.repo_index)
            .map(|repo| (repo.label.clone(), repo.config.worktree_columns.clone()))
            .ok_or_else(|| "no selected repository".to_string())?;
        let items =
            worktree_column_choices(&configured_columns, &self.sessions, context.repo_index);
        let Some(columns) =
            self.ordered_toggle_dialog(raw, &format!("Worktree Columns: {repo_label}"), items)?
        else {
            return Ok(());
        };
        update_worktree_columns_config(&context.config.repo_config_path, &columns)?;
        let config = crate::config::Config::load(&context.repo);
        if let Some(repo) = self.repos.get_mut(context.repo_index) {
            repo.config = config.clone();
        }
        self.sync_selected_repo_context();
        self.start_wt_column_poll();
        self.show_message("worktree columns updated")?;
        Ok(())
    }

    pub(crate) fn add_repository(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let old_roots = self
            .repos
            .iter()
            .map(|repo| repo.repo.root.clone())
            .collect::<BTreeSet<_>>();
        let Some(path) = self.prompt_line_dialog(raw, "Add Repository", "Base/main path: ", "")?
        else {
            return Ok(());
        };
        let path = path.trim();
        if path.is_empty() {
            return Ok(());
        }
        let (_, index, entries) = crate::workspace::ensure_repo_entry(Path::new(path))?;
        self.reload_repositories(entries)?;
        self.select_repo(index);
        self.start_tmux_agent_warmup();
        self.start_wt_column_poll();
        self.start_default_branch_status_poll(true);
        if let Some(context) = self.selected_repo_context()
            && !old_roots.contains(&context.repo.root)
        {
            let _ = refresh_repo_policy_cache(&context.repo, &context.repo.root, &context.config);
            self.offer_worktrunk_approval_if_pending(raw, &context.repo, &context.config)?;
        }
        self.show_message("repository added")?;
        Ok(())
    }

    pub(crate) fn edit_repositories(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let path = crate::workspace::repos_path();
        if !path.exists() {
            let entries = self
                .repos
                .iter()
                .map(|repo| crate::workspace::RepoEntry {
                    root: repo.repo.root.clone(),
                    key: repo.key,
                })
                .collect::<Vec<_>>();
            crate::workspace::save_entries(&entries)?;
        }
        let editor =
            editor_command().ok_or_else(|| "no editor found; set VISUAL or EDITOR".to_string())?;
        raw.suspend()?;
        let result = Command::new(&editor).arg(&path).status();
        let resume_result = raw.resume();
        resume_result?;
        let status = result.map_err(|error| format!("{editor}: {error}"))?;
        if !status.success() {
            return Err(format!("{editor} exited with {status}"));
        }
        let entries = crate::workspace::load_entries();
        if entries.is_empty() {
            return Err("repository list is empty; add at least one [[repos]] block".to_string());
        }
        let old_roots = self
            .repos
            .iter()
            .map(|repo| repo.repo.root.clone())
            .collect::<BTreeSet<_>>();
        let current_root = self
            .selected_repo_context()
            .map(|context| context.repo.root)
            .unwrap_or_else(|| self.repo.root.clone());
        self.reload_repositories(entries)?;
        let index = self
            .repos
            .iter()
            .position(|repo| repo.repo.root == current_root)
            .unwrap_or(0);
        self.select_repo(index);
        self.start_tmux_agent_warmup();
        self.start_wt_column_poll();
        self.start_default_branch_status_poll(true);
        let new_repos = self
            .repos
            .iter()
            .filter(|repo| !old_roots.contains(&repo.repo.root))
            .map(|repo| (repo.repo.clone(), repo.config.clone()))
            .collect::<Vec<_>>();
        for (repo, config) in new_repos {
            let _ = refresh_repo_policy_cache(&repo, &repo.root, &config);
            self.offer_worktrunk_approval_if_pending(raw, &repo, &config)?;
        }
        self.show_message("repositories reloaded")?;
        Ok(())
    }

    pub(crate) fn reorder_repositories(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let entries = crate::workspace::load_entries();
        let items = repository_order_choices(&entries);
        let Some(order) = self.ordered_toggle_dialog(raw, "Repositories", items)? else {
            return Ok(());
        };
        let updated = repository_entries_for_order(&entries, &order)?;
        if updated.is_empty() {
            self.show_message("at least one repository must remain")?;
            return Ok(());
        }
        let retained_roots = updated
            .iter()
            .map(|entry| entry.root.as_path())
            .collect::<BTreeSet<_>>();
        let removed = entries
            .iter()
            .filter(|entry| !retained_roots.contains(entry.root.as_path()))
            .collect::<Vec<_>>();
        if !removed.is_empty() {
            let lines = removed
                .iter()
                .map(|entry| crate::view::DialogLine {
                    text: entry.root.display().to_string(),
                    attention: true,
                })
                .collect();
            let prompt = if removed.len() == 1 {
                "Remove this repository from Prism?"
            } else {
                "Remove these repositories from Prism?"
            };
            if !self.confirm_dialog(raw, "Remove Repositories", lines, prompt, false)? {
                return Ok(());
            }
        }

        let current_root = self
            .selected_repo_context()
            .map(|context| context.repo.root);
        crate::workspace::save_entries(&updated)?;
        self.reload_repositories(updated)?;
        let index = current_root
            .and_then(|root| self.repos.iter().position(|repo| repo.repo.root == root))
            .unwrap_or_else(|| self.current_repo.min(self.repos.len().saturating_sub(1)));
        self.select_repo(index);
        self.start_tmux_agent_warmup();
        self.start_wt_column_poll();
        self.start_default_branch_status_poll(true);
        self.show_message("repositories updated")?;
        Ok(())
    }

    pub(super) fn offer_worktrunk_approval_if_pending(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
        repo: &Repository,
        config: &Config,
    ) -> Result<(), String> {
        let status = match check_worktrunk_approval_status(repo, config) {
            Ok(status) => status,
            Err(error) => {
                let _ =
                    append_runtime_log(repo, &format!("Worktrunk approval check skipped: {error}"));
                return Ok(());
            }
        };
        match status {
            WorktrunkApprovalStatus::Pending => {
                if self.offer_worktrunk_approval(raw, repo, config)? {
                    match check_worktrunk_approval_status(repo, config)? {
                        WorktrunkApprovalStatus::Pending => {
                            self.show_message("Worktrunk approvals still pending")?;
                        }
                        WorktrunkApprovalStatus::Approved => {
                            self.show_message("Worktrunk approvals enabled")?;
                        }
                        WorktrunkApprovalStatus::NotWorktrunk => {}
                    }
                }
            }
            WorktrunkApprovalStatus::Approved | WorktrunkApprovalStatus::NotWorktrunk => {}
        }
        Ok(())
    }

    pub(super) fn offer_worktrunk_approval(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
        repo: &Repository,
        config: &Config,
    ) -> Result<bool, String> {
        let command = format!("wt -C {} config approvals add", repo.root.display());
        let lines = vec![
            crate::view::DialogLine {
                text: "This repo has Worktrunk project commands that must be approved before Prism can create worktrees.".to_string(),
                attention: true,
            },
            crate::view::DialogLine {
                text: command,
                attention: false,
            },
        ];
        if !self.confirm_dialog(
            raw,
            "Worktrunk Approvals",
            lines,
            "Run Worktrunk's approval prompt now?",
            true,
        )? {
            return Ok(false);
        }
        raw.suspend_for(|| run_worktrunk_approval_prompt(repo, config))?;
        Ok(true)
    }

    pub(super) fn reload_repositories(
        &mut self,
        entries: Vec<crate::workspace::RepoEntry>,
    ) -> Result<(), String> {
        let mut repos = Vec::new();
        for entry in crate::workspace::discover_valid_entries(entries) {
            let repo = entry.repo;
            let config = crate::config::Config::load(&repo);
            repos.push(ManagedRepo::new(repo, config, entry.key));
        }
        self.repos = repos;
        self.current_repo = self.current_repo.min(self.repos.len().saturating_sub(1));
        self.selected_repo_root = self
            .repos
            .get(self.current_repo)
            .map(|repo| repo.repo.root.clone());
        self.refresh_sessions()?;
        self.sync_selected_repo_context();
        Ok(())
    }
}

pub(super) fn worktree_column_choices(
    configured: &[String],
    sessions: &[crate::session::Session],
    repo_index: usize,
) -> Vec<crate::view::OrderedToggleItem> {
    let configured_set = configured.iter().cloned().collect::<BTreeSet<_>>();
    let mut discovered = sessions
        .iter()
        .filter(|session| session.repo_index == repo_index)
        .flat_map(|session| session.wt_columns.keys().cloned())
        .filter(|key| !configured_set.contains(key))
        .collect::<BTreeSet<_>>();
    let mut choices = configured
        .iter()
        .map(|key| crate::view::OrderedToggleItem {
            id: key.clone(),
            label: key.clone(),
            enabled: true,
        })
        .collect::<Vec<_>>();
    choices.extend(
        discovered
            .pop_first()
            .into_iter()
            .chain(std::iter::from_fn(move || discovered.pop_first()))
            .map(|key| crate::view::OrderedToggleItem {
                id: key.clone(),
                label: key,
                enabled: false,
            }),
    );
    choices
}

pub(super) fn repository_order_choices(
    entries: &[crate::workspace::RepoEntry],
) -> Vec<crate::view::OrderedToggleItem> {
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| crate::view::OrderedToggleItem {
            id: index.to_string(),
            label: crate::workspace::label_for_root(&entry.root),
            enabled: true,
        })
        .collect()
}

pub(super) fn repository_entries_for_order(
    entries: &[crate::workspace::RepoEntry],
    order: &[String],
) -> Result<Vec<crate::workspace::RepoEntry>, String> {
    order
        .iter()
        .map(|id| {
            let index = id
                .parse::<usize>()
                .map_err(|_| format!("invalid repository order id: {id}"))?;
            entries
                .get(index)
                .cloned()
                .ok_or_else(|| format!("unknown repository order id: {id}"))
        })
        .collect()
}

pub(super) fn update_worktree_columns_config(
    path: &Path,
    columns: &[String],
) -> Result<(), String> {
    let mut text =
        fs::read_to_string(path).map_err(|error| format!("read config file: {error}"))?;
    let line = format!(
        "columns = [{}]",
        columns
            .iter()
            .map(|column| serde_json::to_string(column).unwrap_or_else(|_| "\"\"".to_string()))
            .collect::<Vec<_>>()
            .join(", ")
    );
    text = set_worktree_columns_text(&text, &line);
    fs::write(path, text).map_err(|error| format!("write config file: {error}"))
}

pub(super) fn set_worktree_columns_text(text: &str, columns_line: &str) -> String {
    let mut lines = text.lines().map(str::to_string).collect::<Vec<_>>();
    let worktrees_index = lines.iter().position(|line| line.trim() == "[worktrees]");
    let Some(worktrees_index) = worktrees_index else {
        let mut updated = text.trim_end_matches('\n').to_string();
        if !updated.is_empty() {
            updated.push_str("\n\n");
        }
        updated.push_str("[worktrees]\n");
        updated.push_str(columns_line);
        updated.push('\n');
        return updated;
    };

    let table_end = lines
        .iter()
        .enumerate()
        .skip(worktrees_index + 1)
        .find(|(_, line)| line.trim_start().starts_with('['))
        .map(|(index, _)| index)
        .unwrap_or(lines.len());
    if let Some(columns_index) = lines[worktrees_index + 1..table_end]
        .iter()
        .position(|line| line.trim_start().starts_with("columns"))
        .map(|index| worktrees_index + 1 + index)
    {
        let indent = lines[columns_index]
            .chars()
            .take_while(|ch| ch.is_whitespace())
            .collect::<String>();
        lines[columns_index] = format!("{indent}{columns_line}");
    } else {
        lines.insert(worktrees_index + 1, columns_line.to_string());
    }
    let mut updated = lines.join("\n");
    updated.push('\n');
    updated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_order_preserves_keys_and_omits_disabled_entries() {
        let entries = vec![
            crate::workspace::RepoEntry {
                root: PathBuf::from("/repos/one"),
                key: Some('1'),
            },
            crate::workspace::RepoEntry {
                root: PathBuf::from("/repos/two"),
                key: Some('2'),
            },
            crate::workspace::RepoEntry {
                root: PathBuf::from("/repos/three"),
                key: Some('3'),
            },
        ];

        let choices = repository_order_choices(&entries);
        assert_eq!(
            choices
                .iter()
                .map(|choice| (choice.id.as_str(), choice.label.as_str()))
                .collect::<Vec<_>>(),
            vec![("0", "one"), ("1", "two"), ("2", "three")]
        );

        let reordered =
            repository_entries_for_order(&entries, &["2".to_string(), "0".to_string()]).unwrap();
        assert_eq!(reordered, vec![entries[2].clone(), entries[0].clone()]);
    }

    #[test]
    fn repository_order_can_retain_entries_that_are_not_discovered() {
        let entries = vec![
            crate::workspace::RepoEntry {
                root: PathBuf::from("/repos/available"),
                key: Some('1'),
            },
            crate::workspace::RepoEntry {
                root: PathBuf::from("/repos/unavailable"),
                key: Some('2'),
            },
        ];

        let reordered =
            repository_entries_for_order(&entries, &["1".to_string(), "0".to_string()]).unwrap();

        assert_eq!(reordered, vec![entries[1].clone(), entries[0].clone()]);
    }
}
