update step_attempt
set status = 'cancelled', result_json = '{"reason":"dispatch_handoff_failed"}', finished_unix_ms = ?
where id = ? and status = 'claimed' and worker_id = ? and target_id = ?
  and fencing_token = ? and lease_expires_unix_ms > ?
