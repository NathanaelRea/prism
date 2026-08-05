select session_adapter_id as adapter_id, session_endpoint as endpoint, session_id as "id!"
from plan_step_run where run_id = ? and session_id is not null
