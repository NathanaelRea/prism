update plan_step_run set status = 'aborted', finished_unix_ms = ?,
  error = coalesce(error, 'aborted'), execution_process_id = null,
  execution_process_start_time_ticks = null, process_id = null
where run_id in (select plan_run_id from auto_step_run where run_id = ? and plan_run_id is not null)
  and status in ('queued','starting','running')
