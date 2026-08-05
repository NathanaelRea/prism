update plan_run
set pause_requested = 1,
    status = case when exists(
      select 1 from plan_step_run where run_id = ? and status in ('starting','running')
    ) then status else 'paused' end,
    updated_unix_ms = ?
where id = ? and status not in ('done','failed','aborted')
