insert into startup_run (
  id, time_started_unix_ms, time_finished_unix_ms, status, repo, version, error
) values (?, ?, null, 'running', ?, ?, null)
