-- Durable scheduler, waiting, output, and reconciliation state for the async worker.
create table step_dependency (
  step_id text not null references workflow_step(id) on delete cascade,
  depends_on_step_id text not null references workflow_step(id) on delete cascade,
  primary key (step_id, depends_on_step_id),
  check (step_id <> depends_on_step_id)
);

create table step_resource_requirement (
  step_id text not null references workflow_step(id) on delete cascade,
  resource_key text not null,
  primary key (step_id, resource_key)
);

create table attempt_output (
  attempt_id text not null references step_attempt(id) on delete cascade,
  sequence integer not null check (sequence > 0),
  stream text not null check (stream in ('stdout', 'stderr', 'system')),
  body blob not null,
  time_unix_ms integer not null,
  primary key (attempt_id, sequence)
);
create index attempt_output_time_idx on attempt_output(attempt_id, time_unix_ms);

create table gate_wait (
  step_id text primary key references workflow_step(id) on delete cascade,
  gate_kind text not null,
  due_unix_ms integer not null,
  checkpoint_json text not null,
  poll_count integer not null default 0 check (poll_count >= 0)
);
create index gate_wait_due_idx on gate_wait(due_unix_ms, step_id);

create table workflow_deadline (
  id text primary key,
  run_id text not null references workflow_run(id) on delete cascade,
  step_id text references workflow_step(id) on delete cascade,
  due_unix_ms integer not null,
  kind text not null,
  state text not null check (state in ('pending', 'fired', 'cancelled')),
  unique (run_id, step_id, kind)
);
create index workflow_deadline_due_idx on workflow_deadline(state, due_unix_ms);

create table worker_checkpoint (
  worker_id text primary key,
  instance_id text not null,
  state text not null check (state in ('running', 'draining', 'failed', 'stopped')),
  diagnostic text,
  updated_unix_ms integer not null
);

create table capacity_policy (
  scope text not null,
  capacity_key text not null,
  maximum integer not null check (maximum > 0),
  primary key (scope, capacity_key)
);

create table capacity_claim (
  scope text not null,
  capacity_key text not null,
  slot integer not null check (slot > 0),
  attempt_id text not null references step_attempt(id) on delete cascade,
  fencing_token integer not null,
  acquired_unix_ms integer not null,
  primary key (scope, capacity_key, slot),
  unique (attempt_id, scope, capacity_key)
);

create table trigger_wakeup (
  trigger_id text primary key references trigger_definition(id) on delete cascade,
  next_due_unix_ms integer not null,
  lease_owner text,
  lease_expires_unix_ms integer,
  fencing_token integer not null default 0
);
create index trigger_wakeup_due_idx on trigger_wakeup(next_due_unix_ms, trigger_id);

create table control_plane_metric (
  id integer primary key autoincrement,
  name text not null,
  value integer not null,
  labels_json text not null,
  time_unix_ms integer not null
);
create index control_plane_metric_time_idx on control_plane_metric(time_unix_ms, name);
