insert into auto_event (run_id, step_run_id, time_unix_ms, kind, data_json)
values (?, ?, ?, ?, ?)
returning id as "id!"
