insert into workflow_run (
  id, definition_snapshot_id, repository, status, created_unix_ms, updated_unix_ms
)
select ?, ?, ?, 'runnable', ?, ?
where not exists (
  select 1 from idempotency_record where scope = 'manual_invocation' and key = ?
)
