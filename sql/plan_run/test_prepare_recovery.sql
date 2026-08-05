update plan_run
set status = 'running', pause_requested = 0, updated_unix_ms = 30
where id = 'plan-control-12345678'
