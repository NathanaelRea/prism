select dispatch_state, interruption_generation
from workflow_execution
where run_id = 'plan-control-12345678'
