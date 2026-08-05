update workflow_execution set dispatch_state = ?, recovery_decided_unix_ms = ?, updated_unix_ms = ?,
 requeue_requested = 0, fencing_token = fencing_token + 1,
 interruption_generation = interruption_generation + 1
where workflow_kind = ? and run_id = ? and dispatch_state = 'recovery_pending'
 and interruption_generation = ?
