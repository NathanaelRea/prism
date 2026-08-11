use super::*;

pub(super) fn labelled_line(label: &'static str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label} "), muted_style()),
        Span::raw(value),
    ])
}

pub(super) fn dynamic_labelled_line(label: String, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label} "), muted_style()),
        Span::raw(value),
    ])
}

pub(super) fn heading_line(label: &'static str) -> Line<'static> {
    Line::from(Span::styled(label, title_style(true)))
}

pub(super) fn scroll_start(selected: usize, visible_rows: usize) -> usize {
    if visible_rows == 0 {
        return selected;
    }
    selected.saturating_sub(visible_rows / 2)
}

pub(super) fn agent_icon(state: AgentState) -> &'static str {
    match state {
        AgentState::Idle => "○",
        AgentState::Attached => "◉",
        AgentState::Running => "●",
        AgentState::ExitedOk => "✓",
        AgentState::ExitedError => "✕",
        AgentState::NeedsRestart => "↻",
        AgentState::NeedsInput => "!",
    }
}

pub(super) fn opencode_icon(state: OpencodeState) -> &'static str {
    match state {
        OpencodeState::Starting => "◌",
        OpencodeState::Busy => "●",
        OpencodeState::Retry => "↻",
        OpencodeState::Idle => "○",
        OpencodeState::Done => "✓",
        OpencodeState::NeedsInput => "!",
        OpencodeState::Error => "✕",
        OpencodeState::Unknown | OpencodeState::Offline => "↻",
    }
}

pub(super) fn git_status_indicator(status: &str, icon_style: IconStyle) -> String {
    let mut parts = Vec::new();
    if let Some(count) = status_count(status, "dirty") {
        parts.push(counted_icon(icon_style, "✗", "", count));
    }
    if let Some(count) = status_count(status, "ahead") {
        parts.push(format!("↑{count}"));
    }
    if let Some(count) = status_count(status, "behind") {
        parts.push(format!("↓{count}"));
    }
    parts.join(" ")
}

fn counted_icon(
    icon_style: IconStyle,
    unicode: &'static str,
    nerd_font: &'static str,
    count: usize,
) -> String {
    match icon_style {
        IconStyle::Unicode => format!("{unicode}{count}"),
        IconStyle::NerdFont => format!("{nerd_font} {count}"),
    }
}

pub(super) fn elapsed_label(start_unix_ms: u64, end_unix_ms: u64) -> String {
    let total_seconds = end_unix_ms.saturating_sub(start_unix_ms) / 1000;
    let minutes = total_seconds / 60;
    let seconds = total_seconds % 60;
    format!("{minutes:02}:{seconds:02}")
}

pub(super) fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

pub(super) fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

pub(super) fn short_server(server_url: &str) -> &str {
    server_url
        .strip_prefix("http://")
        .or_else(|| server_url.strip_prefix("https://"))
        .unwrap_or(server_url)
}

pub(super) fn age_label(updated_unix_ms: u64) -> String {
    let seconds = now_unix_ms().saturating_sub(updated_unix_ms) / 1000;
    if seconds < 60 {
        format!("{seconds}s ago")
    } else if seconds < 60 * 60 {
        format!("{}m ago", seconds / 60)
    } else {
        format!("{}h ago", seconds / 60 / 60)
    }
}

pub(super) fn todo_summary(todos: &[crate::opencode::OpencodeTodo]) -> String {
    let mut pending = 0;
    let mut active = 0;
    let mut completed = 0;
    for todo in todos {
        match todo.status.as_str() {
            "completed" | "complete" | "done" => completed += 1,
            "in_progress" | "in-progress" | "active" | "running" => active += 1,
            _ => pending += 1,
        }
    }
    let mut parts = Vec::new();
    if pending > 0 {
        parts.push(format!("pending {pending}"));
    }
    if active > 0 {
        parts.push(format!("active {active}"));
    }
    if completed > 0 {
        parts.push(format!("done {completed}"));
    }
    parts.join("  ")
}

pub(super) fn agent_label(state: AgentState) -> &'static str {
    match state {
        AgentState::Running => "running",
        AgentState::Attached => "attached",
        AgentState::NeedsInput => "input",
        AgentState::NeedsRestart => "restart",
        AgentState::ExitedOk => "done",
        AgentState::ExitedError => "error",
        AgentState::Idle => "idle",
    }
}

pub(super) fn icon(
    icon_style: IconStyle,
    unicode: &'static str,
    nerd_font: &'static str,
) -> &'static str {
    match icon_style {
        IconStyle::Unicode => unicode,
        IconStyle::NerdFont => nerd_font,
    }
}
