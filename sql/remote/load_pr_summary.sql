select branch as "branch!", number as "number!", provider as "provider!",
       canonical_host as "canonical_host!", project_path as "project_path!",
       native_cr_id as "native_cr_id!", display_number as "display_number!",
       source_provider as "source_provider!",
       source_canonical_host as "source_canonical_host!",
       source_project_path as "source_project_path!",
       target_provider as "target_provider!",
       target_canonical_host as "target_canonical_host!",
       target_project_path as "target_project_path!", title as "title!",
       author as "author!", body as "body!", url as "url!", state as "state!",
       review_decision as "review_decision!",
       requested_reviewers as "requested_reviewers!", head_ref as "head_ref!",
       base_ref as "base_ref!", head_sha as "head_sha!", updated_at as "updated_at!",
       check_status as "check_status!", merge_state_status as "merge_state_status!",
       queue_state as "queue_state!", comment_count as "comment_count!",
       merged as "merged!", draft as "draft!", last_refreshed as "last_refreshed!",
       observation_error, native_state_evidence as "native_state_evidence!"
from pr_cache
where branch = ?
