update auto_run set pause_requested = 0, status = 'running', updated_unix_ms = ?
where id = ? and pause_requested = 1
