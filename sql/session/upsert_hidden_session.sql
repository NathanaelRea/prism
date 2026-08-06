insert into hidden_session (branch, hidden_unix_ms)
values (?, ?)
on conflict(branch) do update set hidden_unix_ms = excluded.hidden_unix_ms
