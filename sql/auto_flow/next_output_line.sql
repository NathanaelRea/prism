select coalesce(max(line_number), 0) + 1 as "line_number!: i64"
from auto_output_line
where step_run_id = ?
