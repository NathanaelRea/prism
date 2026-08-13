use super::*;

pub(super) const PROMPT_INPUT_DISPLAY_WIDTH: u16 = 40;

pub(super) fn render_leader_hint(
    frame: &mut Frame<'_>,
    area: Rect,
    hint: &crate::view::LeaderHintModel,
) {
    let lines = choice_lines(hint);
    let content_width = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    let width = content_width
        .max(hint.title.chars().count() as u16)
        .saturating_add(4)
        .min(area.width.max(1));
    let height = (lines.len() as u16)
        .saturating_add(2)
        .min(area.height.max(1));
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    let block = panel_block(
        Line::from(Span::styled(hint.title.clone(), title_style(true))),
        false,
    );
    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Left)
            .block(block),
        popup,
    );
}

pub(super) fn render_dialog(frame: &mut Frame<'_>, area: Rect, dialog: &crate::view::DialogModel) {
    let geometry = dialog_geometry(area, dialog);
    let lines = padded_dialog_lines(dialog, geometry.inner.width as usize);
    let block = panel_block(
        Line::from(Span::styled(dialog_title(dialog), title_style(true))),
        false,
    );
    frame.render_widget(Clear, geometry.popup);
    let mut form_scroll = 0usize;
    let mut paragraph = Paragraph::new(lines.clone()).block(block);
    if let crate::view::DialogModel::Form {
        instructions,
        fields,
        selected,
        dropdown,
        ..
    } = dialog
    {
        form_scroll = form_scroll_offset(
            instructions,
            fields,
            *selected,
            *dropdown,
            form_text_area_width(geometry.inner.width as usize),
            lines.len(),
            geometry.inner.height as usize,
        );
        paragraph = paragraph.scroll((form_scroll.min(u16::MAX as usize) as u16, 0));
    } else {
        paragraph = paragraph.wrap(Wrap { trim: false });
        if let crate::view::DialogModel::Help { scroll, .. }
        | crate::view::DialogModel::Notice { scroll, .. } = dialog
        {
            paragraph = paragraph.scroll(((*scroll).min(u16::MAX as usize) as u16, 0));
        }
    }
    frame.render_widget(paragraph, geometry.popup);
    if let crate::view::DialogModel::Prompt { prompt, input, .. } = dialog {
        set_prompt_cursor(frame, geometry.inner, prompt, input);
    } else if let crate::view::DialogModel::Form {
        instructions,
        fields,
        selected,
        dropdown,
        ..
    } = dialog
    {
        set_form_cursor(
            frame,
            geometry.inner,
            instructions,
            fields,
            *selected,
            *dropdown,
            form_scroll,
        );
    } else if let crate::view::DialogModel::Confirm {
        prompt,
        input,
        default,
        ..
    } = dialog
    {
        set_confirmation_cursor(frame, geometry.inner, dialog, prompt, input, *default);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DialogGeometry {
    popup: Rect,
    inner: Rect,
}

pub(super) fn dialog_geometry(area: Rect, dialog: &crate::view::DialogModel) -> DialogGeometry {
    let title_width = Line::from(dialog_title(dialog)).width() as u16;
    let raw_lines = dialog_lines(dialog);
    let content_width = match dialog {
        crate::view::DialogModel::Prompt { prompt, .. } => {
            prompt_dialog_content_width(prompt, title_width)
        }
        crate::view::DialogModel::Form { .. } => raw_lines
            .iter()
            .map(|line| line.width() as u16)
            .max()
            .unwrap_or(0)
            .max(64)
            .max(title_width),
        crate::view::DialogModel::Help {
            items, info_lines, ..
        } => help_dialog_content_width(items, info_lines, title_width).max(
            raw_lines
                .iter()
                .map(|line| line.width() as u16)
                .max()
                .unwrap_or(0),
        ),
        _ => raw_lines
            .iter()
            .map(|line| line.width() as u16)
            .max()
            .unwrap_or(0)
            .max(title_width),
    };
    let width = content_width
        .saturating_add(4)
        .min(area.width.saturating_sub(2))
        .max(24.min(area.width));
    let height = (raw_lines.len() as u16)
        .saturating_add(2)
        .min(area.height.saturating_sub(2))
        .max(5.min(area.height));
    let popup = centered_rect(width, height, area);
    let inner = Rect {
        x: popup.x.saturating_add(1),
        y: popup.y.saturating_add(1),
        width: popup.width.saturating_sub(2),
        height: popup.height.saturating_sub(2),
    };
    DialogGeometry { popup, inner }
}

pub(super) fn prompt_dialog_content_width(prompt: &str, title_width: u16) -> u16 {
    let prompt_lines = prompt.split('\n').collect::<Vec<_>>();
    let last_prefix_width = prompt_lines
        .last()
        .copied()
        .unwrap_or(prompt)
        .chars()
        .count() as u16;
    prompt_lines
        .iter()
        .map(|line| line.chars().count() as u16)
        .max()
        .unwrap_or(0)
        .max(last_prefix_width.saturating_add(PROMPT_INPUT_DISPLAY_WIDTH))
        .max("Enter to continue, Esc to cancel".chars().count() as u16)
        .max(title_width)
}

pub(super) fn padded_dialog_lines(
    dialog: &crate::view::DialogModel,
    width: usize,
) -> Vec<Line<'static>> {
    let lines = match dialog {
        crate::view::DialogModel::Form {
            instructions,
            submit_label,
            fields,
            selected,
            dropdown,
            error,
            ..
        } => form_lines(
            instructions,
            submit_label,
            fields,
            *selected,
            *dropdown,
            error.as_deref(),
            form_text_area_width(width),
        ),
        _ => dialog_lines(dialog),
    };
    lines
        .into_iter()
        .map(|line| pad_line(line, width))
        .collect()
}

pub(super) fn pad_line(mut line: Line<'static>, width: usize) -> Line<'static> {
    let line_width = line.width();
    if line_width < width {
        line.spans.push(Span::raw(" ".repeat(width - line_width)));
    }
    line
}

pub(super) fn set_prompt_cursor(frame: &mut Frame<'_>, area: Rect, prompt: &str, input: &str) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let prompt_prefix_lines = prompt.split('\n').collect::<Vec<_>>();
    let prompt_prefix = prompt_prefix_lines.last().copied().unwrap_or(prompt);
    let prompt_width = prompt_prefix.chars().count() as u16;
    let input_width = visible_prompt_input_width(area.width, prompt_width);
    let input_cursor = input.chars().count().min(input_width as usize) as u16;
    let x_offset = prompt_width
        .saturating_add(input_cursor)
        .min(area.width.saturating_sub(1));
    let prompt_y = prompt_prefix_lines.len().saturating_sub(1) as u16;
    frame.set_cursor_position((
        area.x + x_offset,
        area.y + prompt_y.min(area.height.saturating_sub(1)),
    ));
}

pub(super) fn visible_prompt_input_width(area_width: u16, prompt_width: u16) -> u16 {
    area_width
        .saturating_sub(prompt_width)
        .saturating_sub(1)
        .min(PROMPT_INPUT_DISPLAY_WIDTH)
}

pub(super) fn dialog_title(dialog: &crate::view::DialogModel) -> String {
    match dialog {
        crate::view::DialogModel::Help { .. } => "Keybindings".to_string(),
        crate::view::DialogModel::Confirm { title, .. }
        | crate::view::DialogModel::Notice { title, .. }
        | crate::view::DialogModel::Prompt { title, .. }
        | crate::view::DialogModel::Form { title, .. }
        | crate::view::DialogModel::OrderedToggle { title, .. }
        | crate::view::DialogModel::Choice {
            choices: crate::view::ChoiceList { title, .. },
            ..
        }
        | crate::view::DialogModel::Progress { title, .. } => title.clone(),
    }
}

pub(super) fn dialog_lines(dialog: &crate::view::DialogModel) -> Vec<Line<'static>> {
    match dialog {
        crate::view::DialogModel::Help {
            filter,
            editing_filter,
            info_lines,
            items,
            ..
        } => {
            let query = filter.trim().to_ascii_lowercase();
            let mut lines = vec![Line::from(vec![
                Span::styled("Filter: ", muted_style()),
                Span::raw(format!("/{filter}")),
                Span::styled(
                    if *editing_filter {
                        "  typing"
                    } else {
                        "  / to search"
                    },
                    muted_style(),
                ),
            ])];
            lines.push(Line::from(""));
            if query.is_empty() && !*editing_filter && !info_lines.is_empty() {
                for line in info_lines {
                    lines.push(line.clone());
                }
                lines.push(Line::from(""));
            }
            let mut matched = 0;
            for item in items {
                if query.is_empty() || item.to_ascii_lowercase().contains(&query) {
                    lines.push(Line::from(item.clone()));
                    matched += 1;
                }
            }
            if matched == 0 {
                lines.push(Line::from(Span::styled(
                    "No matching keybindings",
                    muted_style(),
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Esc/q closes. / searches.",
                muted_style(),
            )));
            lines
        }
        crate::view::DialogModel::Confirm {
            lines,
            prompt,
            input,
            default,
            invalid,
            ..
        } => {
            let mut rendered = Vec::new();
            for line in lines {
                rendered.extend(styled_text_lines(
                    &line.text,
                    if line.attention {
                        attention_style()
                    } else {
                        Style::default()
                    },
                ));
            }
            if !rendered.is_empty() {
                rendered.push(Line::from(""));
            }
            rendered.push(Line::from(vec![
                Span::raw(format!("{prompt} ")),
                Span::styled(
                    if *default { "[Y/n]: " } else { "[y/N]: " },
                    selected_style(true),
                ),
                Span::raw(input.clone()),
            ]));
            if *invalid {
                rendered.push(Line::from(Span::styled(
                    "Please enter y or n.",
                    attention_style(),
                )));
            }
            rendered
        }
        crate::view::DialogModel::Notice { lines, .. } => lines
            .iter()
            .flat_map(|line| {
                styled_text_lines(
                    &line.text,
                    if line.attention {
                        attention_style()
                    } else {
                        Style::default()
                    },
                )
            })
            .collect(),
        crate::view::DialogModel::Prompt { prompt, input, .. } => {
            let mut lines = prompt_dialog_lines(prompt, input);
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Enter to continue, Esc to cancel",
                muted_style(),
            )));
            lines
        }
        crate::view::DialogModel::Form {
            instructions,
            submit_label,
            fields,
            selected,
            dropdown,
            error,
            ..
        } => form_lines(
            instructions,
            submit_label,
            fields,
            *selected,
            *dropdown,
            error.as_deref(),
            56,
        ),
        crate::view::DialogModel::OrderedToggle {
            items,
            selected,
            reorderable,
            ..
        } => ordered_toggle_lines(items, *selected, *reorderable),
        crate::view::DialogModel::Choice { choices, .. } => choice_lines(choices),
        crate::view::DialogModel::Progress { message, .. } => {
            let mut lines = vec![Line::from(Span::styled(
                "[*] Please wait",
                title_style(true),
            ))];
            lines.extend(styled_text_lines(message, Style::default()));
            lines
        }
    }
}

