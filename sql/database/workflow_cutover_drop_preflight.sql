select
  (select count(*) from workflow_execution
   where dispatch_state in ('queued', 'claimed', 'recovery_pending'))
  + (select count(*) from plan_run where status in ('queued', 'running', 'paused'))
  + (select count(*) from auto_run where status in ('queued', 'running', 'paused', 'waiting'))
  + (select count(*) from merge_intent where state = 'armed')
  + (select count(*) from integration_lane where reserved_intent_id is not null)
