select
  'plan' as "kind!: String",
  run_id as "run_id!: String",
  cast(step as text) as "source_step_id!: String",
  'phase-' || step as "step_key!: String",
  status as "status!: String",
  coalesce(started_unix_ms, 0) as "available_unix_ms!: i64",
  json_object(
    'prompt', prompt,
    'summary', summary,
    'error', error,
    'exit_code', exit_code
  ) as "input_json!: String"
from plan_step_run
union all
select
  'auto' as "kind!: String",
  run_id as "run_id!: String",
  cast(id as text) as "source_step_id!: String",
  step_key || '-attempt-' || attempt as "step_key!: String",
  status as "status!: String",
  coalesce(started_unix_ms, 0) as "available_unix_ms!: i64",
  json_object(
    'reason', reason,
    'summary', summary,
    'error', error,
    'commit_sha', commit_sha,
    'head_sha', head_sha
  ) as "input_json!: String"
from auto_step_run
order by 1, 2, 3
