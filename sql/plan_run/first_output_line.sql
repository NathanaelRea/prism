select coalesce(min(line_number), -1) as "line_number!: i64"
from plan_output_line
where run_id = ? and step = ?
