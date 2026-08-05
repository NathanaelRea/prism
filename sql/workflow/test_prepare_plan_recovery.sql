update workflow_execution
set dispatch_state = 'recovery_pending', interruption_generation = 3, updated_unix_ms = 30
where run_id = 'plan-control-12345678'
