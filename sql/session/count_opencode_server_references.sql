select count(*) as "count!"
from opencode_runtime
where repo_root = ? and server_url = ?
