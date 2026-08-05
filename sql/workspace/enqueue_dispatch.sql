insert into workflow_execution (
  workflow_kind, run_id, dispatch_state, fencing_token, requeue_requested,
  interruption_generation, created_unix_ms, updated_unix_ms
) values (?, ?, 'queued', 0, 0, 0, ?, ?)
on conflict(workflow_kind, run_id) do update set
  dispatch_state = case when workflow_execution.dispatch_state = 'claimed' then 'claimed' else 'queued' end,
  requeue_requested = case when workflow_execution.dispatch_state = 'claimed' then 1 else 0 end,
  worker_id = case when workflow_execution.dispatch_state = 'claimed' then worker_id else null end,
  daemon_instance_id = case when workflow_execution.dispatch_state = 'claimed' then daemon_instance_id else null end,
  updated_unix_ms = excluded.updated_unix_ms
