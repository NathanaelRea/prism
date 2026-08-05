select distinct s.plan_run_id as "plan_run_id!", s.run_id as "auto_run_id!"
from auto_step_run s
join auto_run r on r.id = s.run_id
where s.plan_run_id is not null and r.archived_unix_ms is null
order by s.plan_run_id, s.run_id
