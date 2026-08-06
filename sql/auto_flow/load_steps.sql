select
  id as "id!", run_id as "run_id!", sequence as "sequence!",
  step_key as "step_key!", reason, status as "status!", attempt as "attempt!",
  started_unix_ms, finished_unix_ms, execution_state, session_endpoint,
  session_id, execution_process_id, plan_run_id, commit_sha, head_sha,
  work_guard_json, blocker, summary, error, session_adapter_id,
  execution_process_start_time_ticks
from auto_step_run
where run_id = ?
order by sequence
