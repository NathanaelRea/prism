
create table if not exists workflow_database_identity (
  singleton integer primary key check(singleton=1),
  kind text not null check(kind='workflow'),
  schema_epoch integer not null check(schema_epoch=4)
);
insert into workflow_database_identity(singleton, kind, schema_epoch)
values(1, 'workflow', 4)
on conflict(singleton) do update set kind=excluded.kind, schema_epoch=excluded.schema_epoch;

create table if not exists workflow_snapshot (
  digest text primary key,
  workflow_name text not null,
  source_path text not null,
  source_revision text not null,
  source text not null,
  body_json text not null,
  created_unix_ms integer not null
);

create table if not exists workflow_run (
  id text primary key,
  workflow_digest text not null references workflow_snapshot(digest),
  workflow_name text not null,
  repository text not null,
  worktree text not null,
  change_request text,
  change_request_head text,
  status text not null check(status in ('queued','running','waiting','needs_input','paused','succeeded','failed','cancelled','recovery_required')),
  cycle integer not null check(cycle > 0),
  max_agent_runs integer not null check(max_agent_runs > 0),
  agent_runs_consumed integer not null check(agent_runs_consumed >= 0),
  cancellation_requested integer not null check(cancellation_requested in (0,1)),
  created_unix_ms integer not null,
  updated_unix_ms integer not null,
  revision integer not null check(revision >= 0),
  state_json text not null
);
create index if not exists workflow_run_status_idx on workflow_run(status, updated_unix_ms);

create table if not exists workflow_step (
  run_id text not null references workflow_run(id) on delete cascade,
  step_index integer not null,
  step_key text not null,
  trigger_name text,
  phase text not null,
  summary text,
  wake_at_unix_ms integer,
  satisfied_cycle integer,
  unconditional_completed integer not null check(unconditional_completed in (0,1)),
  primary key(run_id, step_index),
  unique(run_id, step_key)
);
create index if not exists workflow_step_wake_idx on workflow_step(wake_at_unix_ms) where wake_at_unix_ms is not null;

create table if not exists workflow_dependency (
  run_id text not null,
  step_index integer not null,
  dependency_key text not null,
  primary key(run_id, step_index, dependency_key),
  foreign key(run_id, step_index) references workflow_step(run_id, step_index) on delete cascade
);

create table if not exists step_lifecycle_attempt (
  id text primary key,
  run_id text not null,
  step_index integer not null,
  attempt_number integer not null check(attempt_number > 0),
  status text not null,
  phase text not null,
  prepared_state_json text,
  agent_status text,
  agent_process_id integer,
  agent_session_id text,
  agent_final_text text,
  agent_turn_in_flight integer check(agent_turn_in_flight > 0),
  error text,
  started_unix_ms integer not null,
  finished_unix_ms integer,
  fencing_token integer not null check(fencing_token > 0),
  phase_owner text,
  lease_expires_unix_ms integer,
  foreign key(run_id, step_index) references workflow_step(run_id, step_index) on delete cascade,
  unique(run_id, step_index, attempt_number)
);

create table if not exists agent_turn (
  attempt_id text not null references step_lifecycle_attempt(id) on delete cascade,
  turn_number integer not null check(turn_number > 0),
  process_id integer,
  session_id text not null,
  final_text text not null,
  primary key(attempt_id, turn_number)
);

create table if not exists workflow_run_event (
  run_id text not null references workflow_run(id) on delete cascade,
  sequence integer not null,
  time_unix_ms integer not null,
  step_key text,
  attempt_id text,
  kind text not null,
  summary text not null,
  primary key(run_id, sequence)
);

create table if not exists trigger_executable_snapshot (
  digest text primary key,
  trigger_name text not null,
  executable_path text not null,
  retained_path text not null,
  created_unix_ms integer not null
);

create table if not exists remote_lane_cooldown (
  canonical_host text not null,
  credential_profile text not null,
  next_request_unix_ms integer not null,
  retry_count integer not null,
  updated_unix_ms integer not null,
  primary key(canonical_host, credential_profile)
);

create table if not exists remote_observation_subscription (
  observation_key text not null,
  subscriber_id text not null,
  created_unix_ms integer not null,
  primary key(observation_key, subscriber_id)
);
