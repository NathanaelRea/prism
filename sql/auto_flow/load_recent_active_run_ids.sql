select id as "id!"
from auto_run
where repo_root = ?
  and archived_unix_ms is null
  and (status in ('queued', 'running', 'paused', 'failed')
       or pending_push_json is not null
       or stabilization_status in ('observing', 'blocked', 'waiting', 'ready'))
order by
  case status
    when 'running' then 0
    when 'queued' then 1
    when 'paused' then 2
    when 'failed' then 3
    else 4
  end,
  updated_unix_ms desc
limit ?
