select occurrence.id, occurrence.trigger_id, definition.definition_snapshot_id,
       definition.config_json, definition.trigger_kind, definition.schedule_json,
       checkpoint.checkpoint_json, occurrence.input_json,
       occurrence.provider_item_id, occurrence.deduplication_key, occurrence.due_unix_ms
from trigger_occurrence occurrence
join trigger_definition definition on definition.id = occurrence.trigger_id
left join trigger_checkpoint checkpoint on checkpoint.trigger_id=definition.id
where definition.enabled = 1 and occurrence.status = 'pending'
  and occurrence.due_unix_ms <= ?
  and (
    definition.overlap_policy = 'parallel'
    or not exists (
      select 1 from trigger_occurrence previous
      join workflow_run active_run on active_run.id = previous.run_id
      where previous.trigger_id = occurrence.trigger_id
        and (
          (previous.provider_item_id is null and occurrence.provider_item_id is null)
          or previous.provider_item_id = occurrence.provider_item_id
        )
        and active_run.status in ('waiting', 'runnable', 'running', 'paused')
    )
  )
order by occurrence.due_unix_ms, occurrence.id
limit ?
