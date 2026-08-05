use super::*;

pub fn append_output_line(conn: &AutoFlowStore, line: &AutoOutputLine) -> Result<(), String> {
    append_output_line_limited(conn, line, 0)
}

pub fn append_output_line_limited(
    conn: &AutoFlowStore,
    line: &AutoOutputLine,
    max_lines_per_step: usize,
) -> Result<(), String> {
    persistence::append_output(conn.path(), line, max_lines_per_step)
        .map_err(|error| crate::execution::claim_write_error("append Auto Flow output", error))
}

pub fn load_output_lines(
    conn: &AutoFlowStore,
    step_run_id: i64,
) -> Result<Vec<AutoOutputLine>, String> {
    let started = std::time::Instant::now();
    let result = persistence::load_output(conn.path(), step_run_id)
        .map_err(|error| format!("load Auto Flow output: {error}"));
    let (row_count, text_bytes) = result
        .as_ref()
        .map(|lines| {
            (
                lines.len(),
                lines.iter().map(|line| line.text.len()).sum::<usize>(),
            )
        })
        .unwrap_or_default();
    crate::flight_recorder::record(
        "output",
        "load_auto",
        Some(started.elapsed()),
        vec![
            crate::flight_recorder::unsigned("row_count", row_count),
            crate::flight_recorder::unsigned("text_bytes", text_bytes),
            crate::flight_recorder::boolean("success", result.is_ok()),
        ],
    );
    result
}

pub fn append_auto_event(conn: &AutoFlowStore, event: &AutoEvent) -> Result<i64, String> {
    let id = persistence::append_event(conn.path(), event)
        .map_err(|error| crate::execution::claim_write_error("append Auto Flow event", error))?;
    emit_auto_event_log(event);
    Ok(id)
}

pub(super) fn append_step_status_output(
    conn: &AutoFlowStore,
    step: &AutoStepRun,
    text: &str,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    let Some(step_id) = step.id else {
        return Ok(());
    };
    append_system_output(
        conn,
        step_id,
        AutoOutputKind::Status,
        text,
        None,
        max_output_lines_per_step,
    )
}

pub(super) fn append_system_output(
    conn: &AutoFlowStore,
    step_run_id: i64,
    kind: AutoOutputKind,
    text: &str,
    block_id: Option<&str>,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    let line_number = next_output_line_number(conn, step_run_id)?;
    append_output_line_limited(
        conn,
        &AutoOutputLine {
            step_run_id,
            line_number,
            time_unix_ms: unix_ms(),
            kind,
            text: text.to_string(),
            block_id: block_id.map(str::to_string),
        },
        max_output_lines_per_step,
    )
}

pub(super) fn next_output_line_number(
    conn: &AutoFlowStore,
    step_run_id: i64,
) -> Result<u64, String> {
    persistence::next_output(conn.path(), step_run_id)
        .map_err(|error| format!("allocate Auto Flow output line: {error}"))
}
