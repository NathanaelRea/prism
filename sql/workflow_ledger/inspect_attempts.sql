select
  attempt.id,
  attempt.step_id,
  attempt.status,
  attempt.worker_id,
  attempt.target_id,
  attempt.fencing_token,
  attempt.process_id,
  attempt.process_start_time_ticks,
  attempt.started_unix_ms,
  attempt.finished_unix_ms
  , attempt.input_revisions_json
from step_attempt attempt
join workflow_step step on step.id = attempt.step_id
where step.run_id = ?
order by attempt.started_unix_ms, attempt.id
