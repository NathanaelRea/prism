select execution_process_id as "process_id!", execution_process_start_time_ticks as process_identity
from auto_step_run where run_id = ? and execution_process_id is not null
union
select execution_process_id, execution_process_start_time_ticks
from plan_step_run where run_id in (
  select plan_run_id from auto_step_run where run_id = ? and plan_run_id is not null
) and execution_process_id is not null
