insert into worktree_harness (
  branch, worktree_path, worktree_incarnation, harness_id, migration_policy, updated_unix_ms
) values (?, ?, ?, ?, ?, ?)
on conflict(branch) do update set
  worktree_path = excluded.worktree_path,
  worktree_incarnation = excluded.worktree_incarnation,
  harness_id = excluded.harness_id,
  migration_policy = excluded.migration_policy,
  updated_unix_ms = excluded.updated_unix_ms
