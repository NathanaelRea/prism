select run.id as id, snapshot.definition_name, run.runtime_status as status, run.repository,
       run.created_unix_ms, run.updated_unix_ms, run.completed_unix_ms
from workflow_run run
join definition_snapshot snapshot on snapshot.id = run.definition_snapshot_id
where run.id = ?
