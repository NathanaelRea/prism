update plan_run set pause_requested = 0, status = 'aborted', updated_unix_ms = ?
where id = ? and status not in ('done','aborted')
