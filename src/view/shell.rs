use super::*;

pub(crate) fn render(frame: &mut Frame<'_>, model: &crate::view::FrameModel<'_>) {
    let area = frame.area();
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(sidebar_width(area.width, model.config.layout.sidebar_width)),
            Constraint::Min(MIN_MAIN_WIDTH),
        ])
        .split(vertical[0]);

    render_sidebar(frame, body[0], model);
    render_main(frame, body[1], model);
    render_footer(frame, vertical[1], model);
    if let Some(hint) = &model.leader_hint {
        render_leader_hint(frame, area, hint);
    }
    if let Some(dialog) = &model.dialog {
        render_dialog(frame, area, dialog);
    }
}

pub(super) fn render_footer(
    frame: &mut Frame<'_>,
    area: Rect,
    model: &crate::view::FrameModel<'_>,
) {
    let actions = match model.focus {
        PanelFocus::Status => vec![
            ("Panels", "1/2/3"),
            ("Main", "0"),
            ("Home", "Enter"),
            ("Focus", "Tab/S-Tab"),
            ("Info", "?"),
            ("Quit", "q"),
        ],
        PanelFocus::Repos => vec![
            ("Select", "j/k"),
            ("Tmux", "Enter"),
            ("Create", "c"),
            ("Unarchive", "U"),
            ("Columns", "C"),
            ("Manage", "R"),
            ("Search", "/"),
            ("Info", "?"),
            ("Quit", "q"),
        ],
        PanelFocus::Worktrees => vec![
            ("Select", "j/k"),
            ("Open", "Enter"),
            ("Visibility", "+/-"),
            ("Search", "/"),
            ("Info", "?"),
            ("Quit", "q"),
        ],
    };
    let mut spans = footer_action_spans(&actions);
    if let Some(message) = model.status_message {
        spans.push(Span::styled(format!(" | {message}"), attention_style()));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn footer_action_spans(actions: &[(&str, &str)]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (index, (label, binding)) in actions.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" | ", muted_style()));
        }
        spans.push(Span::styled(format!("{label} "), muted_style()));
        spans.push(Span::raw((*binding).to_string()));
    }
    spans
}
