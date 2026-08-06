select distinct execution_process_id as "process_id!",
  execution_process_start_time_ticks as "start_time_ticks?"
from plan_step_run
where run_id = ? and execution_process_id is not null
