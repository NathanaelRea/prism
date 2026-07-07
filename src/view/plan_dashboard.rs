use super::*;

pub(super) fn plan_dashboard_lines(
    dashboard: &crate::view::PlanDashboard,
    _width: usize,
    visible_rows: usize,
) -> Vec<Line<'static>> {
    let run = &dashboard.run.run;
    let selected_step = dashboard
        .run
        .steps
        .iter()
        .find(|step| step.step == run.selected_step)
        .or_else(|| dashboard.run.steps.first());
    let counts = dashboard.run.status_counts();
    let output_tail = dashboard
        .output_lines
        .last()
        .map(|line| short_log_tail(&line.text));
    let mut lines = vec![
        heading_line("Plan Run"),
        labelled_line("plan", run.plan_display.clone()),
        labelled_line("scope", run.scope_path.display().to_string()),
    ];
    if let Some(step) = selected_step {
        lines.push(Line::from(vec![
            Span::styled("current ", muted_style()),
            Span::raw(format!("{}/{} ", step.step, run.total_steps)),
            Span::styled(
                plan_step_status_label(step.status),
                plan_step_status_style(step.status),
            ),
            Span::raw(
                output_tail
                    .as_ref()
                    .map(|tail| format!("  {tail}"))
                    .unwrap_or_default(),
            ),
        ]));
    }
    lines.push(Line::from(vec![
        Span::styled("mode ", muted_style()),
        Span::raw(plan_mode_label(run.mode)),
        Span::styled("  status ", muted_style()),
        Span::styled(
            plan_run_status_label(run.status),
            plan_run_status_style(run.status),
        ),
        Span::styled("  elapsed ", muted_style()),
        Span::raw(elapsed_label(run.created_unix_ms, run.updated_unix_ms)),
    ]));
    if let Some(step) = selected_step
        && let Some(error) = step.error.as_deref()
    {
        lines.push(Line::from(vec![
            Span::styled("error ", muted_style()),
            Span::styled(error.to_string(), error_style()),
        ]));
    }
    if dashboard.runs.len() > 1 {
        lines.push(Line::from(""));
        lines.push(heading_line("Runs"));
        let selected_run = dashboard
            .runs
            .iter()
            .position(|run| run.selected)
            .unwrap_or(0);
        let start = scroll_start(selected_run, 5);
        for run in dashboard.runs.iter().skip(start).take(5) {
            lines.push(plan_run_row(run));
        }
    }
    lines.push(Line::from(vec![
        Span::styled("counts queued ", muted_style()),
        Span::raw((counts.queued + counts.starting).to_string()),
        Span::styled("  running ", muted_style()),
        Span::raw(counts.running.to_string()),
        Span::styled("  done ", muted_style()),
        Span::raw(counts.done.to_string()),
        Span::styled("  failed ", muted_style()),
        Span::raw(counts.failed.to_string()),
    ]));
    lines.push(Line::from(""));
    lines.push(heading_line("Steps"));
    let phase_rows_available = visible_rows.saturating_sub(lines.len()).max(3);
    let selected_index = dashboard
        .run
        .steps
        .iter()
        .position(|step| step.step == run.selected_step)
        .unwrap_or(0);
    let start = scroll_start(selected_index, phase_rows_available);
    for step in dashboard
        .run
        .steps
        .iter()
        .skip(start)
        .take(phase_rows_available)
    {
        let tail = if step.step == run.selected_step {
            output_tail.as_deref()
        } else {
            None
        };
        lines.push(plan_step_row(step, run.selected_step, tail));
    }
    lines.truncate(visible_rows);
    lines
}

pub(super) fn plan_run_row(run: &crate::view::PlanRunSummary) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            if run.selected { "▶ " } else { "  " },
            title_style(run.selected),
        ),
        Span::styled(
            format!("{:<8}", plan_run_status_label(run.status)),
            plan_run_status_style(run.status),
        ),
        Span::styled(
            format!(" {} ", age_label(run.updated_unix_ms)),
            muted_style(),
        ),
        Span::raw(run.plan_display.clone()),
        Span::styled(format!("  {}", short_id(&run.id)), muted_style()),
        Span::styled(format!("  {}", run.scope_path), muted_style()),
    ])
}

pub(super) fn plan_opencode_status_lines(step: &PlanStepRun) -> Vec<Line<'static>> {
    let state = step
        .opencode_state
        .map(OpencodeState::label)
        .unwrap_or_else(|| plan_step_status_label(step.status));
    let server = step
        .opencode_server_url
        .as_deref()
        .map(short_server)
        .unwrap_or("none");
    let session = step
        .opencode_session_id
        .as_deref()
        .map(short_id)
        .unwrap_or("none");
    let mut lines = vec![Line::from(vec![
        Span::styled("opencode ", muted_style()),
        Span::raw(state.to_string()),
        Span::styled("  server ", muted_style()),
        Span::raw(server.to_string()),
        Span::styled("  session ", muted_style()),
        Span::raw(session.to_string()),
    ])];
    if let Some(tool) = step.active_tool.as_deref() {
        lines.push(labelled_line("tool", tool.to_string()));
    }
    if let Some(message) = step.latest_message.as_deref() {
        lines.push(labelled_line("latest", message.to_string()));
    }
    let todos = plan_todo_summary(step);
    if !todos.is_empty() {
        lines.push(labelled_line("todos", todos));
    }
    lines
}

pub(super) struct RenderedPlanOutputRow {
    line_number: u64,
    kind: PlanOutputKind,
    text: String,
    collapsed: bool,
    block_key: Option<String>,
}

