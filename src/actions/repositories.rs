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
    pub(crate) fn select_default_harness(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
    ) -> Result<(), String> {
        let config = self.config.clone();
        let entries = harness_choice_entries(&config);
        let page_size = 4;
        let page_count = entries.len().div_ceil(page_size).max(1);
        let mut page = 0;
        let selected = loop {
            let start = page * page_size;
            let visible = &entries[start..entries.len().min(start + page_size)];
            let mut choices = visible
                .iter()
                .enumerate()
                .map(|(index, (id, label))| {
                    harness_key_choice(index, id, label, &config.default_harness)
                })
                .collect::<Vec<_>>();
            choices.push(crate::view::KeyChoice::new("a", "Add generic harness..."));
            if page > 0 {
                choices.push(crate::view::KeyChoice::new("p", "Previous page"));
            }
            if page + 1 < page_count {
                choices.push(crate::view::KeyChoice::new("n", "Next page"));
            }
            let title = if page_count > 1 {
                format!("Default Harness ({}/{page_count})", page + 1)
            } else {
                "Default Harness".to_string()
            };
            match self.prompt_choice_dialog(raw, crate::view::ChoiceList { title, choices })? {
                Some(choice) if choice == "a" => break None,
                Some(choice) if choice == "p" => page = page.saturating_sub(1),
                Some(choice) if choice == "n" => page = (page + 1).min(page_count - 1),
                Some(choice) => {
                    let index = choice
                        .parse::<usize>()
                        .ok()
                        .and_then(|index| index.checked_sub(1))
                        .ok_or_else(|| format!("unknown harness choice '{choice}'"))?;
                    let id = visible
                        .get(index)
                        .map(|(id, _)| id.clone())
                        .ok_or_else(|| format!("unknown harness choice '{choice}'"))?;
                    break Some(id);
                }
                None => return Ok(()),
            }
        };

        let harness_id = if let Some(id) = selected {
            if id == config.default_harness {
                self.show_message(&format!("'{id}' is already the default harness"))?;
                return Ok(());
            }
            config.save_user_default_harness(&id)?;
            id
        } else {
            let Some((id, harness)) = self.prompt_generic_harness(raw, &config)? else {
                return Ok(());
            };
            config.save_user_generic_harness(&id, &harness)?;
            id
        };

        self.config = Config::load(&self.repo);
        self.refresh_sessions()?;
        self.sync_selected_repo_context();
        self.start_tmux_agent_warmup();
        self.start_wt_column_poll();
        self.show_message(&format!(
            "default harness changed to '{harness_id}'; existing worktrees offer migration when opened"
        ))?;
        Ok(())
    }

    fn prompt_generic_harness(
        &mut self,
        raw: &mut crate::tui_runtime::TerminalRuntime,
        config: &Config,
    ) -> Result<Option<(String, HarnessConfig)>, String> {
        let Some(id) = self.prompt_line_dialog(
            raw,
            "Add Generic Harness",
            "Harness ID (lowercase letters, digits, - or _): ",
            "",
        )?
        else {
            return Ok(None);
        };
        let id = id.trim().to_string();
        crate::config::validate_new_generic_harness_id(&id, &config.harnesses)?;

        let Some(interactive) = self.prompt_line_dialog(
            raw,
            "Add Generic Harness",
            "Interactive command (include {prompt} or {prompt_file} if used): ",
            "",
        )?
        else {
            return Ok(None);
        };
        let interactive_command = parse_command_words(&interactive)?;
        let accepts_prompt_argument =
            command_supports_prompt_transport(&interactive_command, PromptTransport::Argument);
        let accepts_no_prompt =
            command_supports_prompt_transport(&interactive_command, PromptTransport::Stdin);
        let accepts_prompt_file =
            command_supports_prompt_transport(&interactive_command, PromptTransport::TempFile);
        let interactive_prompt_transport = match self.prompt_choice_dialog(
            raw,
            crate::view::ChoiceList {
                title: "Interactive Initial Prompt".to_string(),
                choices: vec![
                    if accepts_no_prompt {
                        crate::view::KeyChoice::new("n", "None")
                    } else {
                        crate::view::KeyChoice::disabled("n", "None")
                    },
                    if accepts_prompt_argument {
                        crate::view::KeyChoice::new("a", "Argument")
                    } else {
                        crate::view::KeyChoice::disabled("a", "Argument")
                    },
                    if accepts_prompt_file {
                        crate::view::KeyChoice::new("f", "Temporary file")
                    } else {
                        crate::view::KeyChoice::disabled("f", "Temporary file")
                    },
                ],
            },
        )? {
            Some(choice) if choice == "a" => Some(PromptTransport::Argument),
            Some(choice) if choice == "f" => Some(PromptTransport::TempFile),
            Some(_) => None,
            None => return Ok(None),
        };

        let Some(headless) = self.prompt_line_dialog(
            raw,
            "Add Generic Harness",
            "Headless command (optional; include a prompt placeholder unless using stdin): ",
            "",
        )?
        else {
            return Ok(None);
        };
        let (headless_command, headless_prompt_transport) = if headless.trim().is_empty() {
            (None, None)
        } else {
            let command = parse_command_words(&headless)?;
            let accepts_prompt_argument =
                command_supports_prompt_transport(&command, PromptTransport::Argument);
            let accepts_stdin = command_supports_prompt_transport(&command, PromptTransport::Stdin);
            let accepts_prompt_file =
                command_supports_prompt_transport(&command, PromptTransport::TempFile);
            let transport = match self.prompt_choice_dialog(
                raw,
                crate::view::ChoiceList {
                    title: "Headless Prompt Transport".to_string(),
                    choices: vec![
                        if accepts_prompt_argument {
                            crate::view::KeyChoice::new("a", "Argument")
                        } else {
                            crate::view::KeyChoice::disabled("a", "Argument")
                        },
                        if accepts_stdin {
                            crate::view::KeyChoice::new("s", "Standard input")
                        } else {
                            crate::view::KeyChoice::disabled("s", "Standard input")
                        },
                        if accepts_prompt_file {
                            crate::view::KeyChoice::new("f", "Temporary file")
                        } else {
                            crate::view::KeyChoice::disabled("f", "Temporary file")
                        },
                    ],
                },
            )? {
                Some(choice) if choice == "a" => PromptTransport::Argument,
                Some(choice) if choice == "s" => PromptTransport::Stdin,
                Some(choice) if choice == "f" => PromptTransport::TempFile,
                Some(choice) => return Err(format!("unknown prompt transport '{choice}'")),
                None => return Ok(None),
            };
            (Some(command), Some(transport))
        };

        let harness = HarnessConfig {
            adapter: "generic".to_string(),
            interactive_command,
            arguments: Vec::new(),
            interactive_prompt_transport,
            headless_command,
            headless_prompt_transport,
            output_format: OutputFormat::Text,
            environment: BTreeMap::new(),
        };
        harness.validate(&id)?;
        Ok(Some((id, harness)))
    }

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
                let _ = append_runtime_message(
                    repo,
                    &format!("Worktrunk approval check skipped: {error}"),
                );
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
        let identities = self
            .repos
            .iter()
            .map(|managed| (managed.repo.root.clone(), managed.identity.clone()))
            .collect::<BTreeMap<_, _>>();
        let mut repos = Vec::new();
        for entry in crate::workspace::discover_valid_entries(entries) {
            let repo = entry.repo;
            let config = crate::config::Config::load(&repo);
            let mut managed = ManagedRepo::new(repo, config, entry.key);
            if let Some(identity) = identities.get(&managed.repo.root) {
                managed.identity = identity.clone();
            }
            repos.push(managed);
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

fn harness_key_choice(
    index: usize,
    id: &str,
    label: &str,
    default_harness: &str,
) -> crate::view::KeyChoice {
    if id == default_harness {
        crate::view::KeyChoice::disabled((index + 1).to_string(), label)
    } else {
        crate::view::KeyChoice::new((index + 1).to_string(), label)
    }
}

fn command_supports_prompt_transport(command: &[String], transport: PromptTransport) -> bool {
    let prompt_count = command.iter().filter(|arg| *arg == "{prompt}").count();
    let file_count = command.iter().filter(|arg| *arg == "{prompt_file}").count();
    match transport {
        PromptTransport::Argument => prompt_count == 1 && file_count == 0,
        PromptTransport::Stdin => prompt_count == 0 && file_count == 0,
        PromptTransport::TempFile => prompt_count == 0 && file_count == 1,
    }
}

pub(super) fn harness_choice_entries(config: &Config) -> Vec<(String, String)> {
    let mut ids = crate::harness::BUILTIN_HARNESS_IDS
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    ids.extend(
        config
            .harnesses
            .iter()
            .filter(|(id, harness)| {
                crate::harness::builtin_adapter(id).is_none() && harness.adapter == "generic"
            })
            .map(|(id, _)| id.clone()),
    );
    ids.into_iter()
        .map(|id| {
            let name = match id.as_str() {
                "opencode" => "OpenCode".to_string(),
                "codex" => "Codex".to_string(),
                "claude" => "Claude Code".to_string(),
                "pi" => "Pi".to_string(),
                _ => id.clone(),
            };
            (id, name)
        })
        .collect()
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
    fn harness_choices_list_fixed_builtins_then_configured_generics() {
        let mut config = crate::test_support::test_config();
        for adapter in ["codex", "claude", "pi"] {
            config.harnesses.insert(
                adapter.to_string(),
                HarnessConfig::builtin(adapter, adapter),
            );
        }
        config.harnesses.insert(
            "company-agent".to_string(),
            HarnessConfig {
                adapter: "generic".to_string(),
                interactive_command: vec!["company-agent".to_string()],
                arguments: Vec::new(),
                interactive_prompt_transport: None,
                headless_command: None,
                headless_prompt_transport: None,
                output_format: OutputFormat::Text,
                environment: BTreeMap::new(),
            },
        );

        let choices = harness_choice_entries(&config);

        assert_eq!(
            choices
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            ["opencode", "codex", "claude", "pi", "company-agent"]
        );
        assert_eq!(choices[0].1, "OpenCode");
        assert_eq!(choices[4].1, "company-agent");
        assert!(harness_key_choice(0, &choices[0].0, &choices[0].1, "opencode").disabled);
    }

    #[test]
    fn harness_choices_do_not_drop_large_generic_configurations() {
        let mut config = crate::test_support::test_config();
        for adapter in ["codex", "claude", "pi"] {
            config.harnesses.insert(
                adapter.to_string(),
                HarnessConfig::builtin(adapter, adapter),
            );
        }
        for index in 0..40 {
            config.harnesses.insert(
                format!("generic-{index:02}"),
                HarnessConfig {
                    adapter: "generic".to_string(),
                    interactive_command: vec!["agent".to_string()],
                    arguments: Vec::new(),
                    interactive_prompt_transport: None,
                    headless_command: None,
                    headless_prompt_transport: None,
                    output_format: OutputFormat::Text,
                    environment: BTreeMap::new(),
                },
            );
        }

        assert_eq!(harness_choice_entries(&config).len(), 44);
    }

    #[test]
    fn prompt_transport_choices_require_the_matching_placeholder_shape() {
        let argument = vec!["agent".to_string(), "{prompt}".to_string()];
        assert!(command_supports_prompt_transport(
            &argument,
            PromptTransport::Argument
        ));
        assert!(!command_supports_prompt_transport(
            &argument,
            PromptTransport::Stdin
        ));
        let repeated = vec!["{prompt}".to_string(), "{prompt}".to_string()];
        assert!(!command_supports_prompt_transport(
            &repeated,
            PromptTransport::Argument
        ));
    }

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
