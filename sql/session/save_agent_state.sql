insert into agent_state (branch, state, updated_unix_ms)
values (?, ?, ?)
on conflict(branch) do update set
  state = excluded.state,
  updated_unix_ms = excluded.updated_unix_ms
