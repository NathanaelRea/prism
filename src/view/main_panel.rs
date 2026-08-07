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
    let main_title = if model.focus == PanelFocus::Worktrees {
        let mut title = Line::from(Span::styled("0 Main ", title_style(model.main_focused)));
        for (index, (view, label)) in [
            (WorktreeMainView::Overview, "overview"),
            (WorktreeMainView::Workflow, "workflow"),
        ]
        .into_iter()
        .enumerate()
        {
            if index > 0 {
                title.push_span(Span::styled(" | ", muted_style()));
            }
            title.push_span(Span::styled(
                label,
                if model.worktree_main_view == view {
                    title_style(model.main_focused)
                } else {
                    muted_style()
                },
            ));
        }
        title
    } else {
        Line::from(Span::styled("0 Main", title_style(model.main_focused)))
    };
    let block = panel_block(main_title, model.main_focused);
    let inner_area = block.inner(main_area);
    let content_area = inner_area.height as usize;
    let width = inner_area.width as usize;
    let lines = match model.focus {
        PanelFocus::Status => status_dashboard_lines(model),
        PanelFocus::Repos => repo_overview_lines(model, width, content_area),
        PanelFocus::Worktrees => match model.worktree_main_view {
            WorktreeMainView::Overview => worktree_detail_lines(model),
            WorktreeMainView::Workflow => workflow_dashboard_lines(model),
        },
    };
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

fn workflow_dashboard_lines(model: &crate::view::FrameModel<'_>) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let Some(dashboard) = &model.workflow_dashboard else {
        lines.push(Line::from(Span::styled(
            "No Workflow Runs",
            title_style(false),
        )));
        lines.push(Line::from("This worktree has not run a workflow yet."));
        lines.push(Line::from(vec![
            Span::styled("W", title_style(model.main_focused)),
            Span::raw(" launches a compatible Workflow Definition."),
        ]));
        return lines;
    };
    let position = if dashboard.run_position == 0 {
        "child run".to_string()
    } else {
        format!("Run {} of {}", dashboard.run_position, dashboard.run_count)
    };
    lines.push(Line::from(vec![
        Span::styled(
            dashboard
                .detail
                .as_ref()
                .map_or("Workflow", |run| run.definition_name.as_str())
                .to_string(),
            title_style(false),
        ),
        Span::styled(format!("  {position}"), muted_style()),
    ]));
    lines.push(Line::from(vec![
        Span::styled("status ", muted_style()),
        Span::raw(dashboard.status.clone()),
        Span::styled("  progress ", muted_style()),
        Span::raw(format!(
            "{}/{}",
            dashboard.completed_steps, dashboard.total_steps
        )),
        Span::styled("  run ", muted_style()),
        Span::raw(dashboard.run_id.clone()),
    ]));
    if let Some(parent) = &dashboard.parent_run_id {
        lines.push(Line::from(vec![
            Span::styled("parent ", muted_style()),
            Span::raw(parent.clone()),
            Span::styled("  Backspace returns", muted_style()),
        ]));
    }
    lines.push(Line::from(""));
    let Some(run) = &dashboard.detail else {
        lines.push(Line::from(Span::styled(
            "Loading resolved workflow…",
            muted_style(),
        )));
        return lines;
    };
    lines.push(Line::from(vec![
        Span::styled(
            if model.workflow_graph_expanded {
                "Dependency graph"
            } else {
                "Steps"
            },
            title_style(false),
        ),
        Span::styled(
            if model.workflow_graph_expanded {
                "  v: list"
            } else {
                "  v: graph"
            },
            muted_style(),
        ),
    ]));
    let mut depths = std::collections::BTreeMap::<String, usize>::new();
    for step in &run.steps {
        let depth = step
            .dependencies
            .iter()
            .filter_map(|dependency| depths.get(dependency))
            .max()
            .copied()
            .map_or(0, |depth| depth + 1);
        depths.insert(step.id.clone(), depth);
        depths.insert(step.key.clone(), depth);
        let prefix = if model.workflow_graph_expanded {
            if depth == 0 {
                "● ".to_string()
            } else {
                format!("{}├─", "│  ".repeat(depth.saturating_sub(1)))
            }
        } else {
            "  ".to_string()
        };
        lines.push(workflow_step_line(model, dashboard, run, step, prefix));
    }
    if let Some(selected) = dashboard.selected_step.as_deref()
        && let Some(step) = run.steps.iter().find(|step| step.key == selected)
    {
        lines.push(Line::from(""));
        lines.extend(workflow_step_detail_lines(run, step));
    }
    lines
}

