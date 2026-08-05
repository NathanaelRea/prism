update plan_step_run
set status = ?, execution_process_id = null, execution_process_start_time_ticks = null,
  finished_unix_ms = ?, exit_code = ?, active_tool = ?, error = ?
where run_id = ? and step = ? and status != 'aborted'
