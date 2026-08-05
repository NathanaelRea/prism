insert into auto_run (
  id, repo_root, worktree_path, branch, mode, variant, prompt_summary,
  initial_prompt, status, created_unix_ms, updated_unix_ms
) values (
  'auto-delete', '/repo', '/repo/worktree', 'branch', 'standard', 'default',
  'delete test', 'delete test', 'queued', 1, 1
);
insert into auto_step_run (run_id, sequence, step_key, status, attempt)
values ('auto-delete', 1, 'prepare', 'queued', 1);
insert into auto_output_line (step_run_id, line_number, time_unix_ms, kind, text)
select id, 1, 1, 'system', 'test' from auto_step_run where run_id = 'auto-delete';
insert into auto_event (run_id, time_unix_ms, kind, data_json)
values ('auto-delete', 1, 'test', '{}');
