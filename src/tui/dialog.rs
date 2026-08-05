use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::tui_runtime::{RuntimeEvent, TerminalRuntime};
use crate::view;
use crate::workspace_state::{InspectRequest, WorkspaceContext, WorkspaceState};

use super::{STATUS_MESSAGE_DURATION, Tui};

fn plain_key(event: KeyEvent) -> bool {
    event
        .modifiers
        .intersection(KeyModifiers::CONTROL | KeyModifiers::ALT)
        .is_empty()
}

pub(super) fn ctrl_key(event: KeyEvent) -> bool {
    event.modifiers.contains(KeyModifiers::CONTROL)
}

pub(super) fn confirmation_result(input: &str, default: bool) -> Option<bool> {
    match input.trim().to_ascii_lowercase().as_str() {
        "" => Some(default),
        "y" => Some(true),
        "n" => Some(false),
        _ => None,
    }
}

pub(super) fn toggle_ordered_item(items: &mut Vec<view::OrderedToggleItem>, selected: &mut usize) {
    if items.is_empty() || *selected >= items.len() {
        return;
    }
    let mut item = items.remove(*selected);
    item.enabled = !item.enabled;
    let insert_at = if item.enabled {
        items.iter().take_while(|item| item.enabled).count()
    } else {
        items.len()
    };
    items.insert(insert_at, item);
    *selected = insert_at;
}

pub(super) fn toggle_item_in_place(items: &mut [view::OrderedToggleItem], selected: usize) {
    if let Some(item) = items.get_mut(selected) {
        item.enabled = !item.enabled;
    }
}

pub(super) fn move_enabled_ordered_item(
    items: &mut [view::OrderedToggleItem],
    selected: &mut usize,
    direction: isize,
) {
    if items.is_empty() || *selected >= items.len() || !items[*selected].enabled {
        return;
    }
    let target = if direction < 0 {
        (0..*selected).rev().find(|index| items[*index].enabled)
    } else {
        (*selected + 1..items.len()).find(|index| items[*index].enabled)
    };
    if let Some(target) = target {
        items.swap(*selected, target);
        *selected = target;
    }
}

pub(super) fn selectable_choice_key(choices: &view::ChoiceList, key: &str) -> Option<String> {
    choices
        .choices
        .iter()
        .find(|option| !option.disabled && option.key.eq_ignore_ascii_case(key))
        .map(|option| option.key.to_ascii_lowercase())
}

pub(super) fn choice_list(title: &str, choices: &[(&str, &str)]) -> view::ChoiceList {
    view::ChoiceList {
        title: title.to_string(),
        choices: choices
            .iter()
            .map(|(key, label)| view::KeyChoice::new(*key, *label))
            .collect(),
    }
}

