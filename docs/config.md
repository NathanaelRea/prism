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

[prompt_templates]
review_fix = "Here are review comments on PR {pr_number}.\n\nIf they are applicable, fix them. Otherwise, say why not.\n\n---\n\n{comments}"
ci_failure = "Here are CI failures on PR {pr_number}.\n\nFix the failing checks. Use the log tails below as the primary clues.\n\nPR: {url}\nBranch: {branch}\nHead SHA: {head_sha}\n\n---\n\n{failures}"
```

Prism treats `main` as the default branch by default. The default branch is not polled or shown as a pull request branch.

Prism uses squash merges for pull requests by default. Set `merge_method` to `merge` or `rebase` if a repository requires a different GitHub merge method.

Prism manages one local OpenCode server per worktree session. `opencode_port_base` and `opencode_port_span` define the deterministic local port range used for those servers. By default Prism keeps servers warm after the TUI exits; set `opencode_shutdown_owned_servers = true` to send SIGTERM to OpenCode servers that Prism spawned during the session.
