update workflow_execution set dispatch_state = 'recovery_pending', worker_id = null,
 daemon_instance_id = null, lease_expires_unix_ms = null, executor_pid = null,
 executor_process_identity = null, requeue_requested = 0,
 interruption_generation = interruption_generation + 1, fencing_token = fencing_token + 1,
 updated_unix_ms = ?
where dispatch_state = 'claimed' and (daemon_instance_id != ? or lease_expires_unix_ms <= ?)
