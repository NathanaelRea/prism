select
  id as "id!: String",
  producing_attempt_id,
  revision as "revision!: i64",
  digest as "digest!: String",
  size_bytes as "size_bytes!: i64",
  sensitivity as "sensitivity!: String",
  inline_body,
  file_path,
  created_unix_ms as "created_unix_ms!: i64"
from artifact
where run_id = ?
order by id, revision
