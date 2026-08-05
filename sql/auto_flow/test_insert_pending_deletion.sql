insert into pending_worktree_deletion (
  branch, worktree_path, worktree_incarnation, branch_oid,
  worktree_removed, branch_deleted, updated_unix_ms
) values (?, ?, ?, 'branch-oid', 0, 0, 0)
