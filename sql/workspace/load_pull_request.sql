select number, title, url, state, merge_state_status, check_status,
       refreshed_unix_ms, merged, draft, observation_error
from pr_cache where branch = ?