fn form_field_window(field_count: usize, selected: usize) -> (usize, usize) {
    const VISIBLE_FIELDS: usize = 8;
    let focused_field = selected.min(field_count.saturating_sub(1));
    let start = focused_field
        .saturating_sub(VISIBLE_FIELDS / 2)
        .min(field_count.saturating_sub(VISIBLE_FIELDS));
    (start, start.saturating_add(VISIBLE_FIELDS).min(field_count))
}

fn set_form_cursor(
    frame: &mut Frame<'_>,
    area: Rect,
    instructions: &str,
    fields: &[crate::view::FormField],
    selected: usize,
    dropdown: Option<crate::view::FormDropdown>,
    scroll: usize,
) {
    let Some(field) = fields.get(selected) else {
        return;
    };
    if !matches!(
        field.kind,
        crate::view::FormFieldKind::String
            | crate::view::FormFieldKind::TextArea { .. }
            | crate::view::FormFieldKind::Number
    ) {
        return;
    }
    let text_width = form_text_area_width(area.width as usize);
    let line = form_focus_line(instructions, fields, selected, dropdown, text_width)
        .saturating_sub(scroll);
    if line >= area.height as usize {
        return;
    }
    let x = match field.kind {
        crate::view::FormFieldKind::TextArea { .. } => {
            let visible = visible_text_area_lines(field, text_width);
            4usize.saturating_add(visible.last().map(|line| rendered_width(line)).unwrap_or(0))
        }
        _ => {
            let displayed_value = visible_prompt_input(&field.value, PROMPT_INPUT_DISPLAY_WIDTH);
            let prefix =
                visible_form_field_prefix(field, true, &displayed_value, area.width as usize);
            let prefix_width = rendered_width(&prefix);
            let input_width = (area.width as usize).saturating_sub(prefix_width.saturating_add(1));
            prefix_width.saturating_add(rendered_width(&displayed_value).min(input_width))
        }
    };
    frame.set_cursor_position((
        area.x + (x as u16).min(area.width.saturating_sub(1)),
        area.y + line as u16,
    ));
}

