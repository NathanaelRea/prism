update auto_run set pause_requested = 0,
  status = case when exists(
    select 1 from auto_step_run where run_id = ? and status in ('starting','running','waiting')
  ) then 'running' else 'queued' end,
  updated_unix_ms = ?
where id = ? and (pause_requested = 1 or status = 'paused')
