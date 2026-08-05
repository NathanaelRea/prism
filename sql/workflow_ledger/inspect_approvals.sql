select
  id as "id!: String",
  step_id,
  status as "status!: String",
  requested_unix_ms as "requested_unix_ms!: i64",
  decided_unix_ms,
  decided_by,
  decision_note
from approval_request
where run_id = ?
order by requested_unix_ms, id
