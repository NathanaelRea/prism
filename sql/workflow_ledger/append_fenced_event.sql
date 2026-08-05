insert into audit_event (run_id, step_id, attempt_id, sequence, kind, time_unix_ms, data_json)
select step.run_id, attempt.step_id, attempt.id,
       coalesce((select max(event.sequence) + 1 from audit_event event where event.run_id = step.run_id), 1),
       ?, ?, ?
from step_attempt attempt
join workflow_step step on step.id = attempt.step_id
where attempt.id = ? and attempt.status = 'claimed'
  and attempt.worker_id = ? and attempt.target_id = ? and attempt.fencing_token = ?
  and attempt.lease_expires_unix_ms > ?