impl Tui {
    pub(super) fn show_keybindings_dialog(
        &mut self,
        runtime: &mut TerminalRuntime,
    ) -> Result<(), String> {
        let items = [
            "1 / 2 / 3    focus status / repos / worktrees sidebars; 3 toggles repo/all worktrees",
            "0            focus main panel for the selected sidebar",
            "Tab / Shift-Tab  move focus between panels",
            "h/l, left/right arrows  repos: switch view; status plan: switch phase",
            "Enter       repos: open default-branch tmux; worktrees: open agent or selected plan phase; main comments: details",
            "Ctrl-/       open tmux window 3: terminal",
            "p            repos: pull default branch",
            "P            worktrees: start or focus a plan run dashboard",
            "j/k          main comments: move comment selection; status dashboard: move plan output or phase selection",
            "A            worktrees: start/focus Auto Flow; choose prompt, plan file, or draft plan",
            "Space g R    main comments: resolve all inline review conversations",
            "r            repos: reorder or remove repositories",
            "R            edit repositories/order/keys/remove in repos.toml",
            "C            repos: open a worktree for a remote pull request",
            "c            repos: create worktree session in selected repo",
            "+ / -        worktrees: raise/lower visibility sort",
            "x            worktrees: abort selected agent session when supported",
            "M            worktrees: migrate selected worktree to the default harness",
            "H            choose the global default harness or add a generic harness",
            "e            edit selected repository config, then reload",
            "E            edit user config, then reload",
            "w            edit Worktrunk user config; affects Prism and standalone wt",
            "W            repos: edit visible worktree columns in repo config",
            "o            worktrees: open the selected Worktrunk development URL",
            "L            worktrees: inspect bounded Worktrunk hook logs",
            "/            search/filter focused panel",
            "?            show keybindings; / filters this dialog",
            "D            archive non-default worktree/session",
            "U            repos: choose an archived worktree to unarchive",
            "X            permanently delete non-default worktree/session",
            "j/k, up/down move selection",
            "g g / G      top / bottom",
            "r            refresh outside the repos sidebar",
            "q, Ctrl-C    quit",
        ];
        let items = items
            .iter()
            .map(|item| (*item).to_string())
            .collect::<Vec<_>>();
        let mut filter = String::new();
        let mut editing_filter = false;
        let mut scroll = 0usize;
        let info_lines = view::keybinding_info_lines(self.focused_panel, self.config.icon_style);
        self.dialog = Some(view::DialogModel::Help {
            filter: filter.clone(),
            editing_filter,
            info_lines: info_lines.clone(),
            items: items.clone(),
            scroll,
        });
        self.draw(runtime)?;
        loop {
            if self.tick_tui_action_jobs().any() {
                self.draw(runtime)?;
            }
            let Some(event) = runtime.poll_event(Duration::from_millis(100))? else {
                continue;
            };
            let RuntimeEvent::Key(event) = event else {
                self.draw(runtime)?;
                continue;
            };
            if event.kind != KeyEventKind::Press {
                continue;
            }
            let mut close = false;
            match event.code {
                KeyCode::Char('/') if plain_key(event) && !editing_filter => {
                    editing_filter = true;
                    filter.clear();
                    scroll = 0;
                }
                KeyCode::Enter if editing_filter => editing_filter = false,
                KeyCode::Backspace if editing_filter => {
                    filter.pop();
                    scroll = 0;
                }
                KeyCode::Up | KeyCode::Char('k') if !editing_filter => {
                    scroll = scroll.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') if !editing_filter => {
                    scroll = scroll.saturating_add(1);
                }
                KeyCode::Esc => close = true,
                KeyCode::Char('c') if ctrl_key(event) => close = true,
                KeyCode::Char('q') if plain_key(event) => close = true,
                KeyCode::Char(ch) if editing_filter && plain_key(event) && !ch.is_control() => {
                    filter.push(ch);
                    scroll = 0;
                }
                _ if !editing_filter => close = true,
                _ => {}
            }
            if close {
                self.dialog = None;
                self.draw(runtime)?;
                return Ok(());
            }
            self.dialog = Some(view::DialogModel::Help {
                filter: filter.clone(),
                editing_filter,
                info_lines: info_lines.clone(),
                items: items.clone(),
                scroll,
            });
            self.draw(runtime)?;
        }
    }

    pub(crate) fn confirm_archive_dialog(
        &mut self,
        runtime: &mut TerminalRuntime,
        branch: &str,
        path: &str,
        warnings: &[String],
    ) -> Result<bool, String> {
        let mut lines = vec![
            view::DialogLine {
                text: format!("branch: {branch}"),
                attention: false,
            },
            view::DialogLine {
                text: format!("path: {path}"),
                attention: false,
            },
        ];
        if warnings.is_empty() {
            lines.push(view::DialogLine {
                text: "No warnings detected; worktree files stay on disk.".to_string(),
                attention: false,
            });
        } else {
            for warning in warnings {
                lines.push(view::DialogLine {
                    text: warning.clone(),
                    attention: true,
                });
            }
        }
        lines.push(view::DialogLine {
            text: "Archive hides this worktree from normal navigation. Restore with `git worktree list` and remove the archive marker from Prism state if needed.".to_string(),
            attention: false,
        });
        self.confirm_dialog(
            runtime,
            "Archive Session",
            lines,
            "Archive this session?",
            false,
        )
    }

