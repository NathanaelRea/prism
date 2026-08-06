insert into task_metadata (
  branch, prompt_summary, initial_prompt, worktree, classification, visibility, updated_unix_ms
) values (?, ?, '', ?, ?, ?, ?)
on conflict(branch) do update set
  worktree = excluded.worktree,
  classification = excluded.classification,
  visibility = excluded.visibility,
  updated_unix_ms = excluded.updated_unix_ms
