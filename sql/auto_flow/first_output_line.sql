select min(line_number) as "line_number: i64"
from auto_output_line
where step_run_id = ?
