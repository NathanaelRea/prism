update plan_run set pause_requested = 0,
  status = case when exists(
    select 1 from plan_step_run where run_id = ? and status in ('starting','running')
  ) then 'running' else 'queued' end,
  updated_unix_ms = ?
where id = ? and (pause_requested = 1 or status = 'paused')
