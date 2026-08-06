insert into auto_step_run (
  run_id, sequence, step_key, reason, status, attempt, started_unix_ms,
  finished_unix_ms, execution_state, session_endpoint, session_id,
  execution_process_id, plan_run_id, commit_sha, head_sha, work_guard_json,
  blocker, summary, error, session_adapter_id,
  execution_process_start_time_ticks
) values (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
returning id as "id!"
