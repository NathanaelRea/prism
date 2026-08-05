insert into archived_worktree (
  branch, repo_root, worktree_path, archived_unix_ms, classification
) values (?, ?, ?, ?, ?)
on conflict(branch) do update set
  repo_root = excluded.repo_root,
  worktree_path = excluded.worktree_path,
  archived_unix_ms = excluded.archived_unix_ms,
  classification = excluded.classification
