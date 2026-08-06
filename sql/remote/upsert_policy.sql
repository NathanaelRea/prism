insert into repo_policy_cache (
  provider, canonical_host, project_path, project_path_key, target_branch, default_branch,
  required_approvals, require_conversation_resolution, require_branch_up_to_date,
  required_checks, merge_queue_required, refreshed_unix_ms, error
) values (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
on conflict(provider, canonical_host, project_path_key, target_branch) do update set
  project_path = excluded.project_path, default_branch = excluded.default_branch,
  required_approvals = excluded.required_approvals,
  require_conversation_resolution = excluded.require_conversation_resolution,
  require_branch_up_to_date = excluded.require_branch_up_to_date,
  required_checks = excluded.required_checks,
  merge_queue_required = excluded.merge_queue_required,
  refreshed_unix_ms = excluded.refreshed_unix_ms, error = excluded.error
