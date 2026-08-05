insert into plan_step_run (
  run_id, step, prompt, status, execution_state, session_endpoint, session_id,
  agent_variant, execution_process_id, started_unix_ms, finished_unix_ms, exit_code,
  latest_message, active_tool, todos_json, summary, error, session_adapter_id,
  execution_process_start_time_ticks
) values (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
on conflict(run_id, step) do update set
  prompt = excluded.prompt,
  status = excluded.status,
  execution_state = excluded.execution_state,
  session_endpoint = excluded.session_endpoint,
  session_id = excluded.session_id,
  agent_variant = excluded.agent_variant,
  execution_process_id = excluded.execution_process_id,
  started_unix_ms = excluded.started_unix_ms,
  finished_unix_ms = excluded.finished_unix_ms,
  exit_code = excluded.exit_code,
  latest_message = excluded.latest_message,
  active_tool = excluded.active_tool,
  todos_json = excluded.todos_json,
  summary = excluded.summary,
  error = excluded.error,
  session_adapter_id = excluded.session_adapter_id,
  execution_process_start_time_ticks = excluded.execution_process_start_time_ticks
where plan_step_run.status != 'aborted' or excluded.status = 'queued'
