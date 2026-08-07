select count(*) from sqlite_master
where name in ('auto_run', 'auto_step_run', 'plan_run', 'plan_step_run', 'workflow_execution')
