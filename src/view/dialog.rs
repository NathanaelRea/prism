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
    let mut paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    if let crate::view::DialogModel::Help { scroll, .. } = dialog {
        paragraph = paragraph.scroll(((*scroll).min(u16::MAX as usize) as u16, 0));
    }
    frame.render_widget(paragraph, geometry.popup);
    if let crate::view::DialogModel::Prompt { prompt, input, .. } = dialog {
        set_prompt_cursor(frame, geometry.inner, prompt, input);
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
    dialog_lines(dialog)
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
        | crate::view::DialogModel::Prompt { title, .. }
        | crate::view::DialogModel::WorktreeColumns { title, .. }
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
            confirm_label,
            cancel_label,
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
            rendered.push(Line::from(""));
            rendered.push(Line::from(vec![
                Span::styled("Enter ", selected_style(true)),
                Span::styled(confirm_label.clone(), selected_style(true)),
                Span::styled("   Esc/q ", muted_style()),
                Span::raw(cancel_label.clone()),
            ]));
            rendered
        }
        crate::view::DialogModel::Prompt { prompt, input, .. } => {
            let mut lines = prompt_dialog_lines(prompt, input);
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Enter to continue, Esc to cancel",
                muted_style(),
            )));
            lines
        }
        crate::view::DialogModel::WorktreeColumns {
            columns, selected, ..
        } => worktree_column_lines(columns, *selected),
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
            Line::from(vec![
                Span::styled(format!("[{}]", choice.key), selected_style(true)),
                Span::styled(format!(" {}", choice.label), muted_style()),
            ])
        })
        .collect::<Vec<_>>()
}

pub(super) fn worktree_column_lines(
    columns: &[crate::view::WorktreeColumnChoice],
    selected: usize,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        "j/k select  Space enable/disable  J/K move enabled column  Enter save  Esc cancel",
        muted_style(),
    ))];
    lines.push(Line::from(""));
    if columns.is_empty() {
        lines.push(Line::from(Span::styled(
            "No wt columns found",
            muted_style(),
        )));
        return lines;
    }
    for (index, column) in columns.iter().enumerate() {
        let focused = index == selected;
        lines.push(Line::from(vec![
            Span::styled(if focused { "▶ " } else { "  " }, title_style(focused)),
            Span::styled(
                if column.enabled { "[x]" } else { "[ ]" },
                selected_style(column.enabled),
            ),
            Span::raw(" "),
            Span::styled(column.key.clone(), title_style(focused)),
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
