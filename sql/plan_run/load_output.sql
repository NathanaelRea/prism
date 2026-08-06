select run_id, step, line_number, time_unix_ms, kind, text, block_id
from plan_output_line
where run_id = ? and step = ?
order by line_number
