update plan_step_run set
  status = 'queued', started_unix_ms = null, finished_unix_ms = null,
  execution_state = null, execution_process_id = null,
  execution_process_start_time_ticks = null, process_id = null, error = null
where run_id in (
  select plan_run_id from auto_step_run where run_id = ? and plan_run_id is not null
) and status in ('starting', 'running')
