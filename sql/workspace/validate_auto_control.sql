select r.status, r.pause_requested, r.updated_unix_ms, e.dispatch_state,
       coalesce(e.interruption_generation, 0) as "interruption_generation!: i64"
from auto_run r
left join workflow_execution e on e.workflow_kind = 'auto' and e.run_id = r.id
where r.id = ?
