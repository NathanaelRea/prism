create table if not exists remote_mutation_ledger (
  canonical_host text not null,
  credential_profile text not null,
  request_id text not null,
  request_fingerprint text not null,
  state text not null check(state in ('claimed','uncertain','applied','failed')),
  outcome_json text,
  reason text,
  updated_unix_ms integer not null,
  primary key(canonical_host, credential_profile, request_id),
  check(
    (state = 'applied' and outcome_json is not null and reason is null) or
    (state in ('uncertain','failed') and outcome_json is null and reason is not null) or
    (state = 'claimed' and outcome_json is null and reason is null)
  )
);
