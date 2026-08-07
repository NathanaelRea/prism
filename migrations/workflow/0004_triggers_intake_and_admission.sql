-- Durable Trigger scheduling, provider intake, and evidence-bound admission.
alter table trigger_definition add column trigger_kind text not null default 'manual'
  check (trigger_kind in ('manual','once','interval','cron','startup','provider_poll'));
alter table trigger_definition add column schedule_json text not null default '{}';
alter table trigger_definition add column admission_purpose text not null default 'workflow-launch';
alter table trigger_definition add column created_unix_ms integer not null default 0;
alter table trigger_definition add column updated_unix_ms integer not null default 0;

alter table trigger_occurrence add column observation_revision text;
alter table trigger_occurrence add column provider_item_id text;
alter table trigger_occurrence add column created_unix_ms integer not null default 0;
alter table trigger_occurrence add column completed_unix_ms integer;
alter table trigger_occurrence add column diagnostic text;
alter table trigger_occurrence add column input_json text;

create index trigger_occurrence_history_idx
  on trigger_occurrence(trigger_id, due_unix_ms desc, id);

create table trigger_schedule_checkpoint (
  trigger_id text primary key references trigger_definition(id) on delete cascade,
  last_due_unix_ms integer not null,
  updated_unix_ms integer not null
);

create table provider_item_observation (
  provider_item_id text not null,
  item_kind text not null check (item_kind in ('issue','change_request')),
  observation_revision text not null,
  observation_json text not null,
  trigger_id text not null references trigger_definition(id),
  occurrence_id text references trigger_occurrence(id),
  observed_unix_ms integer not null,
  primary key (provider_item_id, observation_revision)
);
create index provider_item_observation_trigger_idx
  on provider_item_observation(trigger_id, observed_unix_ms, provider_item_id);

create table provider_poll_state (
  trigger_id text primary key references trigger_definition(id) on delete cascade,
  consecutive_failures integer not null check (consecutive_failures > 0),
  retry_after_unix_ms integer not null,
  diagnostic text not null,
  updated_unix_ms integer not null
);

create table admission_decision (
  id text primary key,
  provider_item_id text not null,
  observation_revision text not null,
  purpose text not null,
  outcome text not null check (outcome in ('admitted','rejected')),
  authority_json text not null,
  evidence_json text not null,
  decided_by text not null,
  decided_unix_ms integer not null,
  unique (provider_item_id, observation_revision, purpose),
  foreign key (provider_item_id, observation_revision)
    references provider_item_observation(provider_item_id, observation_revision)
);

create table artifact_provenance (
  artifact_id text primary key references artifact(id) on delete cascade,
  provider_item_id text not null,
  observation_revision text not null,
  trigger_occurrence_id text references trigger_occurrence(id),
  admission_decision_id text not null references admission_decision(id),
  foreign key (provider_item_id, observation_revision)
    references provider_item_observation(provider_item_id, observation_revision)
);

create table implementation_dispatch (
  provider_item_id text not null,
  observation_revision text not null,
  definition_snapshot_id text not null references definition_snapshot(id),
  purpose text not null,
  intake_run_id text not null references workflow_run(id),
  child_run_id text not null unique references workflow_run(id),
  created_unix_ms integer not null,
  primary key (provider_item_id, observation_revision, definition_snapshot_id, purpose)
);
