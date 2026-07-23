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

pub(super) fn plan_mode_label(mode: PlanRunMode) -> &'static str {
    match mode {
        PlanRunMode::Sequential => "sequential",
        PlanRunMode::Parallel => "parallel",
    }
}

pub(super) fn auto_mode_label(mode: AutoRunMode) -> &'static str {
    match mode {
        AutoRunMode::Standard => "standard",
        AutoRunMode::PlanFirst => "plan_first",
    }
}

pub(super) fn auto_source_label(source: AutoImplementationSource) -> &'static str {
    match source {
        AutoImplementationSource::Prompt => "prompt",
        AutoImplementationSource::ExistingPlan => "plan file",
        AutoImplementationSource::DraftPlan => "draft plan",
    }
}

pub(super) fn auto_step_status_label(status: AutoStepStatus) -> &'static str {
    match status {
        AutoStepStatus::Queued => "queued",
        AutoStepStatus::Starting => "starting",
        AutoStepStatus::Running => "running",
        AutoStepStatus::Waiting => "waiting",
        AutoStepStatus::Done => "done",
        AutoStepStatus::Failed => "failed",
        AutoStepStatus::Aborted => "aborted",
        AutoStepStatus::Skipped => "skipped",
    }
}

pub(super) fn auto_output_kind_label(kind: AutoOutputKind) -> &'static str {
    match kind {
        AutoOutputKind::Assistant => "assistant",
        AutoOutputKind::Tool => "tool",
        AutoOutputKind::ToolOutput => "tool out",
        AutoOutputKind::Diff => "diff",
        AutoOutputKind::Status => "status",
        AutoOutputKind::System => "system",
        AutoOutputKind::Error => "error",
        AutoOutputKind::RawJson => "json",
    }
}

pub(super) fn plan_run_status_label(status: PlanRunStatus) -> &'static str {
    match status {
        PlanRunStatus::Draft => "draft",
        PlanRunStatus::Queued => "queued",
        PlanRunStatus::Running => "running",
        PlanRunStatus::Paused => "paused",
        PlanRunStatus::Done => "done",
        PlanRunStatus::Failed => "failed",
        PlanRunStatus::Aborted => "aborted",
    }
}

pub(super) fn plan_step_status_label(status: PlanStepStatus) -> &'static str {
    match status {
        PlanStepStatus::Queued => "queued",
        PlanStepStatus::Starting => "starting",
        PlanStepStatus::Running => "running",
        PlanStepStatus::Done => "done",
        PlanStepStatus::Failed => "failed",
        PlanStepStatus::Aborted => "aborted",
        PlanStepStatus::Skipped => "skipped",
    }
}

pub(super) fn plan_output_kind_label(kind: PlanOutputKind) -> &'static str {
    match kind {
        PlanOutputKind::Assistant => "assistant",
        PlanOutputKind::Tool => "tool",
        PlanOutputKind::ToolOutput => "tool out",
        PlanOutputKind::Diff => "diff",
        PlanOutputKind::Todo => "todo",
        PlanOutputKind::Status => "status",
        PlanOutputKind::RawJson => "json",
        PlanOutputKind::System => "system",
        PlanOutputKind::Error => "error",
    }
}

pub(super) fn plan_todo_summary(step: &PlanStepRun) -> String {
    let mut pending = 0;
    let mut active = 0;
    let mut done = 0;
    for todo in &step.todos {
        match todo.status.as_str() {
            "completed" | "complete" | "done" => done += 1,
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
    if done > 0 {
        parts.push(format!("done {done}"));
    }
    parts.join("  ")
}

pub(super) fn elapsed_step_label(step: &PlanStepRun) -> String {
    match (step.started_unix_ms, step.finished_unix_ms) {
        (Some(start), Some(end)) => elapsed_label(start, end),
        (Some(start), None) => elapsed_label(start, now_unix_ms()),
        _ => String::new(),
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
