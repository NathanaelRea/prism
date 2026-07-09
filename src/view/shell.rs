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
        PanelFocus::Status => "1/2/3 panels  0 main  Enter ~/  Tab/S-Tab  ? help  q quit",
        PanelFocus::Repos => {
            "j/k select  Enter tmux  c create  U unarchive  C columns  R manage  / search  q quit"
        }
        PanelFocus::Worktrees => {
            "j/k select  3 toggle repo/all  0 main  Enter tmux/phase  +/- visibility  / search  q quit"
        }
    };
    let mut spans = vec![Span::raw(actions.to_string())];
    if let Some(message) = model.status_message {
        spans.push(Span::styled(format!(" | {message}"), attention_style()));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}
