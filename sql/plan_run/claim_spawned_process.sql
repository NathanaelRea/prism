update plan_step_run
set status = 'running', execution_process_id = ?, execution_process_start_time_ticks = ?
where run_id = ? and step = ? and status = 'starting'
