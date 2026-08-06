select exists(
 select 1 from workflow_execution where workflow_kind = ? and run_id = ?
  and dispatch_state = 'recovery_pending' and interruption_generation = ?
) as "current!: bool"
