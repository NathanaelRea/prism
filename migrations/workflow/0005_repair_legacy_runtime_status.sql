-- Migration 0003 introduced runtime projections with defaults that did not reflect
-- rows created by the earlier ledger. Repair only those default values so newer
-- specialized runtime states (for example waiting_child or archived) are kept.
update workflow_run
set runtime_status = status
where runtime_status = 'runnable'
  and status <> 'runnable';

update workflow_step
set runtime_status = case status
  when 'claimed' then 'running'
  else status
end
where runtime_status = 'waiting'
  and status <> 'waiting';
