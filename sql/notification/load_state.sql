select state as "state!" from notification_session
where worktree_path = ? and branch = ? and incarnation = ?
