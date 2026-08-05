update startup_run
set time_finished_unix_ms = ?, status = ?, error = ?
where id = ? and status = 'running'