fn form_field_prefix(field: &crate::view::FormField, focused: bool) -> String {
    format!(
        "{} {} [{}] ({}): ",
        if focused { "▶" } else { " " },
        field.name,
        if field.required {
            "required"
        } else {
            "default"
        },
        match field.constraint.as_deref() {
            Some(constraint) => format!("{}: {constraint}", field.kind.label()),
            None => field.kind.label(),
        }
    )
}

fn visible_form_field_prefix(
    field: &crate::view::FormField,
    focused: bool,
    value: &str,
    available_width: usize,
) -> String {
    let prefix = form_field_prefix(field, focused);
    if rendered_width(&prefix).saturating_add(rendered_width(value)) <= available_width {
        prefix
    } else {
        format!("{} {}: ", if focused { "▶" } else { " " }, field.name)
    }
}

fn rendered_width(text: &str) -> usize {
    Line::from(text).width()
}

fn form_text_area_width(inner_width: usize) -> usize {
    inner_width.saturating_sub(5).clamp(1, 56)
}

fn text_area_visual_lines(value: &str, width: usize) -> Vec<String> {
    if value.is_empty() {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    for logical_line in value.split('\n') {
        if logical_line.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut line = String::new();
        for character in logical_line.chars() {
            let mut candidate = line.clone();
            candidate.push(character);
            if !line.is_empty() && rendered_width(&candidate) > width {
                lines.push(line);
                line = character.to_string();
            } else {
                line = candidate;
            }
        }
        lines.push(line);
    }
    lines
}

fn visible_text_area_lines(field: &crate::view::FormField, width: usize) -> Vec<String> {
    let height = match field.kind {
        crate::view::FormFieldKind::TextArea { height } => height.max(1),
        _ => return Vec::new(),
    };
    let visual = text_area_visual_lines(&field.value, width);
    if visual.len() <= height {
        return visual;
    }
    if height == 1 {
        return visual[visual.len() - 1..].to_vec();
    }
    let mut visible = vec!["…".to_string()];
    visible.extend_from_slice(&visual[visual.len() - (height - 1)..]);
    visible
}

fn form_field_render_height(
    field: &crate::view::FormField,
    focused: bool,
    dropdown: Option<crate::view::FormDropdown>,
    text_width: usize,
) -> usize {
    let text_area_height = if matches!(field.kind, crate::view::FormFieldKind::TextArea { .. }) {
        visible_text_area_lines(field, text_width).len()
    } else {
        0
    };
    let dropdown_height = if focused && dropdown.is_some() {
        match &field.kind {
            crate::view::FormFieldKind::Enum { options } => {
                options.len().min(7) + usize::from(options.len() > 7)
            }
            _ => 0,
        }
    } else {
        0
    };
    1 + text_area_height + dropdown_height
}

fn dropdown_window(option_count: usize, selected: usize) -> (usize, usize) {
    const VISIBLE_OPTIONS: usize = 7;
    let start = selected
        .saturating_sub(VISIBLE_OPTIONS / 2)
        .min(option_count.saturating_sub(VISIBLE_OPTIONS));
    (
        start,
        start.saturating_add(VISIBLE_OPTIONS).min(option_count),
    )
}

fn form_focus_line(
    instructions: &str,
    fields: &[crate::view::FormField],
    selected: usize,
    dropdown: Option<crate::view::FormDropdown>,
    text_width: usize,
) -> usize {
    let (start, _) = form_field_window(fields.len(), selected);
    let mut line =
        styled_text_lines(instructions, Style::default()).len() + 1 + usize::from(start > 0);
    for (index, field) in fields.iter().enumerate().take(selected).skip(start) {
        line += form_field_render_height(field, index == selected, dropdown, text_width);
    }
    if let Some(field) = fields.get(selected) {
        if matches!(field.kind, crate::view::FormFieldKind::TextArea { .. }) {
            line += visible_text_area_lines(field, text_width).len();
        } else if let (Some(dropdown), crate::view::FormFieldKind::Enum { options }) =
            (dropdown, &field.kind)
        {
            let (option_start, _) = dropdown_window(options.len(), dropdown.selected);
            line +=
                1 + usize::from(option_start > 0) + dropdown.selected.saturating_sub(option_start);
        }
    } else if selected >= fields.len() {
        line += 1;
    }
    line
}

fn form_scroll_offset(
    instructions: &str,
    fields: &[crate::view::FormField],
    selected: usize,
    dropdown: Option<crate::view::FormDropdown>,
    text_width: usize,
    line_count: usize,
    viewport_height: usize,
) -> usize {
    if viewport_height == 0 {
        return 0;
    }
    let focus = form_focus_line(instructions, fields, selected, dropdown, text_width);
    let max_scroll = line_count.saturating_sub(viewport_height);
    focus
        .saturating_add(1)
        .saturating_sub(viewport_height)
        .min(max_scroll)
}

pub(super) fn form_lines(
    instructions: &str,
    submit_label: &str,
    fields: &[crate::view::FormField],
    selected: usize,
    dropdown: Option<crate::view::FormDropdown>,
    error: Option<&str>,
    text_width: usize,
) -> Vec<Line<'static>> {
    let mut lines = styled_text_lines(instructions, title_style(true));
    lines.push(Line::from(""));
    let (field_start, field_end) = form_field_window(fields.len(), selected);
    if field_start > 0 {
        lines.push(Line::from(Span::styled("  …", muted_style())));
    }
    for (index, field) in fields.iter().enumerate().take(field_end).skip(field_start) {
        let focused = index == selected;
        let value = if matches!(field.kind, crate::view::FormFieldKind::TextArea { .. }) {
            None
        } else {
            Some(if field.value.is_empty() {
                "—"
            } else {
                field.value.as_str()
            })
        };
        let displayed_value = value
            .map(|value| visible_prompt_input(value, PROMPT_INPUT_DISPLAY_WIDTH))
            .unwrap_or_default();
        let available_width = text_width.saturating_add(5);
        let mut spans = vec![Span::styled(
            visible_form_field_prefix(field, focused, &displayed_value, available_width),
            title_style(focused),
        )];
        if value.is_some() {
            spans.push(Span::raw(displayed_value));
        }
        lines.push(Line::from(spans));
        if matches!(field.kind, crate::view::FormFieldKind::TextArea { .. }) {
            for text_line in visible_text_area_lines(field, text_width) {
                lines.push(Line::from(vec![
                    Span::raw("    "),
                    Span::raw(if text_line.is_empty() {
                        "—".to_string()
                    } else {
                        text_line
                    }),
                ]));
            }
        }
        if focused
            && let (Some(dropdown), crate::view::FormFieldKind::Enum { options }) =
                (dropdown, &field.kind)
        {
            let (start, end) = dropdown_window(options.len(), dropdown.selected);
            if start > 0 {
                lines.push(Line::from(Span::styled("      …", muted_style())));
            }
            for (option_index, option) in options.iter().enumerate().take(end).skip(start) {
                let option_focused = option_index == dropdown.selected;
                lines.push(Line::from(vec![
                    Span::styled(
                        if option_focused { "    ▶ " } else { "      " },
                        title_style(option_focused),
                    ),
                    Span::styled(option.clone(), selected_style(option_focused)),
                ]));
            }
            if end < options.len() {
                lines.push(Line::from(Span::styled("      …", muted_style())));
            }
        }
    }
    if field_end < fields.len() {
        lines.push(Line::from(Span::styled("  …", muted_style())));
    }
    lines.push(Line::from(""));
    let submit_focused = selected == fields.len();
    lines.push(Line::from(Span::styled(
        format!("{} {submit_label}", if submit_focused { "▶" } else { " " }),
        title_style(submit_focused),
    )));
    if let Some(description) = fields
        .get(selected)
        .and_then(|field| field.description.as_deref())
    {
        lines.push(Line::from(""));
        lines.extend(styled_text_lines(description, muted_style()));
    }
    if let Some(error) = error {
        lines.push(Line::from(Span::styled(
            format!("Error: {error}"),
            attention_style(),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        if dropdown.is_some() {
            "j/k select  Enter choose  Esc close"
        } else if fields
            .get(selected)
            .is_some_and(|field| matches!(field.kind, crate::view::FormFieldKind::TextArea { .. }))
        {
            "Tab move  Enter newline  Type to edit  Esc cancel"
        } else {
            "Tab/↑/↓ move  Type to edit  Space/Enter choose  Esc cancel"
        },
        muted_style(),
    )));
    lines
}

fn set_confirmation_cursor(
    frame: &mut Frame<'_>,
    area: Rect,
    dialog: &crate::view::DialogModel,
    prompt: &str,
    input: &str,
    default: bool,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let prefix_width = format!("{prompt} {}", if default { "[Y/n]: " } else { "[y/N]: " })
        .chars()
        .count() as u16;
    let input_width = input.chars().count().min(area.width as usize) as u16;
    let invalid = matches!(
        dialog,
        crate::view::DialogModel::Confirm { invalid: true, .. }
    );
    let y = dialog_lines(dialog)
        .len()
        .saturating_sub(if invalid { 2 } else { 1 }) as u16;
    frame.set_cursor_position((
        area.x
            + prefix_width
                .saturating_add(input_width)
                .min(area.width.saturating_sub(1)),
        area.y + y.min(area.height.saturating_sub(1)),
    ));
}

pub(super) fn help_dialog_content_width(
    items: &[String],
    info_lines: &[Line<'static>],
    title_width: u16,
) -> u16 {
    let filter_width = Line::from("Filter: /  / to search").width() as u16;
    items
        .iter()
        .map(|line| Line::from(line.as_str()).width() as u16)
        .chain(info_lines.iter().map(|line| line.width() as u16))
        .max()
        .unwrap_or(0)
        .max(filter_width)
        .max(Line::from("Esc/q closes. / searches.").width() as u16)
        .max(title_width)
}

pub(super) fn choice_lines(choices: &crate::view::ChoiceList) -> Vec<Line<'static>> {
    choices
        .choices
        .iter()
        .map(|choice| {
            let key_style = if choice.disabled {
                disabled_style()
            } else {
                selected_style(true)
            };
            let label_style = if choice.disabled {
                disabled_style()
            } else {
                muted_style()
            };
            Line::from(vec![
                Span::styled(format!("[{}]", choice.key), key_style),
                Span::styled(format!(" {}", choice.label), label_style),
            ])
        })
        .collect::<Vec<_>>()
}

pub(super) fn ordered_toggle_lines(
    items: &[crate::view::OrderedToggleItem],
    selected: usize,
    reorderable: bool,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        if reorderable {
            "j/k select  Space toggle  J/K move  Enter save  Esc cancel"
        } else {
            "j/k select  Space toggle  Enter confirm  Esc later"
        },
        muted_style(),
    ))];
    lines.push(Line::from(""));
    if items.is_empty() {
        lines.push(Line::from(Span::styled(
            "No options available",
            muted_style(),
        )));
        return lines;
    }
    for (index, item) in items.iter().enumerate() {
        let focused = index == selected;
        lines.push(Line::from(vec![
            Span::styled(if focused { "▶ " } else { "  " }, title_style(focused)),
            Span::styled(
                if item.enabled { "[x]" } else { "[ ]" },
                selected_style(item.enabled),
            ),
            Span::raw(" "),
            Span::styled(item.label.clone(), title_style(focused)),
        ]));
    }
    lines
}

