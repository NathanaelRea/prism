select branch as "branch!", worktree_path
from opencode_runtime
where branch not in (select branch from task_metadata)
  and branch not in (select branch from archived_worktree)
