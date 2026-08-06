insert into pr_cache (
  branch, number, provider, canonical_host, project_path, native_cr_id, display_number,
  source_provider, source_canonical_host, source_project_path, target_provider,
  target_canonical_host, target_project_path, title, author, body, url, state,
  review_decision, requested_reviewers, head_ref, base_ref, head_sha, updated_at,
  check_status, merge_state_status, queue_state, comment_count, merged, draft,
  last_refreshed, refreshed_unix_ms, observation_error, native_state_evidence
) values (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
on conflict(branch) do update set
  number = excluded.number, provider = excluded.provider,
  canonical_host = excluded.canonical_host, project_path = excluded.project_path,
  native_cr_id = excluded.native_cr_id, display_number = excluded.display_number,
  source_provider = excluded.source_provider,
  source_canonical_host = excluded.source_canonical_host,
  source_project_path = excluded.source_project_path,
  target_provider = excluded.target_provider,
  target_canonical_host = excluded.target_canonical_host,
  target_project_path = excluded.target_project_path, title = excluded.title,
  author = excluded.author, body = excluded.body, url = excluded.url, state = excluded.state,
  review_decision = excluded.review_decision,
  requested_reviewers = excluded.requested_reviewers, head_ref = excluded.head_ref,
  base_ref = excluded.base_ref, head_sha = excluded.head_sha, updated_at = excluded.updated_at,
  check_status = excluded.check_status, merge_state_status = excluded.merge_state_status,
  queue_state = excluded.queue_state, comment_count = excluded.comment_count,
  merged = excluded.merged, draft = excluded.draft,
  last_refreshed = excluded.last_refreshed, refreshed_unix_ms = excluded.refreshed_unix_ms,
  observation_error = excluded.observation_error,
  native_state_evidence = excluded.native_state_evidence
