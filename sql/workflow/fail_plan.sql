update plan_run set status = 'failed', updated_unix_ms = ?
where id = ? and status not in ('aborted', 'done')
