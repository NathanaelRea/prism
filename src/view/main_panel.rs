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
        && let Some(dashboard) = &model.workflow_dashboard
    {
        lines.push(Line::from(""));
        lines.extend(workflow_dashboard_lines(dashboard));
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

fn workflow_dashboard_lines(dashboard: &crate::view::WorkflowDashboard) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled("Workflow Run", title_style(false))),
        Line::from(format!("run: {}", dashboard.run_id)),
        Line::from(format!("status: {}", dashboard.status)),
        Line::from(format!(
            "progress: {}/{}",
            dashboard.completed_steps, dashboard.total_steps
        )),
    ];
    if let Some(step) = &dashboard.current_step {
        lines.push(Line::from(format!("selected Step: {step}")));
    }
    if let Some(parent) = &dashboard.parent_run_id {
        lines.push(Line::from(format!("parent: {parent}")));
    }
    for child in &dashboard.children {
        lines.push(Line::from(format!("  child: {child}")));
    }
    if let Some(run) = &dashboard.detail {
        lines.push(Line::from(format!("definition: {}", run.definition_name)));
        lines.push(Line::from("Steps"));
        for step in &run.steps {
            lines.push(Line::from(format!(
                "  {} [{}] {} inputs={}",
                step.key, step.status, step.class, step.input_json
            )));
        }
        lines.push(Line::from(format!(
            "Attempts: {}  Artifacts: {}  Effects: {}  Approvals: {}  Gates: {}",
            run.attempts.len(),
            run.artifacts.len(),
            run.effects.len(),
            run.approvals.len(),
            run.gates.len()
        )));
        for attempt in &run.attempts {
            lines.push(Line::from(format!(
                "  attempt {} [{}] worker={} target={}",
                attempt.id, attempt.status, attempt.worker_id, attempt.target_id
            )));
            for binding in &attempt.bindings {
                lines.push(Line::from(format!(
                    "    binding {}:{} = {}",
                    binding.name, binding.schema_id, binding.value_json
                )));
            }
            for output in attempt.output.iter().rev().take(20).rev() {
                lines.push(Line::from(format!(
                    "    {}: {}",
                    output.stream,
                    String::from_utf8_lossy(&output.body)
                )));
            }
        }
        for artifact in &run.artifacts {
            lines.push(Line::from(format!(
                "  Artifact {} rev={} digest={} sensitivity={} provenance={}",
                artifact.id,
                artifact.revision,
                artifact.digest,
                artifact.sensitivity,
                artifact.trigger_occurrence_id.as_deref().unwrap_or("-")
            )));
        }
        for effect in &run.effects {
            lines.push(Line::from(format!(
                "  effect {} [{}] {}",
                effect.effect_kind, effect.status, effect.idempotency_key
            )));
        }
        for approval in &run.approvals {
            lines.push(Line::from(format!(
                "  approval {} [{}] by={}",
                approval.id,
                approval.status,
                approval.decided_by.as_deref().unwrap_or("-")
            )));
        }
        for gate in &run.gates {
            lines.push(Line::from(format!(
                "  blocker {} due={} evidence={}",
                gate.gate_kind,
                gate.due_unix_ms,
                gate.evidence_json.as_deref().unwrap_or("-")
            )));
        }
        if !run.events.is_empty() {
            lines.push(Line::from("History"));
            for event in run.events.iter().rev().take(20).rev() {
                lines.push(Line::from(format!(
                    "  #{} {} {}",
                    event.sequence, event.kind, event.data_json
                )));
            }
        }
    }
    lines
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
