update plan_run set pause_requested = 0,
  status = case when exists(
    select 1 from plan_step_run s where s.run_id = plan_run.id and s.status in ('starting','running')
  ) then 'running' else 'queued' end,
  updated_unix_ms = ?
where id in (select plan_run_id from auto_step_run where run_id = ? and plan_run_id is not null)
  and (pause_requested = 1 or status = 'paused')
