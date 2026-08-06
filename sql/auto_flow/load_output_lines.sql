select
  step_run_id as "step_run_id!", line_number as "line_number!",
  time_unix_ms as "time_unix_ms!", kind as "kind!", text as "text!", block_id
from auto_output_line
where step_run_id = ?
order by line_number
