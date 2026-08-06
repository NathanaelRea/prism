select id as "id!: String"
from plan_run
where repo_root = ? and archived_unix_ms is null
order by case status
  when 'running' then 0 when 'queued' then 1 when 'paused' then 2
  when 'failed' then 3 else 4 end,
  updated_unix_ms desc
limit ?
