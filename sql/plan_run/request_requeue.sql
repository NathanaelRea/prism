update workflow_execution
set requeue_requested = 1, updated_unix_ms = ?
where workflow_kind = 'plan' and run_id = ? and dispatch_state = 'claimed'
