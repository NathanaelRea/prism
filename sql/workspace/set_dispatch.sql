update workflow_execution set dispatch_state = ?, worker_id = null,
  daemon_instance_id = null, lease_expires_unix_ms = null, executor_pid = null,
  executor_process_identity = null, requeue_requested = 0,
  fencing_token = fencing_token + 1, updated_unix_ms = ?
where workflow_kind = ? and run_id = ?
