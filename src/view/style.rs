use super::*;

pub(super) fn highlight_style() -> Style {
    Style::default().fg(highlight_color())
}

pub(super) fn highlight_color() -> Color {
    Color::Rgb(0, 255, 255)
}

pub(super) fn selected_color() -> Color {
    Color::Rgb(0, 160, 160)
}

pub(super) fn title_style(focused: bool) -> Style {
    let style = highlight_style();
    if focused {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

pub(super) fn logo_style() -> Style {
    highlight_style().add_modifier(Modifier::BOLD)
}

pub(super) fn selected_text_style() -> Style {
    Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD)
}

pub(super) fn error_style() -> Style {
    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
}

pub(super) fn selected_style(focused: bool) -> Style {
    let style = if focused {
        Style::default().fg(Color::Black).bg(selected_color())
    } else {
        Style::default().bg(Color::Rgb(32, 32, 32))
    };
    if focused {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

pub(super) fn selected_sidebar_row_style(focused: bool) -> Style {
    if focused {
        Style::default()
            .fg(Color::White)
            .bg(Color::Rgb(0, 64, 64))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().bg(Color::Rgb(32, 32, 32))
    }
}

pub(super) fn selected_sidebar_outline_style(focused: bool) -> Style {
    let style = if focused {
        highlight_style()
    } else {
        Style::default().fg(Color::DarkGray)
    };
    if focused {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

pub(super) fn attention_style() -> Style {
    Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
}

pub(super) fn muted_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

pub(super) fn health_style(health: &str) -> Style {
    if health == "ok" {
        Style::default().fg(Color::Green)
    } else if health.contains('!')
        || health.contains('✕')
        || health.contains('')
        || health.contains("CIx")
    {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Yellow)
    }
}

pub(super) fn agent_style(state: AgentState) -> Style {
    match state {
        AgentState::Running => Style::default().fg(Color::Green),
        AgentState::NeedsInput | AgentState::NeedsRestart => attention_style(),
        AgentState::ExitedOk => muted_style(),
        AgentState::ExitedError => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        AgentState::Idle => muted_style(),
    }
}

pub(super) fn auto_style(status: AutoRunStatus) -> Style {
    match status {
        AutoRunStatus::Running | AutoRunStatus::Queued => Style::default().fg(Color::Green),
        AutoRunStatus::Paused => attention_style(),
        AutoRunStatus::Failed | AutoRunStatus::Aborted => {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        }
        AutoRunStatus::Done => muted_style(),
    }
}

pub(super) fn auto_step_status_style(status: AutoStepStatus) -> Style {
    match status {
        AutoStepStatus::Done => Style::default().fg(Color::Green),
        AutoStepStatus::Failed | AutoStepStatus::Aborted => error_style(),
        AutoStepStatus::Running | AutoStepStatus::Starting | AutoStepStatus::Waiting => {
            attention_style()
        }
        AutoStepStatus::Queued | AutoStepStatus::Skipped => Style::default(),
    }
}

pub(super) fn auto_output_kind_style(kind: AutoOutputKind) -> Style {
    match kind {
        AutoOutputKind::Assistant => Style::default(),
        AutoOutputKind::Tool | AutoOutputKind::ToolOutput => attention_style(),
        AutoOutputKind::Diff => title_style(false),
        AutoOutputKind::Error => error_style(),
        AutoOutputKind::Status | AutoOutputKind::System | AutoOutputKind::RawJson => muted_style(),
    }
}

pub(super) fn plan_run_status_style(status: PlanRunStatus) -> Style {
    match status {
        PlanRunStatus::Paused => Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD),
        PlanRunStatus::Queued | PlanRunStatus::Running => attention_style(),
        PlanRunStatus::Failed | PlanRunStatus::Aborted => error_style(),
        PlanRunStatus::Done => Style::default().fg(Color::Green),
        PlanRunStatus::Draft => muted_style(),
    }
}

pub(super) fn plan_step_status_style(status: PlanStepStatus) -> Style {
    match status {
        PlanStepStatus::Done => Style::default().fg(Color::Green),
        PlanStepStatus::Failed | PlanStepStatus::Aborted => error_style(),
        PlanStepStatus::Running | PlanStepStatus::Starting => attention_style(),
        PlanStepStatus::Queued | PlanStepStatus::Skipped => Style::default(),
    }
}

pub(super) fn plan_output_kind_style(kind: PlanOutputKind) -> Style {
    match kind {
        PlanOutputKind::Assistant => Style::default(),
        PlanOutputKind::Tool | PlanOutputKind::ToolOutput => attention_style(),
        PlanOutputKind::Diff => title_style(false),
        PlanOutputKind::Todo => Style::default().fg(Color::Magenta),
        PlanOutputKind::Error => error_style(),
        PlanOutputKind::Status | PlanOutputKind::RawJson | PlanOutputKind::System => muted_style(),
    }
}

pub(super) fn diff_text_style(kind: PlanOutputKind, text: &str) -> Style {
    if kind != PlanOutputKind::Diff {
        return Style::default();
    }
    if text.starts_with("+++") || text.starts_with("---") || text.starts_with("@@") {
        return title_style(true);
    }
    if text.starts_with('+') {
        return Style::default().fg(Color::Green);
    }
    if text.starts_with('-') {
        return Style::default().fg(Color::Red);
    }
    Style::default()
}
