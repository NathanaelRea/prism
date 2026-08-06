select repo_root, harness_id, branch, worktree_path, server_port, server_url,
       server_pid, opencode_session_id, generation, updated_unix_ms,
       server_start_time_ticks
from opencode_runtime
where repo_root = ? and harness_id = ? and branch = ? and worktree_path = ?
