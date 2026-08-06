update auto_step_run
set status = ?, execution_process_id = null,
    execution_process_start_time_ticks = null, finished_unix_ms = ?, error = ?
where id = ? and status != 'aborted'
