update notification_session
set state = ?, transition_sequence = ?, observed_unix_ms = ?
where worktree_path = ? and branch = ? and incarnation = ?
