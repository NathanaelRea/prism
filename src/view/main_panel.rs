use super::*;

pub(super) fn render_main(frame: &mut Frame<'_>, area: Rect, model: &crate::view::FrameModel<'_>) {
    let areas = if model.tmux_portal.is_some() {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(100), Constraint::Length(0)])
            .split(area)
    };
    let main_area = areas[0];
    let block = panel_block(
        Line::from(Span::styled("0 Main", title_style(model.main_focused))),
        model.main_focused,
    );
    let inner_area = block.inner(main_area);
    let content_area = inner_area.height as usize;
    let width = inner_area.width as usize;
    let mut lines = match model.focus {
        PanelFocus::Status => status_dashboard_lines(model),
        PanelFocus::Repos => repo_overview_lines(model, width, content_area),
        PanelFocus::Worktrees => worktree_detail_lines(model),
    };
    if model.focus == PanelFocus::Worktrees
        && let Some(dashboard) = &model.plan_dashboard
    {
        lines.push(Line::from(""));
        lines.extend(plan_dashboard_lines(dashboard, width, content_area));
    }
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let rendered_lines = paragraph.line_count(inner_area.width);
    let scroll = model
        .main_scroll
        .min(rendered_lines.saturating_sub(content_area));
    frame.render_widget(
        paragraph
            .block(block)
            .scroll((scroll.min(u16::MAX as usize) as u16, 0)),
        main_area,
    );
    if rendered_lines > content_area {
        let mut scrollbar_state = ScrollbarState::new(rendered_lines)
            .position(scroll)
            .viewport_content_length(content_area);
        frame.render_stateful_widget(
            Scrollbar::default()
                .begin_symbol(None)
                .end_symbol(None)
                .thumb_style(title_style(model.main_focused)),
            main_area.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut scrollbar_state,
        );
    }

    if let Some(portal) = &model.tmux_portal {
        render_tmux_portal(frame, areas[1], portal);
    }
}

fn render_tmux_portal(frame: &mut Frame<'_>, area: Rect, portal: &TmuxPortalModel<'_>) {
    let block = panel_block(
        Line::from(Span::styled(
            format!(" tmux · {} ", portal.branch),
            title_style(false),
        )),
        false,
    );
    let height = block.inner(area).height as usize;
    let lines = match &portal.state {
        TmuxPortalState::Loading => vec![Line::from(Span::styled(
            "Loading tmux preview...",
            muted_style(),
        ))],
        TmuxPortalState::Unavailable => vec![Line::from(Span::styled(
            "Tmux session unavailable",
            muted_style(),
        ))],
        TmuxPortalState::Ready(lines) => lines
            .iter()
            .skip(lines.len().saturating_sub(height))
            .cloned()
            .collect(),
    };
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

pub(super) fn status_dashboard_lines(model: &crate::view::FrameModel<'_>) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled("░▒▓█▓▒░ P ◤◥◣◢◤◥◣", logo_style())),
        Line::from(Span::styled("▒▓█▓▒░▒ R ◥◣◢◤◥◣◢", logo_style())),
        Line::from(Span::styled("▓█▓▒░▒▓ I ◣◢◤◥◣◢◤", logo_style())),
        Line::from(Span::styled("█▓▒░▒▓█ S ◢◤◥◣◢◤◥", logo_style())),
        Line::from(Span::styled("▓▒░▒▓█▓ M ◤◥◣◢◤◥◣", logo_style())),
        Line::from(""),
        Line::from(format!("version {}", env!("CARGO_PKG_VERSION"))),
        labelled_line("selected repo", model.selected_repo_label.clone()),
        Line::from(Span::styled(
            model.selected_repo_root.clone(),
            muted_style(),
        )),
        Line::from(""),
        heading_line("Navigation"),
        Line::from("1 status  2 repos  3 worktrees"),
        Line::from("Enter opens selected repo/worktree; Tab cycles focus"),
        Line::from("repos h/l switches views"),
        Line::from("e edits repo config; E edits user config"),
        Line::from(""),
        heading_line("Documentation"),
        Line::from("GitHub repository  https://github.com/NathanaelRea/prism"),
        Line::from("Keybindings         docs/keybindings.md"),
        Line::from("Configuration       docs/config.md"),
        Line::from("README              README.md"),
        Line::from(""),
        Line::from(Span::styled("Status", title_style(true))),
        Line::from(Span::styled(
            "Local board for repository worktrees and agents",
            muted_style(),
        )),
        Line::from(""),
        Line::from(format!("Repositories: {}", model.repos.len())),
        Line::from(format!("Worktrees: {}", model.worktrees.len())),
    ];
    for row in &model.status {
        lines.push(Line::from(vec![
            Span::styled(format!("{}: ", row.label), muted_style()),
            Span::styled(
                row.value.clone(),
                if row.attention {
                    attention_style()
                } else {
                    Style::default()
                },
            ),
        ]));
    }
    lines
}
