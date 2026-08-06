select state as "state!", transition_sequence as "transition_sequence!"
from notification_session
where worktree_path = ? and branch = ? and incarnation = ?
