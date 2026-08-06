select branch as "branch!"
from agent_state
where branch not in (select branch from task_metadata)
  and branch not in (select branch from archived_worktree)
