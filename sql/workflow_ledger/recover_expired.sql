update step_attempt
set status = 'expired', finished_unix_ms = ?
where status = 'claimed' and lease_expires_unix_ms <= ?
returning id, step_id, worker_id, target_id, fencing_token, process_id, process_start_time_ticks
