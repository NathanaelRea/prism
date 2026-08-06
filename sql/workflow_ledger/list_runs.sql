select run.id as "id!"
from workflow_run run
where (? is null or run.repository = ?)
order by run.updated_unix_ms desc, run.id
limit ?
