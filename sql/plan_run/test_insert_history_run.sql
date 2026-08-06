insert into plan_run (
  id, harness_id, adapter_id, repo_root, scope_path, plan_path, plan_display,
  step_name, start_step, total_steps, mode, status, pause_requested,
  selected_step, created_unix_ms, updated_unix_ms
) values (
  'plan-history-87654321', 'opencode', 'opencode', ?1, ?1, ?2, 'old-plan.md',
  'phase', 1, 1, 'sequential', 'done', 0, 1, 1, 5
)
