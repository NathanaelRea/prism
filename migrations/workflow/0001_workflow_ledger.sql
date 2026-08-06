-- Global workflow control-plane schema. Repository-local caches intentionally do not live here.
create table workflow_database_identity (
  singleton integer primary key check (singleton = 1),
  kind text not null check (kind = 'workflow'),
  schema_epoch integer not null
);
insert into workflow_database_identity values (1, 'workflow', 1);

create table definition_snapshot (
  id text primary key,
  definition_name text not null,
  revision text not null,
  source text not null,
  trusted integer not null check (trusted in (0, 1)),
  body_json text not null,
  digest text not null,
  created_unix_ms integer not null,
  unique (definition_name, revision, digest)
);

create table workflow_run (
  id text primary key,
  definition_snapshot_id text not null references definition_snapshot(id),
  repository text,
  status text not null check (status in ('waiting','runnable','running','paused','succeeded','failed','cancelled','recovery_required')),
  created_unix_ms integer not null,
  updated_unix_ms integer not null,
  completed_unix_ms integer
);
create index workflow_run_status_idx on workflow_run(status, updated_unix_ms);

create table workflow_step (
  id text primary key,
  run_id text not null references workflow_run(id) on delete cascade,
  step_key text not null,
  implementation text not null,
  target_id text not null,
  status text not null check (status in ('waiting','runnable','claimed','succeeded','failed','cancelled')),
  available_unix_ms integer not null,
  input_json text not null,
  unique (run_id, step_key)
);
create index workflow_step_runnable_idx on workflow_step(status, available_unix_ms, id);

create table step_attempt (
  id text primary key,
  step_id text not null references workflow_step(id) on delete cascade,
  attempt_number integer not null check (attempt_number > 0),
  status text not null check (status in ('claimed','succeeded','failed','cancelled','expired','recovery_required')),
  worker_id text not null,
  target_id text not null,
  fencing_token integer not null check (fencing_token > 0),
  lease_expires_unix_ms integer not null,
  process_id integer,
  process_start_time_ticks integer,
  started_unix_ms integer not null,
  finished_unix_ms integer,
  result_json text,
  unique (step_id, attempt_number),
  unique (step_id, fencing_token)
);
create index step_attempt_lease_idx on step_attempt(status, lease_expires_unix_ms);

create table resource_claim (
  resource_key text not null,
  attempt_id text not null references step_attempt(id) on delete cascade,
  fencing_token integer not null,
  acquired_unix_ms integer not null,
  primary key (resource_key),
  unique (attempt_id, resource_key)
);

create table audit_event (
  id integer primary key autoincrement,
  run_id text not null references workflow_run(id) on delete cascade,
  step_id text references workflow_step(id) on delete cascade,
  attempt_id text references step_attempt(id) on delete cascade,
  sequence integer not null,
  kind text not null,
  time_unix_ms integer not null,
  data_json text not null,
  unique (run_id, sequence)
);

create table artifact (
  id text primary key,
  run_id text not null references workflow_run(id),
  producing_attempt_id text references step_attempt(id),
  revision integer not null check (revision > 0),
  digest text not null,
  size_bytes integer not null check (size_bytes >= 0),
  sensitivity text not null,
  inline_body blob,
  file_path text,
  created_unix_ms integer not null,
  check ((inline_body is null) != (file_path is null))
);
create table artifact_lineage (
  artifact_id text not null references artifact(id) on delete cascade,
  parent_artifact_id text not null references artifact(id),
  primary key (artifact_id, parent_artifact_id)
);

create table approval_request (
  id text primary key,
  run_id text not null references workflow_run(id),
  step_id text references workflow_step(id),
  status text not null check (status in ('pending','approved','rejected','expired')),
  requested_unix_ms integer not null,
  decided_unix_ms integer,
  decided_by text,
  decision_note text
);
create table authority_grant (
  id text primary key,
  run_id text not null references workflow_run(id),
  scope text not null,
  granted_by text not null,
  granted_unix_ms integer not null,
  expires_unix_ms integer
);

create table effect_intent (
  id text primary key,
  run_id text not null references workflow_run(id),
  attempt_id text not null references step_attempt(id),
  fencing_token integer not null,
  effect_kind text not null,
  idempotency_key text not null unique,
  status text not null check (status in ('prepared','dispatching','succeeded','failed','indeterminate')),
  request_json text not null,
  result_json text,
  created_unix_ms integer not null,
  updated_unix_ms integer not null
);

create table trigger_definition (
  id text primary key,
  definition_snapshot_id text not null references definition_snapshot(id),
  overlap_policy text not null,
  config_json text not null,
  enabled integer not null check (enabled in (0,1))
);
create table trigger_occurrence (
  id text primary key,
  trigger_id text not null references trigger_definition(id),
  deduplication_key text not null,
  due_unix_ms integer not null,
  status text not null,
  run_id text references workflow_run(id),
  unique (trigger_id, deduplication_key)
);
create table trigger_checkpoint (
  trigger_id text primary key references trigger_definition(id),
  checkpoint_json text not null,
  updated_unix_ms integer not null
);

create table execution_workspace (
  id text primary key,
  run_id text not null references workflow_run(id),
  repository text not null,
  path text not null,
  state text not null check (state in ('active','released','quarantined')),
  quarantine_reason text,
  updated_unix_ms integer not null
);
create table idempotency_record (
  scope text not null,
  key text not null,
  result_kind text not null,
  result_id text not null,
  created_unix_ms integer not null,
  primary key (scope, key)
);
create table import_journal (
  source_database_identity text not null,
  source_schema_version integer not null,
  legacy_run_identity text not null,
  importer_revision text not null,
  status text not null,
  imported_run_id text references workflow_run(id),
  updated_unix_ms integer not null,
  primary key (source_database_identity, source_schema_version, legacy_run_identity, importer_revision)
);
