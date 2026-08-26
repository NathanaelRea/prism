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
    let main_title = Line::from(Span::styled("0 Main", title_style(model.main_focused)));
    let block = panel_block(main_title, model.main_focused);
    let inner_area = block.inner(main_area);
    let content_area = inner_area.height as usize;
    let width = inner_area.width as usize;
    let lines = match model.focus {
        PanelFocus::Status => status_dashboard_lines(model),
        PanelFocus::Repos => repo_overview_lines(model, width, content_area),
        PanelFocus::Worktrees | PanelFocus::Merges => {
            let mut lines = worktree_detail_lines(model);
            lines.push(Line::from(""));
            lines.extend(workflow_dashboard_lines(model));
            lines
        }
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
    let Some(dashboard) = &model.workflow_dashboard else {
        return vec![Line::from(vec![
            Span::styled("Workflow", title_style(false)),
            Span::styled(" · none · W to run", muted_style()),
        ])];
    };
    let workflow_name = dashboard
        .detail
        .as_ref()
        .map_or("Workflow", |run| run.workflow_name.as_str());
    let mut lines = vec![Line::from(vec![
        Span::styled("Workflow", title_style(false)),
        Span::styled(
            format!(" · {workflow_name} · {}", dashboard.status),
            muted_style(),
        ),
    ])];
    let Some(run) = &dashboard.detail else {
        lines.push(Line::from(Span::styled("  loading…", muted_style())));
        return lines;
    };
    for step in &run.steps {
        let (icon, style) = match step.phase {
            crate::PromptStepPhase::Satisfied | crate::PromptStepPhase::Completed => {
                ("✓", Style::default().fg(Color::Green))
            }
            crate::PromptStepPhase::Checking
            | crate::PromptStepPhase::Preparing
            | crate::PromptStepPhase::Prepared
            | crate::PromptStepPhase::RunningAgent
            | crate::PromptStepPhase::AgentSucceeded
            | crate::PromptStepPhase::Finalizing => ("…", highlight_style()),
            crate::PromptStepPhase::Waiting => ("…", attention_style()),
            crate::PromptStepPhase::Failed | crate::PromptStepPhase::RecoveryRequired => {
                ("✕", error_style())
            }
            crate::PromptStepPhase::Cancelled => ("×", disabled_style()),
            crate::PromptStepPhase::Pending => ("○", muted_style()),
        };
        let summary = step
            .summary
            .as_deref()
            .unwrap_or_else(|| prompt_phase_label(step.phase));
        let dependency = if step.explicit_dependencies {
            if step.dependencies.is_empty() {
                "  [root]".to_string()
            } else {
                format!("  [after {}]", step.dependencies.join(", "))
            }
        } else {
            String::new()
        };
        let text = format!(
            "{icon} {:<18}  {summary}{dependency}",
            step.key.replace(['_', '-'], " ")
        );
        let selected = dashboard.selected_step.as_deref() == Some(step.key.as_str());
        lines.push(Line::from(Span::styled(
            text,
            if selected {
                selected_style(model.main_focused)
            } else {
                style
            },
        )));
    }
    if let Some(selected) = dashboard.selected_step.as_deref()
        && let Some(step) = run.steps.iter().find(|step| step.key == selected)
    {
        lines.push(Line::from(""));
        lines.push(labelled_line(
            "phase",
            prompt_phase_label(step.phase).to_string(),
        ));
        if let Some(wake) = step.wake_at_unix_ms {
            lines.push(labelled_line("next check", wake.to_string()));
        }
        if let Some(final_text) = step.final_text() {
            lines.push(labelled_line("Agent", final_text.to_string()));
        }
    }
    lines
}

fn prompt_phase_label(phase: crate::PromptStepPhase) -> &'static str {
    match phase {
        crate::PromptStepPhase::Pending => "pending",
        crate::PromptStepPhase::Checking => "checking",
        crate::PromptStepPhase::Preparing | crate::PromptStepPhase::Prepared => "preparing",
        crate::PromptStepPhase::RunningAgent => "running Agent",
        crate::PromptStepPhase::AgentSucceeded | crate::PromptStepPhase::Finalizing => "finalizing",
        crate::PromptStepPhase::Waiting => "waiting",
        crate::PromptStepPhase::Satisfied => "satisfied",
        crate::PromptStepPhase::Completed => "completed",
        crate::PromptStepPhase::Failed => "failed",
        crate::PromptStepPhase::Cancelled => "cancelled",
        crate::PromptStepPhase::RecoveryRequired => "recovery required",
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
