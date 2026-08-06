update notification_outbox
set delivery_state = 'superseded', superseded_unix_ms = ?
where worktree_path = ? and branch = ? and incarnation = ?
  and kind = ? and delivery_state = 'pending'
