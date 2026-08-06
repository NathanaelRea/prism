select
  attempt.id as "id!: String",
  attempt.step_id as "step_id!: String",
  attempt.status as "status!: String",
  attempt.worker_id as "worker_id!: String",
  attempt.target_id as "target_id!: String",
  attempt.fencing_token as "fencing_token!: i64",
  attempt.process_id,
  attempt.process_start_time_ticks,
  attempt.started_unix_ms as "started_unix_ms!: i64",
  attempt.finished_unix_ms
from step_attempt attempt
join workflow_step step on step.id = attempt.step_id
where step.run_id = ?
order by attempt.started_unix_ms, attempt.id
