select
  version as "version!: i64",
  description as "description!: String",
  success as "success!: bool",
  hex(checksum) as "checksum!: String"
from _sqlx_migrations
order by version
