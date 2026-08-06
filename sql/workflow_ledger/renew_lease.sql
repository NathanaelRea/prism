update step_attempt
set lease_expires_unix_ms = ?
where id = ? and status = 'claimed' and worker_id = ? and target_id = ?
  and fencing_token = ? and lease_expires_unix_ms > ?
  and exists (
    select 1
    from workflow_step step
    join workflow_run run on run.id = step.run_id
    where step.id = step_attempt.step_id and run.status <> 'cancelled'
  )
