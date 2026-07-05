use super::*;

pub fn append_output_line(
    conn: &rusqlite::Connection,
    line: &PlanOutputLine,
    max_lines_per_step: usize,
) -> Result<(), String> {
    conn.execute(
        "insert or replace into plan_output_line (
           run_id, step, line_number, time_unix_ms, kind, text, block_id
         ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            line.run_id.as_str(),
            usize_to_i64(line.step),
            u64_to_i64(line.line_number),
            u64_to_i64(line.time_unix_ms),
            line.kind.as_str(),
            line.text.as_str(),
            line.block_id.as_deref(),
        ],
    )
    .map_err(|error| format!("write plan output line: {error}"))?;
    trim_output_lines(conn, &line.run_id, line.step, max_lines_per_step)
}

pub(super) fn append_system_output(
    conn: &rusqlite::Connection,
    step: &PlanStepRun,
    kind: PlanOutputKind,
    text: &str,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    append_system_output_with_block(conn, step, kind, text, None, max_output_lines_per_step)
}

pub(super) fn append_system_output_with_block(
    conn: &rusqlite::Connection,
    step: &PlanStepRun,
    kind: PlanOutputKind,
    text: &str,
    block_id: Option<&str>,
    max_output_lines_per_step: usize,
) -> Result<(), String> {
    let line_number = next_output_line_number(conn, &step.run_id, step.step)?;
    append_output_line(
        conn,
        &PlanOutputLine {
            run_id: step.run_id.clone(),
            step: step.step,
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
    conn: &rusqlite::Connection,
    run_id: &str,
    step: usize,
) -> Result<u64, String> {
    let current: Option<i64> = conn
        .query_row(
            "select max(line_number) from plan_output_line where run_id = ?1 and step = ?2",
            params![run_id, usize_to_i64(step)],
            |row| row.get(0),
        )
        .map_err(|error| format!("read next plan output line number: {error}"))?;
    Ok(current.unwrap_or(0).max(0) as u64 + 1)
}

pub fn load_output_lines(
    conn: &rusqlite::Connection,
    run_id: &str,
    step: usize,
) -> Result<Vec<PlanOutputLine>, String> {
    let mut statement = conn
        .prepare(
            "select run_id, step, line_number, time_unix_ms, kind, text, block_id
             from plan_output_line
             where run_id = ?1 and step = ?2
             order by line_number",
        )
        .map_err(|error| format!("prepare plan output load: {error}"))?;
    let rows = statement
        .query_map(params![run_id, usize_to_i64(step)], |row| {
            let kind: String = row.get(4)?;
            Ok(PlanOutputLine {
                run_id: row.get(0)?,
                step: i64_to_usize(row.get(1)?, 1),
                line_number: i64_to_u64(row.get(2)?, 2),
                time_unix_ms: i64_to_u64(row.get(3)?, 3),
                kind: PlanOutputKind::parse(&kind).map_err(from_string_error)?,
                text: row.get(5)?,
                block_id: row.get(6)?,
            })
        })
        .map_err(|error| format!("load plan output lines: {error}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read plan output lines: {error}"))
}

pub(super) fn trim_output_lines(
    conn: &rusqlite::Connection,
    run_id: &str,
    step: usize,
    max_lines_per_step: usize,
) -> Result<(), String> {
    if max_lines_per_step == 0 {
        return Ok(());
    }
    let retained_line_count = max_lines_per_step.saturating_sub(1);
    if retained_line_count == 0 {
        conn.execute(
            "delete from plan_output_line where run_id = ?1 and step = ?2",
            params![run_id, usize_to_i64(step)],
        )
        .map_err(|error| format!("trim plan output lines: {error}"))?;
        return Ok(());
    }
    let deleted = conn
        .execute(
            "delete from plan_output_line
             where run_id = ?1
               and step = ?2
               and line_number not in (
                 select line_number
                 from plan_output_line
                 where run_id = ?1 and step = ?2
                 order by line_number desc
                 limit ?3
               )",
            params![
                run_id,
                usize_to_i64(step),
                usize_to_i64(retained_line_count),
            ],
        )
        .map_err(|error| format!("trim plan output lines: {error}"))?;
    if deleted == 0 {
        return Ok(());
    }
    let first_retained: Option<i64> = conn
        .query_row(
            "select min(line_number) from plan_output_line where run_id = ?1 and step = ?2",
            params![run_id, usize_to_i64(step)],
            |row| row.get(0),
        )
        .map_err(|error| format!("read retained plan output marker position: {error}"))?;
    let Some(first_retained) = first_retained else {
        return Ok(());
    };
    let marker_line = first_retained.saturating_sub(1);
    conn.execute(
        "insert or replace into plan_output_line (
           run_id, step, line_number, time_unix_ms, kind, text, block_id
         ) values (?1, ?2, ?3, ?4, 'system', ?5, null)",
        params![
            run_id,
            usize_to_i64(step),
            marker_line,
            u64_to_i64(unix_ms()),
            format!("[... omitted {deleted} older output lines ...]"),
        ],
    )
    .map_err(|error| format!("write plan output omission marker: {error}"))?;
    Ok(())
}
