insert into workflow_execution (
  workflow_kind, run_id, dispatch_state, fencing_token,
  interruption_generation, created_unix_ms, updated_unix_ms
) values ('auto', ?, 'queued', 0, 0, ?, ?)
on conflict(workflow_kind, run_id) do update set
  dispatch_state = 'queued', worker_id = null, daemon_instance_id = null,
  lease_expires_unix_ms = null, heartbeat_unix_ms = null, executor_pid = null,
  executor_process_identity = null, requeue_requested = 0,
  recovery_decided_unix_ms = null,
  fencing_token = workflow_execution.fencing_token + 1,
  updated_unix_ms = excluded.updated_unix_ms
where workflow_execution.dispatch_state != 'claimed'
