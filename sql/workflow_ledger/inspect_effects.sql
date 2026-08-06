select
  id as "id!: String",
  attempt_id as "attempt_id!: String",
  effect_kind as "effect_kind!: String",
  idempotency_key as "idempotency_key!: String",
  status as "status!: String",
  request_json as "request_json!: String",
  result_json,
  created_unix_ms as "created_unix_ms!: i64",
  updated_unix_ms as "updated_unix_ms!: i64"
from effect_intent
where run_id = ?
order by created_unix_ms, id
