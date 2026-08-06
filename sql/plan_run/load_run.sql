select id as "id!", harness_id, adapter_id, repo_root, scope_path, plan_path, plan_display,
  step_name, start_step, total_steps, mode, status, pause_requested, selected_step,
  created_unix_ms, updated_unix_ms, archived_unix_ms
from plan_run
where id = ?
