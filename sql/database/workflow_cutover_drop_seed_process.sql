insert into plan_run (
  id, repo_root, scope_path, plan_path, plan_display, step_name, start_step,
  total_steps, mode, status, selected_step, created_unix_ms, updated_unix_ms
) values ('process-owner', '/repo', '/repo', '/repo/work.md', 'work.md', 'work', 1,
          1, 'sequential', 'done', 1, 1, 1)
