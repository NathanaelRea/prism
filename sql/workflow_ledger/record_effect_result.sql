update effect_intent
set status = ?, result_json = ?, updated_unix_ms = ?
where id = ? and status in ('prepared', 'dispatching')
  and exists (
    select 1 from step_attempt attempt
    where attempt.id = effect_intent.attempt_id and attempt.status = 'claimed'
      and attempt.worker_id = ? and attempt.target_id = ?
      and attempt.fencing_token = effect_intent.fencing_token
      and attempt.fencing_token = ? and attempt.lease_expires_unix_ms > ?
  )
