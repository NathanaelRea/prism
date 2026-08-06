use super::*;

pub fn append_output_line(
    store: &PlanRunStore,
    line: &PlanOutputLine,
    max_lines_per_step: usize,
) -> Result<(), String> {
    store.validate_claim()?;
    crate::persistence::plan_run::append_output(store.path(), line, max_lines_per_step)
        .map_err(|error| crate::execution::claim_write_error("write plan output line", error))
}

pub(super) fn append_system_output(
    store: &PlanRunStore,
    step: &PlanStepRun,
    kind: PlanOutputKind,
    text: &str,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    append_system_output_with_block(store, step, kind, text, None, max_output_lines_per_step)
}

pub(super) fn append_system_output_with_block(
    store: &PlanRunStore,
    step: &PlanStepRun,
    kind: PlanOutputKind,
    text: &str,
    block_id: Option<&str>,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    store.validate_claim()?;
    crate::persistence::plan_run::append_allocated_output(
        store.path(),
        &step.run_id,
        step.step,
        unix_ms(),
        kind,
        text,
        block_id,
        max_output_lines_per_step,
    )
    .map_err(|error| crate::execution::claim_write_error("write allocated plan output line", error))
}

pub fn load_output_lines(
    store: &PlanRunStore,
    run_id: &str,
    step: usize,
) -> Result<Vec<PlanOutputLine>, String> {
    let started = std::time::Instant::now();
    let result = crate::persistence::plan_run::load_output(store.path(), run_id, step)
        .map_err(|error| format!("load plan output lines: {error}"));
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
        "load_plan",
        Some(started.elapsed()),
        vec![
            crate::flight_recorder::unsigned("row_count", row_count),
            crate::flight_recorder::unsigned("text_bytes", text_bytes),
            crate::flight_recorder::boolean("success", result.is_ok()),
        ],
    );
    result
}
