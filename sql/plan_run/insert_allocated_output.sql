insert into plan_output_line (
  run_id, step, line_number, time_unix_ms, kind, text, block_id
)
select ?, ?, coalesce(max(line_number), 0) + 1, ?, ?, ?, ?
from plan_output_line
where run_id = ? and step = ?
