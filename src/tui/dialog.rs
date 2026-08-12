use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};
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

fn toggle_bool_field(value: &mut String) {
    *value = if value == "true" { "false" } else { "true" }.into();
}

fn selected_option(options: &[String], value: &str) -> usize {
    options
        .iter()
        .position(|option| option == value)
        .unwrap_or(0)
}

fn cycle_enum_field(value: &mut String, options: &[String], forward: bool) {
    if options.is_empty() {
        return;
    }
    if !options.iter().any(|option| option == value) {
        *value = options[0].clone();
        return;
    }
    let selected = selected_option(options, value);
    let next = if forward {
        (selected + 1).min(options.len() - 1)
    } else {
        selected.saturating_sub(1)
    };
    *value = options[next].clone();
}

fn validate_workflow_form(
    workflow: &crate::CompiledWorkflow,
    worktree: &Path,
    fields: &mut [view::FormField],
) -> Result<BTreeMap<String, String>, (usize, String)> {
    let mut supplied = BTreeMap::new();
    for (index, field) in fields.iter_mut().enumerate() {
        let input = workflow
            .inputs
            .get(&field.name)
            .expect("Workflow form fields come from the compiled input map");
        let raw = if field.value.is_empty() {
            let Some(default) = input.default_value() else {
                return Err((
                    index,
                    format!("Workflow input '{}' is required", field.name),
                ));
            };
            default
        } else {
            field.value.clone()
        };
        match crate::validate_workflow_input(worktree, input, &raw) {
            Ok(value) => {
                field.value = value.clone();
                supplied.insert(field.name.clone(), value);
            }
            Err(problem) => {
                return Err((index, format!("Workflow input '{}': {problem}", field.name)));
            }
        }
    }
    crate::bind_workflow_inputs(workflow, worktree, &supplied)
        .map(|bound| bound.input_values)
        .map_err(|problem| (0, problem.to_string()))
}

