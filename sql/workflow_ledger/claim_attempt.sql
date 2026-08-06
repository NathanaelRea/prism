insert into step_attempt (
  id, step_id, attempt_number, status, worker_id, target_id,
  fencing_token, lease_expires_unix_ms, started_unix_ms
)
select ?, step.id,
       coalesce((select max(previous.attempt_number) + 1 from step_attempt previous where previous.step_id = step.id), 1),
       'claimed', ?, step.target_id,
       coalesce((select max(previous.fencing_token) + 1 from step_attempt previous where previous.step_id = step.id), 1),
       ?, ?
from workflow_step step
where step.id = ? and step.status = 'runnable' and step.available_unix_ms <= ?
  and not exists (
    select 1 from step_attempt active
    where active.step_id = step.id and active.status = 'claimed'
  )
returning id as "id!", step_id, worker_id, target_id, fencing_token, lease_expires_unix_ms
