select branch as "branch!", worktree
from task_metadata
where branch not in (select branch from archived_worktree)
