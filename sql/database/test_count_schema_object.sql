select count(*) as "count!: i64"
from sqlite_schema
where name = ?1
