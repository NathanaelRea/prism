select
  id as "id!: String",
  step_key as "key!: String",
  implementation as "implementation!: String",
  target_id as "target_id!: String",
  status as "status!: String",
  input_json as "input_json!: String"
from workflow_step
where run_id = ?
order by id
