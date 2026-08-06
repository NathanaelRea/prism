select run_id, step, prompt, status, execution_state, session_endpoint, session_id,
  agent_variant, execution_process_id, started_unix_ms, finished_unix_ms, exit_code,
  latest_message, active_tool, todos_json, summary, error, session_adapter_id,
  execution_process_start_time_ticks
from plan_step_run
where run_id = ?
order by step
