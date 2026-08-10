-- Fresh-install repository schema.

CREATE TABLE active_worktree_session (
  worktree_session_id text primary key references worktree_session(id),
  repo_root text not null,
  branch text not null,
  worktree_path text not null,
  worktree_incarnation text not null,
  observed_unix_ms integer not null,
  unique(repo_root, branch),
  unique(repo_root, worktree_path)
) without rowid;

CREATE TABLE agent_state (
  branch text primary key,
  state text not null,
  updated_unix_ms integer not null
);

CREATE TABLE archived_worktree (
  branch text primary key,
  repo_root text not null,
  worktree_path text not null,
  archived_unix_ms integer not null,
  classification text not null default 'work'
);

CREATE TABLE event (
  id integer primary key autoincrement,
  time_unix_ms integer not null,
  level text not null,
  target text not null,
  action text not null,
  operation_id text,
  parent_operation_id text,
  repo text,
  branch text,
  session text,
  message text not null,
  data_json text
);

CREATE TABLE hidden_session (
  branch text primary key,
  hidden_unix_ms integer not null
);

CREATE TABLE metadata (
  key text primary key,
  value text not null
);

CREATE TABLE notification_outbox (
  id integer primary key autoincrement,
  worktree_path text not null,
  branch text not null,
  incarnation text not null,
  transition_sequence integer not null,
  kind text not null,
  title text not null,
  body text not null,
  observed_unix_ms integer not null,
  expires_unix_ms integer not null,
  delivery_state text not null,
  attempt_count integer not null default 0,
  available_unix_ms integer not null,
  attempted_unix_ms integer,
  backend_accepted_unix_ms integer,
  superseded_unix_ms integer,
  last_failure_category text,
  unique (worktree_path, branch, incarnation, transition_sequence)
);

CREATE TABLE notification_session (
  worktree_path text not null,
  branch text not null,
  incarnation text not null,
  state text not null,
  transition_sequence integer not null,
  observed_unix_ms integer not null,
  primary key (worktree_path, branch, incarnation)
);

CREATE TABLE opencode_runtime (
  repo_root text not null,
  harness_id text not null default 'opencode',
  branch text not null,
  worktree_path text not null,
  server_port integer not null,
  server_url text not null,
  server_pid integer,
  opencode_session_id text,
  generation integer not null,
  updated_unix_ms integer not null,
  server_start_time_ticks integer,
  worktree_session_id text,
  primary key (repo_root, harness_id, branch, worktree_path)
);

CREATE TABLE pending_worktree_deletion (
  branch text primary key,
  worktree_path text not null,
  worktree_incarnation text not null,
  branch_oid text,
  worktree_removed integer not null default 0,
  branch_deleted integer not null default 0,
  updated_unix_ms integer not null
);

CREATE TABLE pr_cache (
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

CREATE TABLE pr_details_cache (
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

CREATE TABLE repo_policy_cache (
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

CREATE TABLE startup_phase (
  id integer primary key autoincrement,
  run_id text not null references startup_run(id) on delete cascade,
  phase text not null,
  time_started_unix_ms integer not null,
  time_finished_unix_ms integer,
  status text not null,
  error text
);

CREATE TABLE startup_run (
  id text primary key,
  time_started_unix_ms integer not null,
  time_finished_unix_ms integer,
  status text not null,
  repo text,
  version text not null,
  error text
);

CREATE TABLE task_metadata (
  branch text primary key,
  prompt_summary text not null,
  initial_prompt text not null,
  worktree text not null,
  classification text not null default 'work',
  visibility integer not null default 0,
  updated_unix_ms integer not null
);

CREATE TABLE worktree_harness (
  branch text primary key,
  worktree_path text not null,
  worktree_incarnation text not null,
  harness_id text not null,
  migration_policy text not null default 'ask',
  updated_unix_ms integer not null
);

CREATE TABLE worktree_session (
  id text primary key,
  repo_root text not null,
  initial_branch text not null,
  initial_worktree_path text not null,
  created_unix_ms integer not null
) without rowid;

CREATE INDEX active_worktree_session_location_idx
  on active_worktree_session(repo_root, branch, worktree_path);

CREATE INDEX event_action_idx on event(action);

CREATE INDEX event_branch_idx on event(branch);

CREATE INDEX event_operation_idx on event(operation_id);

CREATE INDEX event_target_idx on event(target);

CREATE INDEX event_time_idx on event(time_unix_ms);

CREATE INDEX notification_outbox_delivery_idx
  on notification_outbox(delivery_state, expires_unix_ms, id);

CREATE INDEX opencode_runtime_branch_idx
  on opencode_runtime(repo_root, harness_id, branch);
