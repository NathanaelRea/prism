select id as "id!: String", status, time_finished_unix_ms
from startup_run
order by time_started_unix_ms desc
limit 1
