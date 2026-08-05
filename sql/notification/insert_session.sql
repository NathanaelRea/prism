insert into notification_session (
  worktree_path, branch, incarnation, state, transition_sequence, observed_unix_ms
) values (?, ?, ?, ?, 0, ?)
