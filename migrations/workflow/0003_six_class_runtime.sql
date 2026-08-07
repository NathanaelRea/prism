-- Canonical six-class runtime state. Definition snapshots remain the source of truth;
-- these columns are the persisted execution projection used by the coordinator.
alter table workflow_run add column input_json text not null default '{}';
alter table workflow_run add column parent_run_id text references workflow_run(id);
alter table workflow_run add column parent_step_id text references workflow_step(id);
alter table workflow_run add column lineage_root_id text;
alter table workflow_run add column detached integer not null default 0 check (detached in (0, 1));
alter table workflow_run add column attempt_budget integer;
alter table workflow_run add column attempts_consumed integer not null default 0 check (attempts_consumed >= 0);
alter table workflow_run add column archived_unix_ms integer;
alter table workflow_run add column runtime_status text not null default 'runnable';

alter table workflow_step add column class text not null default 'action'
  check (class in ('action','gate','approval','wait','notification','workflow_call'));
alter table workflow_step add column bindings_json text not null default '{}';
alter table workflow_step add column outputs_json text not null default '{}';
alter table workflow_step add column settings_json text not null default '{}';
alter table workflow_step add column condition_json text;
alter table workflow_step add column on_unknown text not null default 'wait'
  check (on_unknown in ('wait','skip','fail'));
alter table workflow_step add column skippable integer not null default 0 check (skippable in (0, 1));
alter table workflow_step add column retry_max_attempts integer not null default 0 check (retry_max_attempts >= 0);
alter table workflow_step add column child_snapshot_id text references definition_snapshot(id);
alter table workflow_step add column invalidated_unix_ms integer;
alter table workflow_step add column runtime_status text not null default 'waiting';
alter table workflow_step add column repeat_json text;
alter table workflow_step add column resolved_input_revisions_json text not null default '{}';
alter table workflow_step add column effect_boundary text not null default 'none'
  check (effect_boundary in ('none','brokered','unbrokered'));

alter table step_attempt add column input_revisions_json text not null default '{}';

create table attempt_output_binding (
  attempt_id text not null references step_attempt(id) on delete cascade,
  name text not null,
  schema_id text not null,
  value_json text not null,
  artifact_id text references artifact(id),
  primary key (attempt_id, name)
);

create table approval_evidence (
  approval_id text primary key references approval_request(id) on delete cascade,
  subject_json text not null,
  evidence_json text not null,
  policy_json text not null
);

create table gate_observation (
  attempt_id text primary key references step_attempt(id) on delete cascade,
  step_id text not null references workflow_step(id) on delete cascade,
  subject_json text not null,
  evidence_json text not null,
  policy_json text not null,
  observed_unix_ms integer not null
);

create table child_run_link (
  parent_run_id text not null references workflow_run(id) on delete cascade,
  parent_step_id text not null references workflow_step(id) on delete cascade,
  iteration integer not null check (iteration > 0),
  child_run_id text not null unique references workflow_run(id),
  primary key (parent_step_id, iteration)
);

create table step_output_binding (
  step_id text not null references workflow_step(id) on delete cascade,
  name text not null,
  schema_id text not null,
  value_json text not null,
  source_run_id text references workflow_run(id),
  artifact_id text references artifact(id),
  primary key (step_id, name)
);

create table workflow_input_binding (
  run_id text not null references workflow_run(id) on delete cascade,
  name text not null,
  schema_id text not null,
  artifact_id text not null references artifact(id),
  revision integer not null,
  primary key (run_id, name)
);

create index workflow_run_parent_idx on workflow_run(parent_run_id, created_unix_ms);
create index workflow_step_class_status_idx on workflow_step(class, status, available_unix_ms);

drop table import_journal;
