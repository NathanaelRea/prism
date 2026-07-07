use super::*;

pub(super) fn worktree_detail_lines(model: &crate::view::FrameModel<'_>) -> Vec<Line<'static>> {
    let Some(index) = model.selected_session else {
        return vec![Line::from(Span::styled(
            "No worktree selected",
            muted_style(),
        ))];
    };
    let Some(session) = model.sessions.get(index) else {
        return vec![Line::from(Span::styled(
            "Selected worktree is filtered",
            muted_style(),
        ))];
    };
    let mut lines = vec![
        Line::from(Span::styled(session.branch.clone(), title_style(true))),
        Line::from(Span::styled(session.path_display.clone(), muted_style())),
        Line::from(""),
        Line::from(vec![
            Span::styled("status ", muted_style()),
            Span::raw(git_status_indicator(
                &session.status_label,
                model.config.icon_style,
            )),
            Span::styled("  agent ", muted_style()),
            Span::styled(
                agent_label(session.agent_state),
                agent_style(session.agent_state),
            ),
            Span::styled("  kind ", muted_style()),
            Span::raw(session.classification.label().to_string()),
            Span::styled("  adopted ", muted_style()),
            Span::raw(if session.adopted { "yes" } else { "no" }),
        ]),
    ];
    if !session.prompt_summary.trim().is_empty() {
        lines.push(labelled_line("prompt", session.prompt_summary.clone()));
    }
    if session.is_task_branch(model.config) && model.config.default_agent == "opencode" {
        match &session.opencode_status {
            Some(status) => lines.extend(opencode_status_lines(status)),
            None => lines.extend(opencode_not_started_lines()),
        }
    }
    lines.push(Line::from(""));
    lines.extend(pr_panel_lines(model.config, Some(session)));
    lines
}

pub(super) fn opencode_status_lines(status: &OpencodeStatus) -> Vec<Line<'static>> {
    let session = status.session_id.as_deref().map(short_id).unwrap_or("none");
    let server = status
        .server_url
        .as_deref()
        .map(short_server)
        .unwrap_or("none");
    let title = status.title.as_deref().filter(|title| !title.is_empty());
    let mut lines = vec![match title {
        Some(title) => Line::from(vec![
            Span::styled("opencode ", muted_style()),
            Span::raw(status.state.label().to_string()),
            Span::styled("  server ", muted_style()),
            Span::raw(server.to_string()),
            Span::styled("  session ", muted_style()),
            Span::raw(session.to_string()),
            Span::raw(format!("  {title}")),
        ]),
        None => Line::from(vec![
            Span::styled("opencode ", muted_style()),
            Span::raw(status.state.label().to_string()),
            Span::styled("  server ", muted_style()),
            Span::raw(server.to_string()),
            Span::styled("  session ", muted_style()),
            Span::raw(session.to_string()),
        ]),
    }];
    if let Some(tool) = &status.active_tool {
        lines.push(labelled_line("tool", tool.clone()));
    }
    if let Some(message) = &status.latest_message {
        lines.push(labelled_line("latest", message.clone()));
    }
    let todo = todo_summary(&status.todos);
    if !todo.is_empty() {
        lines.push(labelled_line("todos", todo));
    }
    if let Some(updated) = status.last_updated_unix_ms {
        lines.push(labelled_line("updated", age_label(updated)));
    }
    lines
}

pub(super) fn opencode_not_started_lines() -> Vec<Line<'static>> {
    vec![Line::from(vec![
        Span::styled("opencode ", muted_style()),
        Span::raw("not started"),
        Span::styled("  server ", muted_style()),
        Span::raw("none"),
        Span::styled("  session ", muted_style()),
        Span::raw("none"),
    ])]
}
