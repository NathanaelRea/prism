update plan_run set pause_requested = 0, status = 'aborted', updated_unix_ms = ?
where id in (select plan_run_id from auto_step_run where run_id = ? and plan_run_id is not null)
  and status not in ('done','failed','aborted')