fn pick_workflow_file(
    fzf: &str,
    name: &str,
    worktree: &Path,
    workflow: &crate::CompiledWorkflow,
) -> Result<Option<String>, String> {
    let input = workflow
        .inputs
        .get(name)
        .ok_or_else(|| format!("unknown Workflow input '{name}'"))?;
    let candidates = crate::workflow_file_input_candidates(worktree, input)
        .map_err(|error| error.to_string())?;
    let glob = input.file_glob().unwrap_or_default();
    if candidates.is_empty() {
        return Err(format!(
            "no files match '{glob}' under {}",
            worktree.display()
        ));
    }
    let mut child = Command::new(fzf)
        .args([
            &format!("--prompt={name}> "),
            &format!("--header=Select a file matching {glob}"),
            "--height=80%",
            "--reverse",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("start fzf '{fzf}' for Workflow input '{name}': {error}"))?;
    {
        let stdin = child.stdin.as_mut().expect("fzf stdin is piped");
        for candidate in candidates {
            writeln!(stdin, "{candidate}")
                .map_err(|error| format!("write Workflow input candidates: {error}"))?;
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("wait for Workflow input picker: {error}"))?;
    if output
        .status
        .code()
        .is_some_and(|code| matches!(code, 1 | 130))
    {
        return Ok(None);
    }
    if !output.status.success() {
        return Err(format!(
            "Workflow input picker exited with {}",
            output.status
        ));
    }
    let selected = String::from_utf8(output.stdout)
        .map_err(|error| format!("Workflow input picker returned invalid UTF-8: {error}"))?;
    let selected = selected.trim_end_matches(['\r', '\n']);
    if selected.is_empty() {
        return Ok(None);
    }
    Ok(Some(selected.to_string()))
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
            "[ / ]        switch repository views or worktree list scope",
            "h/l, left/right arrows  repos: switch view",
            "Enter       repos: open default-branch tmux; worktrees: open agent; main comments: details",
            "Ctrl-/       open tmux window 3: terminal",
            "p            repos: pull default branch",
            "W / Space w w  open the flat Workflow run/edit picker",
            "Space w a    create an AI one-off Workflow for the selected worktree",
            "Space c      open the unified configuration tree",
            "{ / }        cycle Workflow Runs for the selected worktree",
            "u / f        pause or resume the selected Workflow Run / retry its failed Step",
            "j/k          move the selection in the focused dashboard or comments panel",
            "Space g R    main comments: resolve all inline review conversations",
            "r            repos: reorder or remove repositories",
            "C            repos: open a worktree for a remote pull request",
            "c            repos: create worktree session in selected repo",
            "> / <        worktrees: raise/lower priority",
            "x            cancel selected Workflow Run, otherwise abort selected agent session",
            "M            worktrees: migrate selected worktree to the default harness",
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

    pub(crate) fn prompt_workflow_input_form(
        &mut self,
        runtime: &mut TerminalRuntime,
        workflow: &crate::CompiledWorkflow,
        worktree: &Path,
        fzf: &str,
    ) -> Result<Option<BTreeMap<String, String>>, String> {
        if workflow.inputs.is_empty() {
            return Ok(Some(BTreeMap::new()));
        }
        let mut fields = workflow
            .inputs
            .iter()
            .map(|(name, input)| view::FormField {
                name: name.clone(),
                value: input.default_value().unwrap_or_default(),
                description: input.description().map(str::to_string),
                constraint: match input {
                    crate::CompiledWorkflowInput::String {
                        min_length,
                        max_length,
                        ..
                    } => match (min_length, max_length) {
                        (Some(min), Some(max)) => Some(format!("{min}–{max} chars")),
                        (Some(min), None) => Some(format!("at least {min} chars")),
                        (None, Some(max)) => Some(format!("at most {max} chars")),
                        (None, None) => None,
                    },
                    crate::CompiledWorkflowInput::Number { min, max, .. } => match (min, max) {
                        (Some(min), Some(max)) => Some(format!("{min}–{max}")),
                        (Some(min), None) => Some(format!("at least {min}")),
                        (None, Some(max)) => Some(format!("at most {max}")),
                        (None, None) => None,
                    },
                    crate::CompiledWorkflowInput::Enum { options, .. } => {
                        Some(format!("{} options", options.len()))
                    }
                    _ => None,
                },
                required: input.is_required(),
                kind: match input {
                    crate::CompiledWorkflowInput::File { glob, .. } => {
                        view::FormFieldKind::File { glob: glob.clone() }
                    }
                    crate::CompiledWorkflowInput::String { .. } => view::FormFieldKind::String,
                    crate::CompiledWorkflowInput::Bool { .. } => view::FormFieldKind::Bool,
                    crate::CompiledWorkflowInput::Number { .. } => view::FormFieldKind::Number,
                    crate::CompiledWorkflowInput::Enum { options, .. } => {
                        view::FormFieldKind::Enum {
                            options: options.clone(),
                        }
                    }
                },
            })
            .collect::<Vec<_>>();
        let mut selected = 0usize;
        let mut dropdown = None;
        let mut error = None;
        loop {
            self.dialog = Some(view::DialogModel::Form {
                title: format!("Run {}", workflow.name),
                fields: fields.clone(),
                selected,
                dropdown,
                error: error.clone(),
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
            error = None;

            if let Some(mut menu) = dropdown {
                let Some(view::FormField {
                    kind: view::FormFieldKind::Enum { options },
                    ..
                }) = fields.get_mut(selected)
                else {
                    dropdown = None;
                    continue;
                };
                match event.code {
                    KeyCode::Esc => dropdown = None,
                    KeyCode::Enter if plain_key(event) => {
                        if let Some(value) = options.get(menu.selected) {
                            fields[selected].value = value.clone();
                        }
                        dropdown = None;
                    }
                    KeyCode::Up | KeyCode::Char('k') if plain_key(event) => {
                        menu.selected = menu.selected.saturating_sub(1);
                        dropdown = Some(menu);
                    }
                    KeyCode::Down | KeyCode::Char('j') if plain_key(event) => {
                        menu.selected = menu
                            .selected
                            .saturating_add(1)
                            .min(options.len().saturating_sub(1));
                        dropdown = Some(menu);
                    }
                    KeyCode::Home if plain_key(event) => {
                        menu.selected = 0;
                        dropdown = Some(menu);
                    }
                    KeyCode::End if plain_key(event) => {
                        menu.selected = options.len().saturating_sub(1);
                        dropdown = Some(menu);
                    }
                    KeyCode::Char('c') if ctrl_key(event) => {
                        self.dialog = None;
                        self.draw(runtime)?;
                        return Ok(None);
                    }
                    _ => dropdown = Some(menu),
                }
                continue;
            }

            let field_kind = fields.get(selected).map(|field| field.kind.clone());
            match event.code {
                KeyCode::Esc | KeyCode::Char('c')
                    if event.code == KeyCode::Esc || ctrl_key(event) =>
                {
                    self.dialog = None;
                    self.draw(runtime)?;
                    return Ok(None);
                }
                KeyCode::Tab | KeyCode::Down if plain_key(event) => {
                    selected = selected.saturating_add(1).min(fields.len());
                }
                KeyCode::BackTab | KeyCode::Up if plain_key(event) => {
                    selected = selected.saturating_sub(1);
                }
                KeyCode::Enter if plain_key(event) && selected == fields.len() => {
                    match validate_workflow_form(workflow, worktree, &mut fields) {
                        Ok(values) => {
                            self.dialog = None;
                            self.draw(runtime)?;
                            return Ok(Some(values));
                        }
                        Err((field, problem)) => {
                            selected = field;
                            error = Some(problem);
                        }
                    }
                }
                KeyCode::Enter if plain_key(event) => match field_kind {
                    Some(view::FormFieldKind::String | view::FormFieldKind::Number) => {
                        selected = selected.saturating_add(1).min(fields.len());
                    }
                    Some(view::FormFieldKind::Bool) => {
                        toggle_bool_field(&mut fields[selected].value);
                    }
                    Some(view::FormFieldKind::Enum { ref options }) => {
                        dropdown = Some(view::FormDropdown {
                            selected: selected_option(options, &fields[selected].value),
                        });
                    }
                    Some(view::FormFieldKind::File { .. }) => {
                        let picked = runtime.suspend_for(|| {
                            Ok(pick_workflow_file(
                                fzf,
                                &fields[selected].name,
                                worktree,
                                workflow,
                            ))
                        })?;
                        match picked {
                            Ok(Some(value)) => fields[selected].value = value,
                            Ok(None) => {}
                            Err(problem) => error = Some(problem),
                        }
                    }
                    None => {}
                },
                KeyCode::Char(' ') if plain_key(event) => match field_kind {
                    Some(view::FormFieldKind::Bool) => {
                        toggle_bool_field(&mut fields[selected].value);
                    }
                    Some(view::FormFieldKind::Enum { ref options }) => {
                        dropdown = Some(view::FormDropdown {
                            selected: selected_option(options, &fields[selected].value),
                        });
                    }
                    Some(view::FormFieldKind::File { .. }) => {
                        let picked = runtime.suspend_for(|| {
                            Ok(pick_workflow_file(
                                fzf,
                                &fields[selected].name,
                                worktree,
                                workflow,
                            ))
                        })?;
                        match picked {
                            Ok(Some(value)) => fields[selected].value = value,
                            Ok(None) => {}
                            Err(problem) => error = Some(problem),
                        }
                    }
                    Some(view::FormFieldKind::String) => fields[selected].value.push(' '),
                    _ => {}
                },
                KeyCode::Left | KeyCode::Right if plain_key(event) => match field_kind {
                    Some(view::FormFieldKind::Bool) => {
                        fields[selected].value = if event.code == KeyCode::Left {
                            "false".into()
                        } else {
                            "true".into()
                        };
                    }
                    Some(view::FormFieldKind::Enum { ref options }) => {
                        cycle_enum_field(
                            &mut fields[selected].value,
                            options,
                            event.code == KeyCode::Right,
                        );
                    }
                    _ => {}
                },
                KeyCode::Backspace if plain_key(event) => {
                    if let Some(field) = fields.get_mut(selected) {
                        match field.kind {
                            view::FormFieldKind::String | view::FormFieldKind::Number => {
                                field.value.pop();
                            }
                            view::FormFieldKind::File { .. } => field.value.clear(),
                            _ => {}
                        }
                    }
                }
                KeyCode::Delete if plain_key(event) => {
                    if let Some(field) = fields.get_mut(selected) {
                        field.value.clear();
                    }
                }
                KeyCode::Char(ch) if plain_key(event) && !ch.is_control() => {
                    if let Some(field) = fields.get_mut(selected)
                        && matches!(
                            field.kind,
                            view::FormFieldKind::String | view::FormFieldKind::Number
                        )
                    {
                        field.value.push(ch);
                    }
                }
                _ => {}
            }
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
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(i64::MAX as u128) as i64;
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
                let step = workflow
                    .current_step
                    .as_ref()
                    .map(|step| step.label.as_str())
                    .unwrap_or("Workflow");
                view::OrderedToggleItem {
                    id: index.to_string(),
                    label: format!(
                        "{} / {}  {}  {}  {}",
                        repository.label, workflow.worktree.display, "Workflow", step, age
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
