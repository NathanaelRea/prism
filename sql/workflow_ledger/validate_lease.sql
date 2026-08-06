select exists(
  select 1 from step_attempt
  where id = ? and status = 'claimed' and worker_id = ? and target_id = ?
    and fencing_token = ? and lease_expires_unix_ms > ?
)
