insert into workflow_execution (
  workflow_kind, run_id, dispatch_state, fencing_token,
  interruption_generation, created_unix_ms, updated_unix_ms
) values ('plan', 'plan-history-87654321', 'terminal', 0, 0, 1, 5)
