insert into plan_run (
  id, harness_id, repo_root, scope_path, plan_path, plan_display, step_name, start_step,
  total_steps, mode, status, pause_requested, selected_step, created_unix_ms,
  updated_unix_ms, archived_unix_ms, adapter_id
) values (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
on conflict(id) do update set
  repo_root = excluded.repo_root,
  harness_id = excluded.harness_id,
  adapter_id = excluded.adapter_id,
  scope_path = excluded.scope_path,
  plan_path = excluded.plan_path,
  plan_display = excluded.plan_display,
  step_name = excluded.step_name,
  start_step = excluded.start_step,
  total_steps = excluded.total_steps,
  mode = excluded.mode,
  status = excluded.status,
  pause_requested = excluded.pause_requested,
  selected_step = excluded.selected_step,
  updated_unix_ms = excluded.updated_unix_ms,
  archived_unix_ms = excluded.archived_unix_ms
where plan_run.status != 'aborted' or excluded.status = 'queued'
