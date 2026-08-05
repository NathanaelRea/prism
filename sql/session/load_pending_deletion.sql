select branch as "branch!", worktree_path, worktree_incarnation, branch_oid,
       worktree_removed, branch_deleted
from pending_worktree_deletion
where branch = ?
