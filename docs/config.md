# Configuration

Tracked repositories live in `~/.config/prism/repos.toml`.

Each repository entry has a path and may have a digit key. Digit keys are used as `Space <digit>` shortcuts in the TUI.

```toml
[[repos]]
path = "/path/to/repo"
key = "1"
```

Repository-specific Prism config lives under the repository config path shown by `e` from the Repos panel. Common settings include `default_base`, worktree columns, merge method, OpenCode runtime settings, tools, and prompt templates.

Use `R` from Prism to edit repository order, keys, and tracked repositories.

```toml
default_base = "main"
merge_method = "squash"

# Prism starts local OpenCode servers on deterministic ports in this range.
opencode_port_base = 41000
opencode_port_span = 1000

# Default false keeps OpenCode servers warm after Prism exits.
opencode_shutdown_owned_servers = false

[worktrees]
columns = ["url", "ci"]

[tools]
opencode = "opencode"
```
