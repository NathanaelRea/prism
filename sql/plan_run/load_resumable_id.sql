select id as "id!: String"
from plan_run
where repo_root = ? and scope_path = ? and plan_path = ? and step_name = ?
  and start_step = ? and total_steps = ? and mode = ? and harness_id = ? and adapter_id = ?
  and archived_unix_ms is null and status in ('queued', 'running', 'paused')
order by updated_unix_ms desc
limit 1
