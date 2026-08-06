update plan_step_run set status = 'queued', started_unix_ms = null, finished_unix_ms = null,
 execution_state = null, execution_process_id = null, execution_process_start_time_ticks = null,
 process_id = null, error = null where run_id = ? and status in ('starting', 'running')
