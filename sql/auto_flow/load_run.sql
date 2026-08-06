select
  id as "id!", harness_id as "harness_id!", adapter_id as "adapter_id!",
  repo_root as "repo_root!", worktree_path as "worktree_path!",
  worktree_incarnation, branch as "branch!", mode as "mode!",
  implementation_source as "implementation_source!", plan_path,
  plan_run_mode as "plan_run_mode!", variant as "variant!", agent_profile,
  prompt_summary as "prompt_summary!", initial_prompt as "initial_prompt!",
  status as "status!", pause_requested as "pause_requested!",
  selected_step_run_id, pr_number, pr_url, current_head_sha,
  review_baseline_json, stabilization_status, stabilization_blocker,
  stabilization_next_work, pending_push_json,
  created_unix_ms as "created_unix_ms!", updated_unix_ms as "updated_unix_ms!",
  archived_unix_ms
from auto_run
where id = ?
