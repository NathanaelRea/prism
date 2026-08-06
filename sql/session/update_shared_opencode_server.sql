update opencode_runtime
set server_port = ?, server_url = ?, server_pid = ?,
    server_start_time_ticks = ?, updated_unix_ms = ?
where repo_root = ? and harness_id = ?
  and (server_port != ? or server_url != ? or server_pid is not ?
       or server_start_time_ticks is not ?)
