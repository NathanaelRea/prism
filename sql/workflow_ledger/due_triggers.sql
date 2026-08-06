select occurrence.id, occurrence.trigger_id, definition.definition_snapshot_id,
       definition.config_json, occurrence.deduplication_key, occurrence.due_unix_ms
from trigger_occurrence occurrence
join trigger_definition definition on definition.id = occurrence.trigger_id
where definition.enabled = 1 and occurrence.status = 'pending'
  and occurrence.due_unix_ms <= ?
  and (
    definition.overlap_policy = 'allow'
    or not exists (
      select 1 from trigger_occurrence previous
      join workflow_run active_run on active_run.id = previous.run_id
      where previous.trigger_id = occurrence.trigger_id
        and active_run.status in ('waiting', 'runnable', 'running', 'paused')
    )
  )
order by occurrence.due_unix_ms, occurrence.id
limit ?
