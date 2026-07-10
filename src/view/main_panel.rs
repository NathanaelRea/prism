use super::*;

pub(super) fn render_main(frame: &mut Frame<'_>, area: Rect, model: &crate::view::FrameModel<'_>) {
    let content_area = panel_block(
        Line::from(Span::styled("0 Main", title_style(model.main_focused))),
        model.main_focused,
    )
    .inner(area)
    .height
    .saturating_sub(0) as usize;
    let width = area.width.saturating_sub(2) as usize;
    let mut lines = match model.focus {
        PanelFocus::Status => status_dashboard_lines(model),
        PanelFocus::Repos => repo_overview_lines(model, width, content_area),
        PanelFocus::Worktrees => worktree_detail_lines(model),
    };
    if model.focus == PanelFocus::Worktrees {
        if let Some(dashboard) = &model.plan_dashboard {
            lines.push(Line::from(""));
            lines.extend(plan_dashboard_lines(dashboard, width, content_area));
        }
        if let Some(dashboard) = &model.auto_dashboard {
            lines.push(Line::from(""));
            lines.extend(auto_dashboard_lines(dashboard, width, content_area));
        }
        lines.truncate(content_area);
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel_block(
                Line::from(Span::styled("0 Main", title_style(model.main_focused))),
                model.main_focused,
            ))
            .wrap(Wrap { trim: false }),
        area,
    );
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
