select worktree_path, worktree_incarnation, harness_id, migration_policy
from worktree_harness
where branch = ?
