select
  output.attempt_id,
  output.sequence,
  output.stream,
  output.body,
  output.time_unix_ms
from attempt_output output
join step_attempt attempt on attempt.id = output.attempt_id
join workflow_step step on step.id = attempt.step_id
where step.run_id = ?
order by output.attempt_id, output.sequence
