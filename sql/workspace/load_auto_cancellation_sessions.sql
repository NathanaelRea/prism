select session_adapter_id as adapter_id, session_endpoint as endpoint, session_id as "id!"
from auto_step_run where run_id = ? and session_id is not null
union
select session_adapter_id, session_endpoint, session_id
from plan_step_run where run_id in (
  select plan_run_id from auto_step_run where run_id = ? and plan_run_id is not null
) and session_id is not null