fn workflow_step_line(
    model: &crate::view::FrameModel<'_>,
    dashboard: &crate::view::WorkflowDashboard,
    run: &crate::WorkflowProjection,
    step: &crate::WorkflowStepProjection,
    prefix: String,
) -> Line<'static> {
    let active = dashboard.current_step.as_deref() == Some(step.key.as_str())
        && matches!(
            step.status.as_str(),
            "claimed" | "running" | "runnable" | "waiting"
        );
    let selected = dashboard.selected_step.as_deref() == Some(step.key.as_str());
    let (icon, style) = if active {
        ("→", highlight_style().add_modifier(Modifier::BOLD))
    } else {
        match step.status.as_str() {
            "succeeded" => ("✓", Style::default().fg(Color::Green)),
            "failed" => ("✕", error_style()),
            "skipped" => ("⊘", muted_style()),
            "cancelled" => ("×", disabled_style()),
            "runnable" => ("○", highlight_style()),
            "waiting" => ("◌", attention_style()),
            "claimed" | "running" => ("→", highlight_style().add_modifier(Modifier::BOLD)),
            _ => ("○", muted_style()),
        }
    };
    let reason = workflow_step_reason(run, step);
    let child = run
        .children
        .iter()
        .rev()
        .find(|child| child.step_id == step.id)
        .map(|child| format!("  child={}", child.status))
        .unwrap_or_default();
    let dependencies = if model.workflow_graph_expanded && !step.dependencies.is_empty() {
        format!("  ← {}", dependency_names(run, step).join(", "))
    } else {
        String::new()
    };
    let text = format!(
        "{prefix}{icon} {}  [{}] {} — {reason}{child}{dependencies}",
        step.key, step.status, step.class
    );
    Line::from(Span::styled(
        text,
        if selected {
            selected_style(model.main_focused)
        } else {
            style
        },
    ))
}

fn workflow_step_reason(
    run: &crate::WorkflowProjection,
    step: &crate::WorkflowStepProjection,
) -> String {
    if step.status == "waiting" {
        let blockers = step
            .dependencies
            .iter()
            .filter_map(|dependency| {
                run.steps
                    .iter()
                    .find(|candidate| candidate.id == *dependency || candidate.key == *dependency)
            })
            .filter(|dependency| !matches!(dependency.status.as_str(), "succeeded" | "skipped"))
            .map(|dependency| dependency.key.clone())
            .collect::<Vec<_>>();
        if !blockers.is_empty() {
            return format!("waiting on {}", blockers.join(", "));
        }
    }
    match step.status.as_str() {
        "succeeded" => "complete",
        "claimed" | "running" => "active",
        "runnable" => "ready",
        "waiting" => "waiting",
        "skipped" => "skipped",
        "failed" => "attempt failed",
        "cancelled" => "cancelled",
        _ => "pending",
    }
    .to_string()
}

fn dependency_names(
    run: &crate::WorkflowProjection,
    step: &crate::WorkflowStepProjection,
) -> Vec<String> {
    step.dependencies
        .iter()
        .map(|dependency| {
            run.steps
                .iter()
                .find(|candidate| candidate.id == *dependency || candidate.key == *dependency)
                .map_or_else(|| dependency.clone(), |candidate| candidate.key.clone())
        })
        .collect()
}

fn workflow_step_detail_lines(
    run: &crate::WorkflowProjection,
    step: &crate::WorkflowStepProjection,
) -> Vec<Line<'static>> {
    let attempts = run
        .attempts
        .iter()
        .filter(|attempt| attempt.step_id == step.id)
        .collect::<Vec<_>>();
    let latest = attempts.last().copied();
    let input_names = serde_json::from_str::<serde_json::Value>(&step.input_json)
        .ok()
        .and_then(|value| {
            value
                .as_object()
                .map(|object| object.keys().cloned().collect::<Vec<_>>())
        })
        .unwrap_or_default();
    let attempt_ids = attempts
        .iter()
        .map(|attempt| attempt.id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let output_names = run
        .artifacts
        .iter()
        .filter(|artifact| {
            artifact
                .producing_attempt_id
                .as_deref()
                .is_some_and(|id| attempt_ids.contains(id))
        })
        .map(|artifact| artifact.id.clone())
        .collect::<Vec<_>>();
    let duration = latest.map(|attempt| {
        format_duration_ms(
            attempt
                .finished_unix_ms
                .unwrap_or(run.updated_unix_ms)
                .saturating_sub(attempt.started_unix_ms),
        )
    });
    let mut lines = vec![
        Line::from(Span::styled(
            format!("Step · {}", step.key),
            title_style(false),
        )),
        labelled_line(
            "state",
            format!("{} — {}", step.status, workflow_step_reason(run, step)),
        ),
        labelled_line("depends on", display_names(&dependency_names(run, step))),
        labelled_line("implementation", step.implementation.clone()),
        labelled_line("target", step.target_id.clone()),
        labelled_line("attempts", attempts.len().to_string()),
        labelled_line("inputs", display_names(&input_names)),
        labelled_line("outputs", display_names(&output_names)),
    ];
    if let Some(duration) = duration {
        lines.push(labelled_line("duration", duration));
    }
    if let Some(child) = run
        .children
        .iter()
        .rev()
        .find(|child| child.step_id == step.id)
    {
        lines.push(labelled_line(
            "child",
            format!("{} [{}] · Enter opens", child.run_id, child.status),
        ));
    }
    lines
}

fn display_names(names: &[String]) -> String {
    if names.is_empty() {
        "—".to_string()
    } else {
        names.join(", ")
    }
}

fn format_duration_ms(milliseconds: i64) -> String {
    let milliseconds = milliseconds.max(0);
    if milliseconds < 1_000 {
        format!("{milliseconds}ms")
    } else if milliseconds < 60_000 {
        format!("{:.1}s", milliseconds as f64 / 1_000.0)
    } else {
        format!(
            "{}m {}s",
            milliseconds / 60_000,
            milliseconds % 60_000 / 1_000
        )
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
        TmuxPortalState::Loading => Vec::new(),
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
    vec![
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
    ]
}