    pub(crate) fn confirm_delete_dialog(
        &mut self,
        runtime: &mut TerminalRuntime,
        branch: &str,
        path: &str,
        warnings: &[String],
        default: bool,
    ) -> Result<bool, String> {
        let mut lines = vec![
            view::DialogLine {
                text: format!("branch: {branch}"),
                attention: false,
            },
            view::DialogLine {
                text: format!("path: {path}"),
                attention: false,
            },
        ];
        if warnings.is_empty() {
            lines.push(view::DialogLine {
                text: "No warnings detected.".to_string(),
                attention: false,
            });
        } else {
            for warning in warnings {
                lines.push(view::DialogLine {
                    text: warning.clone(),
                    attention: true,
                });
            }
        }
        self.confirm_dialog(
            runtime,
            "Delete Session",
            lines,
            "Delete this session?",
            default,
        )
    }

    pub(crate) fn prompt_line_dialog(
        &mut self,
        runtime: &mut TerminalRuntime,
        title: &str,
        prompt: &str,
        initial: &str,
    ) -> Result<Option<String>, String> {
        let mut input = initial.to_string();
        self.dialog = Some(view::DialogModel::Prompt {
            title: title.to_string(),
            prompt: prompt.to_string(),
            input: input.clone(),
        });
        self.draw(runtime)?;
        loop {
            if self.tick_tui_action_jobs().any() {
                self.draw(runtime)?;
            }
            let Some(event) = runtime.poll_event(Duration::from_millis(100))? else {
                continue;
            };
            let RuntimeEvent::Key(event) = event else {
                self.draw(runtime)?;
                continue;
            };
            if event.kind != KeyEventKind::Press {
                continue;
            }
            match event.code {
                KeyCode::Enter => {
                    self.dialog = None;
                    self.draw(runtime)?;
                    return Ok(Some(input));
                }
                KeyCode::Esc | KeyCode::Char('c')
                    if event.code == KeyCode::Esc || ctrl_key(event) =>
                {
                    self.dialog = None;
                    self.draw(runtime)?;
                    return Ok(None);
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(ch) if plain_key(event) && !ch.is_control() => {
                    input.push(ch);
                }
                _ => {}
            }
            self.dialog = Some(view::DialogModel::Prompt {
                title: title.to_string(),
                prompt: prompt.to_string(),
                input: input.clone(),
            });
            self.draw(runtime)?;
        }
    }

    pub(crate) fn prompt_choice_dialog(
        &mut self,
        runtime: &mut TerminalRuntime,
        choices: view::ChoiceList,
    ) -> Result<Option<String>, String> {
        self.dialog = Some(view::DialogModel::Choice {
            choices: choices.clone(),
        });
        self.draw(runtime)?;
        loop {
            if self.tick_tui_action_jobs().any() {
                self.draw(runtime)?;
            }
            let Some(event) = runtime.poll_event(Duration::from_millis(100))? else {
                continue;
            };
            let RuntimeEvent::Key(event) = event else {
                self.draw(runtime)?;
                continue;
            };
            if event.kind != KeyEventKind::Press {
                continue;
            }
            match event.code {
                KeyCode::Esc | KeyCode::Char('c')
                    if event.code == KeyCode::Esc || ctrl_key(event) =>
                {
                    self.dialog = None;
                    self.draw(runtime)?;
                    return Ok(None);
                }
                KeyCode::Char(ch) if plain_key(event) && !ch.is_control() => {
                    let normalized = ch.to_string().to_ascii_lowercase();
                    if selectable_choice_key(&choices, &normalized).is_some() {
                        self.dialog = None;
                        self.draw(runtime)?;
                        return Ok(Some(normalized));
                    }
                }
                _ => {}
            }
            self.dialog = Some(view::DialogModel::Choice {
                choices: choices.clone(),
            });
            self.draw(runtime)?;
        }
    }

    pub(crate) fn ordered_toggle_dialog(
        &mut self,
        runtime: &mut TerminalRuntime,
        title: &str,
        mut items: Vec<view::OrderedToggleItem>,
    ) -> Result<Option<Vec<String>>, String> {
        items.sort_by_key(|item| !item.enabled);
        let mut selected = 0usize;
        loop {
            self.dialog = Some(view::DialogModel::OrderedToggle {
                title: title.to_string(),
                items: items.clone(),
                selected,
                reorderable: true,
            });
            self.draw(runtime)?;
            if self.tick_tui_action_jobs().any() {
                self.draw(runtime)?;
            }
            let Some(event) = runtime.poll_event(Duration::from_millis(100))? else {
                continue;
            };
            let RuntimeEvent::Key(event) = event else {
                continue;
            };
            if event.kind != KeyEventKind::Press {
                continue;
            }
            match event.code {
                KeyCode::Esc | KeyCode::Char('c')
                    if event.code == KeyCode::Esc || ctrl_key(event) =>
                {
                    self.dialog = None;
                    self.draw(runtime)?;
                    return Ok(None);
                }
                KeyCode::Enter if plain_key(event) => {
                    self.dialog = None;
                    self.draw(runtime)?;
                    return Ok(Some(
                        items
                            .iter()
                            .filter(|item| item.enabled)
                            .map(|item| item.id.clone())
                            .collect(),
                    ));
                }
                KeyCode::Up | KeyCode::Char('k') if plain_key(event) => {
                    selected = selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') if plain_key(event) => {
                    selected = selected
                        .saturating_add(1)
                        .min(items.len().saturating_sub(1));
                }
                KeyCode::Char(' ') if plain_key(event) => {
                    toggle_ordered_item(&mut items, &mut selected);
                }
                KeyCode::Char('K') if plain_key(event) => {
                    move_enabled_ordered_item(&mut items, &mut selected, -1);
                }
                KeyCode::Char('J') if plain_key(event) => {
                    move_enabled_ordered_item(&mut items, &mut selected, 1);
                }
                _ => {}
            }
        }
    }

    pub(super) fn recovery_selection_dialog(
        &mut self,
        runtime: &mut TerminalRuntime,
        mut items: Vec<view::OrderedToggleItem>,
    ) -> Result<Option<Vec<String>>, String> {
        let mut selected = 0usize;
        loop {
            self.dialog = Some(view::DialogModel::OrderedToggle {
                title: "Restart interrupted work".to_string(),
                items: items.clone(),
                selected,
                reorderable: false,
            });
            self.draw(runtime)?;
            let Some(event) = runtime.poll_event(Duration::from_millis(100))? else {
                continue;
            };
            let RuntimeEvent::Key(event) = event else {
                continue;
            };
            if event.kind != KeyEventKind::Press {
                continue;
            }
            match event.code {
                KeyCode::Esc | KeyCode::Char('c')
                    if event.code == KeyCode::Esc || ctrl_key(event) =>
                {
                    self.dialog = None;
                    return Ok(None);
                }
                KeyCode::Enter if plain_key(event) => {
                    self.dialog = None;
                    return Ok(Some(
                        items
                            .iter()
                            .filter(|item| item.enabled)
                            .map(|item| item.id.clone())
                            .collect(),
                    ));
                }
                KeyCode::Up | KeyCode::Char('k') if plain_key(event) => {
                    selected = selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') if plain_key(event) => {
                    selected = selected
                        .saturating_add(1)
                        .min(items.len().saturating_sub(1));
                }
                KeyCode::Char(' ') if plain_key(event) => {
                    toggle_item_in_place(&mut items, selected);
                }
                _ => {}
            }
        }
    }

    pub(super) fn offer_interrupted_run_recovery(
        &mut self,
        runtime: &mut TerminalRuntime,
    ) -> Result<(), String> {
        let state = WorkspaceState::open(WorkspaceContext {
            repo: None,
            cwd: self.repo.root.clone(),
        })?;
        let snapshot = state.inspect(InspectRequest {
            include_hidden: true,
            include_terminal: true,
        })?;
        let candidates = snapshot
            .repositories
            .iter()
            .flat_map(|repository| {
                repository
                    .workflows
                    .iter()
                    .filter(|workflow| workflow.available_controls.recover)
                    .map(move |workflow| (repository, workflow))
            })
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return Ok(());
        }
        let now = crate::execution::now_ms();
        let items = candidates
            .iter()
            .enumerate()
            .map(|(index, (repository, workflow))| {
                let age_ms = workflow
                    .dispatch
                    .heartbeat_unix_ms
                    .map(|heartbeat| now.saturating_sub(heartbeat))
                    .unwrap_or(0);
                let age = if age_ms >= 60_000 {
                    format!("{}m ago", age_ms / 60_000)
                } else {
                    format!("{}s ago", age_ms / 1_000)
                };
                let kind = match workflow.identity.kind.as_str() {
                    "auto" => "Auto Flow",
                    _ => "Plan",
                };
                let step = workflow
                    .current_step
                    .as_ref()
                    .map(|step| step.label.as_str())
                    .unwrap_or(kind);
                view::OrderedToggleItem {
                    id: index.to_string(),
                    label: format!(
                        "{} / {}  {}  {}  {}",
                        repository.label, workflow.worktree.display, kind, step, age
                    ),
                    enabled: false,
                }
            })
            .collect();
        let Some(selected) = self.recovery_selection_dialog(runtime, items)? else {
            return Ok(());
        };
        let selected = selected.into_iter().collect::<BTreeSet<_>>();
        let decisions = candidates
            .iter()
            .enumerate()
            .map(
                |(index, (_, workflow))| crate::workspace_state::RecoveryDecision {
                    workflow: workflow.identity.clone(),
                    interruption_generation: workflow.dispatch.interruption_generation,
                    restart: selected.contains(&index.to_string()),
                },
            )
            .collect::<Vec<_>>();
        let receipt = state.recover_batch(&decisions)?;
        if !receipt.warnings.is_empty() {
            self.show_message(&receipt.warnings.join("; "))?;
        }
        Ok(())
    }

