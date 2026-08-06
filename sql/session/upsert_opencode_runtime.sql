insert into opencode_runtime (
  repo_root, harness_id, branch, worktree_path, server_port, server_url, server_pid,
  opencode_session_id, generation, updated_unix_ms, server_start_time_ticks
) values (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
on conflict(repo_root, harness_id, branch, worktree_path) do update set
  server_port = excluded.server_port,
  server_url = excluded.server_url,
  server_pid = excluded.server_pid,
  opencode_session_id = excluded.opencode_session_id,
  generation = excluded.generation,
  updated_unix_ms = excluded.updated_unix_ms,
  server_start_time_ticks = excluded.server_start_time_ticks
