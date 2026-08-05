select 'auto' as "kind!: String", r.id as "run_id!", r.worktree_path as "worktree_path!",
       r.status as "lifecycle!", r.pause_requested, r.updated_unix_ms,
       e.dispatch_state, e.daemon_instance_id, e.worker_id, e.lease_expires_unix_ms,
       e.heartbeat_unix_ms, coalesce(e.interruption_generation, 0) as "interruption_generation!: i64",
       e.updated_unix_ms as dispatch_updated_unix_ms,
       (select s.step_key from auto_step_run s where s.run_id = r.id order by s.sequence desc limit 1) as current_step,
       (select s.status from auto_step_run s where s.run_id = r.id order by s.sequence desc limit 1) as current_step_state,
       (select count(*) from auto_step_run s where s.run_id = r.id and s.status = 'done') as "completed!: i64",
       (select count(*) from auto_step_run s where s.run_id = r.id) as "total!: i64"
from auto_run r
left join workflow_execution e on e.workflow_kind = 'auto' and e.run_id = r.id
where r.repo_root = ? and r.archived_unix_ms is null
union all
select 'plan', r.id, r.scope_path, r.status, r.pause_requested, r.updated_unix_ms,
       e.dispatch_state, e.daemon_instance_id, e.worker_id, e.lease_expires_unix_ms,
       e.heartbeat_unix_ms, coalesce(e.interruption_generation, 0), e.updated_unix_ms,
       (select r.step_name || ' ' || s.step || '/' || r.total_steps from plan_step_run s where s.run_id = r.id order by case s.status when 'running' then 0 when 'starting' then 1 when 'queued' then 2 else 3 end, s.step limit 1),
       (select s.status from plan_step_run s where s.run_id = r.id order by case s.status when 'running' then 0 when 'starting' then 1 when 'queued' then 2 else 3 end, s.step limit 1),
       (select count(*) from plan_step_run s where s.run_id = r.id and s.status in ('done','skipped')),
       r.total_steps
from plan_run r
left join workflow_execution e on e.workflow_kind = 'plan' and e.run_id = r.id
where r.repo_root = ? and r.archived_unix_ms is null
