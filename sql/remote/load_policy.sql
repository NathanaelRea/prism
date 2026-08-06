select provider as "provider!", canonical_host as "canonical_host!",
       project_path as "project_path!", project_path_key as "project_path_key!",
       target_branch as "target_branch!", default_branch, required_approvals as "required_approvals!",
       require_conversation_resolution as "require_conversation_resolution!",
       require_branch_up_to_date as "require_branch_up_to_date!",
       required_checks as "required_checks!", merge_queue_required as "merge_queue_required!",
       refreshed_unix_ms as "refreshed_unix_ms!", error
from repo_policy_cache
where provider = ? and canonical_host = ? and project_path_key = ? and target_branch = ?
