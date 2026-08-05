update auto_run
set pause_requested = 1,
    status = case when exists(
      select 1 from auto_step_run where run_id = ? and status in ('starting','running','waiting')
    ) then status else 'paused' end,
    updated_unix_ms = ?
where id = ? and status not in ('done','failed','aborted')
