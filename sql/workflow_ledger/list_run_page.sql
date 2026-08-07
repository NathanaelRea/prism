select run.id as id, snapshot.definition_name, run.runtime_status as status, run.repository,
       run.created_unix_ms, run.updated_unix_ms, run.completed_unix_ms,
       run.parent_run_id, run.lineage_root_id, run.archived_unix_ms,
       run.detached, run.attempt_budget, run.attempts_consumed
from workflow_run run
join definition_snapshot snapshot on snapshot.id = run.definition_snapshot_id
where (? is null or run.repository = ?)
order by run.updated_unix_ms desc, run.id
limit ? offset ?
