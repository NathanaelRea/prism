select
  branch as "branch!",
  worktree_path as "worktree_path!",
  worktree_incarnation as "worktree_incarnation!",
  branch_oid as "branch_oid!",
  warnings_json as "warnings_json!"
from deferred_merge_cleanup
where branch = ?
