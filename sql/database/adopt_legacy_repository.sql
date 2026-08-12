-- Normalize rebuildable provider caches after a complete released v2 contract match, then remove
-- generalized Workflow execution state. Durable repository, Worktree Session, and notification
-- state remains in place.
drop table auto_schema_version;
drop index repo_policy_cache_v2_identity_key;

alter table pr_details_cache rename to historical_pr_details_cache;
alter table pr_cache rename to historical_pr_cache;
alter table repo_policy_cache rename to historical_repo_policy_cache;
alter table repo_policy_cache_v2 rename to historical_repo_policy_cache_v2;

create table pr_cache (
  branch text primary key,
  number integer not null,
  provider text not null,
  canonical_host text not null,
  project_path text not null,
  native_cr_id text not null,
  display_number integer not null,
  source_provider text not null,
  source_canonical_host text not null,
  source_project_path text not null,
  target_provider text not null,
  target_canonical_host text not null,
  target_project_path text not null,
  title text not null,
  author text not null default '',
  body text not null default '',
  url text not null,
  state text not null,
  review_decision text not null,
  requested_reviewers text not null default '',
  head_ref text not null,
  base_ref text not null,
  head_sha text not null,
  updated_at text not null,
  check_status text not null,
  merge_state_status text not null default '',
  queue_state text not null default '',
  comment_count integer not null default 0,
  merged integer not null,
  draft integer not null,
  last_refreshed text not null,
  refreshed_unix_ms integer not null,
  observation_error text,
  native_state_evidence text not null default '{}'
);
insert into pr_cache
select branch, number, provider, canonical_host, project_path, native_cr_id, display_number,
       source_provider, source_canonical_host, source_project_path, target_provider,
       target_canonical_host, target_project_path, title, author, body, url, state,
       review_decision,
       case when json_valid(requested_reviewers) and json_type(requested_reviewers) = 'array'
            then requested_reviewers else '[]' end,
       head_ref, base_ref, head_sha, updated_at,
       check_status, merge_state_status, queue_state, comment_count, merged, draft,
       last_refreshed, refreshed_unix_ms, observation_error, native_state_evidence
from historical_pr_cache
where identity_complete = 1;

create table pr_details_cache (
  branch text primary key,
  pr_number integer not null,
  head_sha text not null,
  provider text not null,
  canonical_host text not null,
  project_path text not null,
  native_cr_id text not null,
  display_number integer not null,
  source_provider text not null,
  source_canonical_host text not null,
  source_project_path text not null,
  target_provider text not null,
  target_canonical_host text not null,
  target_project_path text not null,
  comments text not null,
  reviews text not null,
  review_comments text not null,
  files text not null,
  failing_checks text not null,
  check_contexts text not null default '[]',
  ci_failures text not null default '[]',
  refreshed_unix_ms integer not null,
  observation_error text,
  foreign key (branch) references pr_cache(branch) on delete cascade
);
insert into pr_details_cache
select branch, pr_number, head_sha, provider, canonical_host, project_path, native_cr_id,
       display_number, source_provider, source_canonical_host, source_project_path,
       target_provider, target_canonical_host, target_project_path, comments, reviews,
       review_comments, files, failing_checks, check_contexts, ci_failures, refreshed_unix_ms,
       observation_error
from historical_pr_details_cache
where identity_complete = 1
  and pr_number is not null
  and head_sha is not null
  and exists (select 1 from pr_cache where pr_cache.branch = historical_pr_details_cache.branch);

create table repo_policy_cache (
  provider text not null,
  canonical_host text not null,
  project_path text not null,
  project_path_key text not null,
  target_branch text not null,
  default_branch text,
  required_approvals integer not null default 0,
  require_conversation_resolution integer not null default 0,
  require_branch_up_to_date integer not null default 0,
  required_checks text not null default '[]',
  merge_queue_required integer not null default 0,
  refreshed_unix_ms integer not null,
  error text,
  primary key (provider, canonical_host, project_path_key, target_branch)
);
insert into repo_policy_cache
select provider, canonical_host, project_path, project_path_key, target_branch, default_branch,
       required_approvals, require_conversation_resolution, require_branch_up_to_date,
       required_checks, merge_queue_required, refreshed_unix_ms, error
from historical_repo_policy_cache_v2;

drop table historical_pr_details_cache;
drop table historical_pr_cache;
drop table historical_repo_policy_cache;
drop table historical_repo_policy_cache_v2;

update auto_run set selected_step_run_id = null;
update auto_step_run set plan_run_id = null;
drop table auto_output_line;
drop table auto_event;
drop table auto_step_run;
drop table auto_run;
drop table plan_output_line;
drop table plan_step_run;
drop table plan_run;
drop table workflow_execution;
drop table integration_lane;
drop table merge_intent;
