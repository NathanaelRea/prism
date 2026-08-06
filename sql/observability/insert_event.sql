insert into event (
  time_unix_ms, level, target, action, operation_id, parent_operation_id,
  repo, branch, session, message, data_json
) values (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
