select prompt_summary, classification, visibility
from task_metadata
where branch = ?
