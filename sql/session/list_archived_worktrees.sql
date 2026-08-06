select branch as "branch!", worktree_path, classification
from archived_worktree
order by archived_unix_ms desc, branch asc
