update plan_run set status = 'queued', pause_requested = 0, updated_unix_ms = ?
where id in (
  select plan_run_id from auto_step_run where run_id = ? and plan_run_id is not null
) and status in ('queued', 'running', 'paused')
  and exists (
    select 1 from plan_step_run s where s.run_id = plan_run.id and s.status = 'queued'
  )
