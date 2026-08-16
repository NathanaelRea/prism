CREATE TABLE deferred_merge_cleanup (
  branch text primary key,
  worktree_path text not null,
  worktree_incarnation text not null,
  branch_oid text not null,
  warnings_json text not null,
  updated_unix_ms integer not null
);
