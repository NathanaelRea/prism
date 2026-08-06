select branch as "branch!", pr_number as "pr_number!", head_sha as "head_sha!",
       provider as "provider!", canonical_host as "canonical_host!",
       project_path as "project_path!", native_cr_id as "native_cr_id!",
       display_number as "display_number!", source_provider as "source_provider!",
       source_canonical_host as "source_canonical_host!",
       source_project_path as "source_project_path!",
       target_provider as "target_provider!",
       target_canonical_host as "target_canonical_host!",
       target_project_path as "target_project_path!", comments as "comments!",
       reviews as "reviews!", review_comments as "review_comments!", files as "files!",
       failing_checks as "failing_checks!", check_contexts as "check_contexts!",
       ci_failures as "ci_failures!", observation_error
from pr_details_cache
where branch = ?
