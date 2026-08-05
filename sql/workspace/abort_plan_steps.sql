update plan_step_run set status = 'aborted', finished_unix_ms = ?,
  error = coalesce(error, 'aborted'), execution_process_id = null,
  execution_process_start_time_ticks = null, process_id = null
where run_id = ? and status in ('queued','starting','running')
