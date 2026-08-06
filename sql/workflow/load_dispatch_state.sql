select dispatch_state as "dispatch_state!"
from workflow_execution where workflow_kind = ? and run_id = ?
