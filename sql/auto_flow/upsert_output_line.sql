insert into auto_output_line (
  step_run_id, line_number, time_unix_ms, kind, text, block_id
) values (?, ?, ?, ?, ?, ?)
on conflict(step_run_id, line_number) do update set
  time_unix_ms = excluded.time_unix_ms,
  kind = excluded.kind,
  text = excluded.text,
  block_id = excluded.block_id
