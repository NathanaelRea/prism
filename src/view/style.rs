use super::*;

pub(super) fn highlight_style() -> Style {
    Style::default().fg(highlight_color())
}

pub(super) fn highlight_color() -> Color {
    Color::Rgb(0, 255, 255)
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
        Style::default().fg(Color::Black).bg(highlight_color())
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
        Style::default().add_modifier(Modifier::BOLD)
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
    Style::default().fg(Color::Gray)
}

pub(super) fn disabled_style() -> Style {
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
        AgentState::Idle | AgentState::Attached => muted_style(),
    }
}
