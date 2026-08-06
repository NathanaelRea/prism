select
  sequence,
  step_id,
  attempt_id,
  kind,
  time_unix_ms,
  data_json
from audit_event
where run_id = ?
order by sequence
