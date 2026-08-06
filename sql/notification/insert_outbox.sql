insert into notification_outbox (
  worktree_path, branch, incarnation, transition_sequence, kind, title, body,
  observed_unix_ms, expires_unix_ms, delivery_state, available_unix_ms
) values (?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?)
returning id as "id!"
