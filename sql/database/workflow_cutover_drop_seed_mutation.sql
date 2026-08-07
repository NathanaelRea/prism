insert into auto_run (
  id, repo_root, worktree_path, branch, mode, variant, prompt_summary,
  initial_prompt, status, created_unix_ms, updated_unix_ms
) values (
  'terminal-with-armed-merge', '/repo', '/repo/worktree', 'feature', 'prompt',
  'default', 'done', 'done', 'completed', 1, 1
);

insert into merge_intent (
  id, run_id, generation, state, placement, created_unix_ms, updated_unix_ms
) values (1, 'terminal-with-armed-merge', 1, 'armed', 'queue', 1, 1);

insert into integration_lane (
  lane_key, reserved_intent_id, updated_unix_ms
) values ('default', 1, 1);
