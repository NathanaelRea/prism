-- Bring the released user_version=1 repository schema to the durable shape released as v2.
-- The caller matches the complete v1 contract before executing this script.

alter table opencode_runtime add column worktree_session_id text;
alter table plan_run add column worktree_session_id text;
alter table auto_run add column worktree_session_id text;

alter table workflow_execution add column execution_version integer not null default 1;
alter table workflow_execution add column not_before_unix_ms integer;
alter table workflow_execution add column wake_reason text;
alter table workflow_execution add column workflow_revision integer not null default 0;
drop index workflow_execution_dispatch_idx;
create index workflow_execution_dispatch_idx
  on workflow_execution(dispatch_state, not_before_unix_ms, created_unix_ms);

create table merge_intent (
  id integer primary key autoincrement,
  run_id text not null references auto_run(id) on delete cascade,
  generation integer not null,
  state text not null,
  placement text not null,
  change_request_identity_json text,
  lane_key text,
  target_branch text,
  pr_number integer,
  head_sha text,
  ready_sequence integer,
  created_unix_ms integer not null,
  updated_unix_ms integer not null,
  unique(run_id, generation)
);
create unique index merge_intent_active_run_idx
  on merge_intent(run_id) where state = 'armed';
create index merge_intent_lane_ready_idx
  on merge_intent(lane_key, state, ready_sequence);

create table integration_lane (
  lane_key text primary key,
  next_ready_sequence integer not null default 1,
  reserved_intent_id integer references merge_intent(id) on delete set null,
  updated_unix_ms integer not null
);

create table worktree_session (
  id text primary key,
  repo_root text not null,
  initial_branch text not null,
  initial_worktree_path text not null,
  created_unix_ms integer not null
) without rowid;

create table active_worktree_session (
  worktree_session_id text primary key references worktree_session(id),
  repo_root text not null,
  branch text not null,
  worktree_path text not null,
  worktree_incarnation text not null,
  observed_unix_ms integer not null,
  unique(repo_root, branch),
  unique(repo_root, worktree_path)
) without rowid;
create index active_worktree_session_location_idx
  on active_worktree_session(repo_root, branch, worktree_path);
