insert into deferred_merge_cleanup (
  branch, worktree_path, worktree_incarnation, branch_oid, warnings_json, updated_unix_ms
) values (?, ?, ?, ?, ?, ?)
on conflict(branch) do update set
  worktree_path = excluded.worktree_path,
  worktree_incarnation = excluded.worktree_incarnation,
  branch_oid = excluded.branch_oid,
  warnings_json = excluded.warnings_json,
  updated_unix_ms = excluded.updated_unix_ms
