insert into pr_details_cache (
  branch, pr_number, head_sha, provider, canonical_host, project_path, native_cr_id,
  display_number, source_provider, source_canonical_host, source_project_path,
  target_provider, target_canonical_host, target_project_path, comments, reviews,
  review_comments, files, failing_checks, check_contexts, ci_failures,
  refreshed_unix_ms, observation_error
) values (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
on conflict(branch) do update set
  pr_number = excluded.pr_number, head_sha = excluded.head_sha, provider = excluded.provider,
  canonical_host = excluded.canonical_host, project_path = excluded.project_path,
  native_cr_id = excluded.native_cr_id, display_number = excluded.display_number,
  source_provider = excluded.source_provider,
  source_canonical_host = excluded.source_canonical_host,
  source_project_path = excluded.source_project_path,
  target_provider = excluded.target_provider,
  target_canonical_host = excluded.target_canonical_host,
  target_project_path = excluded.target_project_path, comments = excluded.comments,
  reviews = excluded.reviews, review_comments = excluded.review_comments,
  files = excluded.files, failing_checks = excluded.failing_checks,
  check_contexts = excluded.check_contexts, ci_failures = excluded.ci_failures,
  refreshed_unix_ms = excluded.refreshed_unix_ms,
  observation_error = excluded.observation_error
