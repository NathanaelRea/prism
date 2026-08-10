select step.id, step.run_id, step.implementation, step.target_id, step.input_json,
       step.resolved_input_revisions_json as input_revisions_json, run.repository,
       step.timeout_seconds
from workflow_step step
join workflow_run run on run.id = step.run_id
where step.status = 'runnable' and step.available_unix_ms <= ?
  and run.status in ('runnable', 'running')
  and not exists (
    select 1 from step_dependency dependency
    join workflow_step prerequisite on prerequisite.id = dependency.depends_on_step_id
    where dependency.step_id = step.id and prerequisite.status <> 'succeeded'
  )
  and not exists (
    select 1 from step_attempt attempt
    where attempt.step_id = step.id and attempt.status = 'claimed'
  )
order by step.available_unix_ms, step.id
limit ?