pub(super) fn render_plan_output_rows(
    dashboard: &crate::view::PlanDashboard,
    width: usize,
) -> Vec<RenderedPlanOutputRow> {
    let mut rows = Vec::new();
    let mut index = 0;
    while index < dashboard.output_lines.len() {
        let line = &dashboard.output_lines[index];
        if let Some(block_key) = plan_output_block_key(line) {
            let block_len = block_len_at(&dashboard.output_lines, index, &block_key);
            let collapsed = !dashboard.output_state.expanded_blocks.contains(&block_key);
            if collapsed {
                rows.push(RenderedPlanOutputRow {
                    line_number: line.line_number,
                    kind: line.kind,
                    text: collapsed_block_summary(
                        &dashboard.output_lines[index..index + block_len],
                        width,
                    ),
                    collapsed: true,
                    block_key: Some(block_key),
                });
                index += block_len;
                continue;
            }
        }
        for text in output_display_lines(line) {
            rows.push(RenderedPlanOutputRow {
                line_number: line.line_number,
                kind: line.kind,
                text,
                collapsed: false,
                block_key: plan_output_block_key(line),
            });
        }
        index += 1;
    }
    rows
}

pub(super) fn block_len_at(lines: &[PlanOutputLine], index: usize, block_key: &str) -> usize {
    let mut len = 0;
    for line in &lines[index..] {
        if plan_output_block_key(line).as_deref() != Some(block_key) {
            break;
        }
        len += 1;
    }
    len.max(1)
}

pub(super) fn selected_rendered_output_index(
    dashboard: &crate::view::PlanDashboard,
    rendered_output: &[RenderedPlanOutputRow],
) -> usize {
    let Some(selected_line) = dashboard.output_lines.get(
        dashboard
            .output_state
            .cursor
            .min(dashboard.output_lines.len().saturating_sub(1)),
    ) else {
        return 0;
    };
    let selected_block_key = plan_output_block_key(selected_line);
    if let Some(block_key) = selected_block_key.as_deref()
        && let Some(index) = rendered_output
            .iter()
            .position(|row| row.collapsed && row.block_key.as_deref() == Some(block_key))
    {
        return index;
    }
    rendered_output
        .iter()
        .position(|row| row.line_number == selected_line.line_number)
        .or_else(|| {
            selected_block_key.as_deref().and_then(|block_key| {
                rendered_output
                    .iter()
                    .position(|row| row.block_key.as_deref() == Some(block_key))
            })
        })
        .unwrap_or_else(|| rendered_output.len().saturating_sub(1))
}

pub(super) fn collapsed_block_summary(lines: &[PlanOutputLine], _width: usize) -> String {
    let Some(first) = lines.first() else {
        return String::new();
    };
    let line_count = lines
        .iter()
        .map(|line| line.text.lines().count().max(1))
        .sum::<usize>();
    let text = first.text.lines().next().unwrap_or("").replace('\n', " ");
    format!("[+] L{} {} lines  {}", first.line_number, line_count, text)
}

pub(super) fn output_display_lines(line: &PlanOutputLine) -> Vec<String> {
    let rows = line.text.lines().collect::<Vec<_>>();
    if rows.is_empty() {
        return vec![String::new()];
    }
    rows.into_iter().map(ToString::to_string).collect()
}

pub(super) fn plan_step_row(
    step: &PlanStepRun,
    selected_step: usize,
    output_tail: Option<&str>,
) -> Line<'static> {
    let selected = step.step == selected_step;
    let detail = step
        .error
        .as_deref()
        .or(output_tail)
        .or(step.active_tool.as_deref())
        .or(step.latest_message.as_deref())
        .unwrap_or("");
    Line::from(vec![
        Span::styled(if selected { "▶ " } else { "  " }, title_style(selected)),
        Span::styled(
            plan_checklist_mark(step.status),
            plan_step_status_style(step.status),
        ),
        Span::raw(" "),
        Span::styled(
            format!("Step {} ", step.step),
            plan_step_status_style(step.status),
        ),
        Span::styled(
            format!("{:<10}", plan_step_status_label(step.status)),
            plan_step_status_style(step.status),
        ),
        Span::styled(format!(" {:<8}", elapsed_step_label(step)), muted_style()),
        Span::raw(format!(" {}", short_log_tail(detail))),
    ])
}

pub(super) fn plan_checklist_mark(status: PlanStepStatus) -> &'static str {
    match status {
        PlanStepStatus::Done | PlanStepStatus::Skipped => "[x]",
        PlanStepStatus::Failed | PlanStepStatus::Aborted => "[!]",
        PlanStepStatus::Starting | PlanStepStatus::Running => "[-]",
        PlanStepStatus::Queued => "[ ]",
    }
}

pub(super) fn short_log_tail(text: &str) -> String {
    truncate(&text.replace('\n', " "), 50)
}

pub(super) fn plan_output_row(row: &RenderedPlanOutputRow, selected: bool) -> Line<'static> {
    let fold = if row.block_key.is_some() {
        if row.collapsed { "[+]" } else { "[-]" }
    } else {
        "   "
    };
    Line::from(vec![
        Span::styled(if selected { "> " } else { "  " }, title_style(selected)),
        Span::raw(format!("{fold} ")),
        Span::styled(format!("L{:<4} ", row.line_number), muted_style()),
        Span::styled(
            format!("{:<10}", plan_output_kind_label(row.kind)),
            plan_output_kind_style(row.kind),
        ),
        Span::raw(" "),
        Span::styled(row.text.clone(), diff_text_style(row.kind, &row.text)),
    ])
}
