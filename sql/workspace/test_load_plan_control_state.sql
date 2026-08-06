select p.status, p.pause_requested, e.dispatch_state
from plan_run p
join workflow_execution e on e.run_id = p.id
where p.id = 'plan-control-12345678'
