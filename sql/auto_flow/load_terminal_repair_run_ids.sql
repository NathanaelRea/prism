select run.id as "id!"
from auto_run run
where run.repo_root = ?
  and run.archived_unix_ms is null
  and run.variant = 'repair'
  and run.status in ('done', 'aborted')
  and not exists (
    select 1 from auto_run newer
    where newer.repo_root = run.repo_root
      and newer.worktree_path = run.worktree_path
      and newer.archived_unix_ms is null
      and newer.variant = 'repair'
      and newer.status in ('done', 'aborted')
      and newer.updated_unix_ms > run.updated_unix_ms
  )
order by run.updated_unix_ms desc
