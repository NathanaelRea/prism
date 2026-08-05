select count(*) as "count!: i64"
from auto_event
where run_id = ? and kind = ?