    pub(crate) fn show_loading_dialog(
        &mut self,
        runtime: &mut TerminalRuntime,
        title: &str,
        message: &str,
    ) -> Result<(), String> {
        self.dialog = Some(view::DialogModel::Progress {
            title: title.to_string(),
            message: message.to_string(),
        });
        self.draw(runtime)?;
        self.dialog = None;
        Ok(())
    }

    pub(crate) fn wait_for_dialog_job<T>(
        &mut self,
        runtime: &mut TerminalRuntime,
        title: &str,
        message: &str,
        receiver: std::sync::mpsc::Receiver<T>,
    ) -> Result<Option<T>, String> {
        self.dialog = Some(view::DialogModel::Progress {
            title: title.to_string(),
            message: message.to_string(),
        });
        self.draw(runtime)?;
        loop {
            match receiver.recv_timeout(Duration::from_millis(50)) {
                Ok(value) => {
                    self.dialog = None;
                    self.draw(runtime)?;
                    return Ok(Some(value));
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    self.dialog = None;
                    self.draw(runtime)?;
                    return Err("background dialog job stopped unexpectedly".to_string());
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
            if self.tick_tui_action_jobs().any() {
                self.draw(runtime)?;
            }
            if let Some(RuntimeEvent::Key(event)) = runtime.poll_event(Duration::ZERO)?
                && event.kind == KeyEventKind::Press
                && matches!(event.code, KeyCode::Esc)
            {
                self.dialog = None;
                self.draw(runtime)?;
                return Ok(None);
            }
        }
    }

    pub(crate) fn confirm_dialog(
        &mut self,
        runtime: &mut TerminalRuntime,
        title: &str,
        lines: Vec<view::DialogLine>,
        prompt: &str,
        default: bool,
    ) -> Result<bool, String> {
        let mut input = String::new();
        let mut invalid = false;
        self.dialog = Some(view::DialogModel::Confirm {
            title: title.to_string(),
            lines: lines.clone(),
            prompt: prompt.to_string(),
            input: input.clone(),
            default,
            invalid,
        });
        self.draw(runtime)?;
        loop {
            if self.tick_tui_action_jobs().any() {
                self.draw(runtime)?;
            }
            let Some(event) = runtime.poll_event(Duration::from_millis(100))? else {
                continue;
            };
            let RuntimeEvent::Key(event) = event else {
                self.draw(runtime)?;
                continue;
            };
            if event.kind != KeyEventKind::Press {
                continue;
            }
            match event.code {
                KeyCode::Enter if plain_key(event) => {
                    if let Some(result) = confirmation_result(&input, default) {
                        self.dialog = None;
                        self.draw(runtime)?;
                        return Ok(result);
                    }
                    input.clear();
                    invalid = true;
                }
                KeyCode::Esc | KeyCode::Char('c')
                    if event.code == KeyCode::Esc || ctrl_key(event) =>
                {
                    self.dialog = None;
                    self.draw(runtime)?;
                    return Ok(default);
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(ch) if plain_key(event) && !ch.is_control() => {
                    input.push(ch);
                }
                _ => {}
            }
            self.dialog = Some(view::DialogModel::Confirm {
                title: title.to_string(),
                lines: lines.clone(),
                prompt: prompt.to_string(),
                input: input.clone(),
                default,
                invalid,
            });
            self.draw(runtime)?;
        }
    }

    pub(crate) fn confirm_action_dialog(
        &mut self,
        runtime: &mut TerminalRuntime,
        title: &str,
        message: &str,
        default: bool,
    ) -> Result<bool, String> {
        self.confirm_dialog(runtime, title, vec![], message, default)
    }

    pub(crate) fn notice_dialog(
        &mut self,
        runtime: &mut TerminalRuntime,
        title: &str,
        lines: Vec<view::DialogLine>,
    ) -> Result<(), String> {
        self.dialog = Some(view::DialogModel::Notice {
            title: title.to_string(),
            lines: lines.clone(),
            scroll: 0,
        });
        self.draw(runtime)?;
        let mut scroll = 0usize;
        loop {
            let Some(event) = runtime.poll_event(Duration::from_millis(100))? else {
                continue;
            };
            if let RuntimeEvent::Key(event) = event
                && event.kind == KeyEventKind::Press
            {
                match event.code {
                    KeyCode::Down | KeyCode::Char('j') => scroll = scroll.saturating_add(1),
                    KeyCode::Up | KeyCode::Char('k') => scroll = scroll.saturating_sub(1),
                    KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                        self.dialog = None;
                        self.draw(runtime)?;
                        return Ok(());
                    }
                    _ => {}
                }
                self.dialog = Some(view::DialogModel::Notice {
                    title: title.to_string(),
                    lines: lines.clone(),
                    scroll,
                });
                self.draw(runtime)?;
            }
        }
    }

    pub(crate) fn show_message(&mut self, message: &str) -> Result<(), String> {
        self.status_message = Some(message.to_string());
        self.status_message_until = Some(Instant::now() + STATUS_MESSAGE_DURATION);
        let _ = crate::observability::append_runtime_message(&self.repo, message);
        Ok(())
    }

    pub(super) fn show_error(&mut self, context: &str, error: &str) -> Result<(), String> {
        let message = format!("{context}: {error}");
        self.show_message(&message)
    }
}
