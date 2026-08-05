select
  type as "kind!: String",
  name as "name!: String",
  tbl_name as "table_name!: String",
  coalesce(sql, '') as "sql!: String"
from sqlite_schema
where name not like 'sqlite_%' and name != '_sqlx_migrations'
order by type, name, tbl_name
