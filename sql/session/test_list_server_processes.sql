select server_pid as "server_pid!: i64", server_port
from opencode_runtime
where server_pid is not null
