update auto_step_run
set status = 'running', execution_process_id = ?,
    execution_process_start_time_ticks = ?
where id = ? and status = 'starting'
