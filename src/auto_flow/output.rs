use super::*;

pub fn append_output_line(
    conn: &rusqlite::Connection,
    line: &AutoOutputLine,
) -> Result<(), String> {
    append_output_line_limited(conn, line, 0)
}

pub fn append_output_line_limited(
    conn: &rusqlite::Connection,
    line: &AutoOutputLine,
    max_lines_per_step: usize,
) -> Result<(), String> {
    conn.execute(
        "insert or replace into auto_output_line (
           step_run_id, line_number, time_unix_ms, kind, text, block_id
         ) values (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            line.step_run_id,
            u64_to_i64(line.line_number),
            u64_to_i64(line.time_unix_ms),
            line.kind.as_str(),
            line.text.as_str(),
            line.block_id.as_deref(),
        ],
    )
    .map_err(|error| format!("write auto output line: {error}"))?;
    trim_output_lines(conn, line.step_run_id, max_lines_per_step)
}

pub fn load_output_lines(
    conn: &rusqlite::Connection,
    step_run_id: i64,
) -> Result<Vec<AutoOutputLine>, String> {
    let mut statement = conn
        .prepare(
            "select step_run_id, line_number, time_unix_ms, kind, text, block_id
             from auto_output_line
             where step_run_id = ?1
             order by line_number",
        )
        .map_err(|error| format!("prepare auto output load: {error}"))?;
    let rows = statement
        .query_map(params![step_run_id], |row| {
            let kind: String = row.get(3)?;
            Ok(AutoOutputLine {
                step_run_id: row.get(0)?,
                line_number: i64_to_u64(row.get(1)?, 1),
                time_unix_ms: i64_to_u64(row.get(2)?, 2),
                kind: AutoOutputKind::parse(&kind).map_err(from_string_error)?,
                text: row.get(4)?,
                block_id: row.get(5)?,
            })
        })
        .map_err(|error| format!("load auto output lines: {error}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read auto output lines: {error}"))
}

pub fn append_auto_event(conn: &rusqlite::Connection, event: &AutoEvent) -> Result<i64, String> {
    conn.execute(
        "insert into auto_event (
           run_id, step_run_id, time_unix_ms, kind, data_json
         ) values (?1, ?2, ?3, ?4, ?5)",
        params![
            event.run_id.as_str(),
            event.step_run_id,
            u64_to_i64(event.time_unix_ms),
            event.kind.as_str(),
            event.data_json.as_str(),
        ],
    )
    .map_err(|error| format!("write auto event: {error}"))?;
    emit_auto_event_log(event);
    Ok(conn.last_insert_rowid())
}

pub(super) fn append_step_status_output(
    conn: &rusqlite::Connection,
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
    conn: &rusqlite::Connection,
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
    conn: &rusqlite::Connection,
    step_run_id: i64,
) -> Result<u64, String> {
    let current: Option<i64> = conn
        .query_row(
            "select max(line_number) from auto_output_line where step_run_id = ?1",
            params![step_run_id],
            |row| row.get(0),
        )
        .map_err(|error| format!("read auto output line number: {error}"))?;
    Ok(current.map(i64_to_next_u64).unwrap_or(1))
}

pub(super) fn trim_output_lines(
    conn: &rusqlite::Connection,
    step_run_id: i64,
    max_lines_per_step: usize,
) -> Result<(), String> {
    if max_lines_per_step == 0 {
        return Ok(());
    }
    let retained_line_count = max_lines_per_step.saturating_sub(1);
    if retained_line_count == 0 {
        conn.execute(
            "delete from auto_output_line where step_run_id = ?1",
            params![step_run_id],
        )
        .map_err(|error| format!("trim auto output lines: {error}"))?;
        return Ok(());
    }
    let deleted = conn
        .execute(
            "delete from auto_output_line
             where step_run_id = ?1
               and line_number not in (
                 select line_number
                 from auto_output_line
                 where step_run_id = ?1
                 order by line_number desc
                 limit ?2
               )",
            params![step_run_id, usize_to_i64(retained_line_count)],
        )
        .map_err(|error| format!("trim auto output lines: {error}"))?;
    if deleted == 0 {
        return Ok(());
    }
    let first_retained: Option<i64> = conn
        .query_row(
            "select min(line_number) from auto_output_line where step_run_id = ?1",
            params![step_run_id],
            |row| row.get(0),
        )
        .map_err(|error| format!("read retained auto output marker position: {error}"))?;
    let Some(first_retained) = first_retained else {
        return Ok(());
    };
    let marker_line = first_retained.saturating_sub(1);
    conn.execute(
        "insert or replace into auto_output_line (
           step_run_id, line_number, time_unix_ms, kind, text, block_id
         ) values (?1, ?2, ?3, 'system', ?4, null)",
        params![
            step_run_id,
            marker_line,
            u64_to_i64(unix_ms()),
            format!("[... omitted {deleted} older output lines ...]"),
        ],
    )
    .map_err(|error| format!("write auto output omission marker: {error}"))?;
    Ok(())
}