pub(super) fn prompt_dialog_lines(prompt: &str, input: &str) -> Vec<Line<'static>> {
    let prompt_lines = prompt.split('\n').collect::<Vec<_>>();
    let mut lines = Vec::new();
    for (index, line) in prompt_lines.iter().enumerate() {
        let mut spans = styled_prompt_spans(line);
        if index + 1 == prompt_lines.len() {
            spans.push(Span::raw(visible_prompt_input(
                input,
                PROMPT_INPUT_DISPLAY_WIDTH,
            )));
        }
        lines.push(Line::from(spans));
    }
    lines
}

pub(super) fn visible_prompt_input(input: &str, max_width: u16) -> String {
    let len = input.chars().count();
    let skip = len.saturating_sub(max_width as usize);
    input.chars().skip(skip).collect()
}

pub(super) fn styled_prompt_spans(text: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('[') {
        let (before, after_start) = rest.split_at(start);
        if !before.is_empty() {
            spans.push(Span::styled(before.to_string(), muted_style()));
        }
        if let Some(end) = after_start.find(']') {
            let (option, after_option) = after_start.split_at(end + 1);
            spans.push(Span::styled(option.to_string(), selected_style(true)));
            rest = after_option;
        } else {
            rest = after_start;
            break;
        }
    }
    if !rest.is_empty() {
        spans.push(Span::styled(rest.to_string(), muted_style()));
    }
    spans
}

pub(super) fn styled_text_lines(text: &str, style: Style) -> Vec<Line<'static>> {
    text.split('\n')
        .map(|line| Line::from(Span::styled(line.to_string(), style)))
        .collect()
}
