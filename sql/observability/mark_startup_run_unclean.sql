update startup_run
set time_finished_unix_ms = ?, status = 'unclean',
    error = 'process exited without completing its run marker'
where id = ? and status = 'running'
