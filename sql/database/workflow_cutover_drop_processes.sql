select execution_process_id as process_id, execution_process_start_time_ticks as process_identity
from plan_step_run where execution_process_id is not null
union
select process_id, null from plan_step_run where process_id is not null
union
select execution_process_id, execution_process_start_time_ticks
from auto_step_run where execution_process_id is not null
union
select process_id, null from auto_step_run where process_id is not null
union
select executor_pid, cast(executor_process_identity as integer)
from workflow_execution where executor_pid is not null
