insert into effect_intent (
  id, run_id, attempt_id, fencing_token, effect_kind, idempotency_key,
  status, request_json, created_unix_ms, updated_unix_ms
)
select ?, step.run_id, attempt.id, attempt.fencing_token, ?, ?,
       'prepared', ?, ?, ?
from step_attempt attempt
join workflow_step step on step.id = attempt.step_id
where attempt.id = ? and attempt.status = 'claimed'
  and attempt.worker_id = ? and attempt.target_id = ? and attempt.fencing_token = ?
  and attempt.lease_expires_unix_ms > ?
  and exists (
    select 1 from authority_grant grant_record
    where grant_record.run_id = step.run_id and grant_record.scope = ?
      and (grant_record.expires_unix_ms is null or grant_record.expires_unix_ms > ?)
  )
on conflict(idempotency_key) do nothing
returning id
