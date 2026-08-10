-- Workflow execution deadlines are opt-in. Retry policy is snapshotted as a complete value so
-- scheduling remains deterministic across package or configuration changes.
alter table workflow_step add column timeout_seconds integer check (timeout_seconds is null or timeout_seconds > 0);
alter table workflow_step add column timeout_policy text not null default 'fail'
  check (timeout_policy in ('fail','input_required'));
alter table workflow_step add column retry_json text not null default '{"max_attempts":1,"on":["transient"],"initial_delay_seconds":2,"max_delay_seconds":60}';

update workflow_step
set retry_json = json_object(
  'max_attempts', retry_max_attempts,
  'on', json_array('transient'),
  'initial_delay_seconds', 2,
  'max_delay_seconds', 60
)
where retry_max_attempts > 1;
