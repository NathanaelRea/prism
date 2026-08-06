insert into plan_run (
  id, harness_id, adapter_id, repo_root, scope_path, plan_path, plan_display,
  step_name, start_step, total_steps, mode, status, pause_requested,
  selected_step, created_unix_ms, updated_unix_ms
) values (
  'plan-control-12345678', 'opencode', 'opencode', ?1, ?1, ?2, 'plan.md',
  'phase', 1, 2, 'sequential', 'queued', 0, 1, 10, 20
)
