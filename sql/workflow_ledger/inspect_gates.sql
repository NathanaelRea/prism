select
  gate.step_id as "step_id!: String",
  gate.gate_kind as "gate_kind!: String",
  gate.due_unix_ms as "due_unix_ms!: i64",
  gate.checkpoint_json as "checkpoint_json!: String",
  gate.poll_count as "poll_count!: i64"
from gate_wait gate
join workflow_step step on step.id = gate.step_id
where step.run_id = ?
order by gate.due_unix_ms, gate.step_id
