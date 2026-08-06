select
  run_id as "run_id!: String",
  id as "source_event_id!: i64",
  time_unix_ms as "time_unix_ms!: i64",
  kind as "kind!: String",
  data_json as "data_json!: String"
from auto_event
order by run_id, id
