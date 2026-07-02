# Configuration

Tracked repositories live in `~/.config/prism/repos.toml`.

Each repository entry has a path and may have a digit key. Digit keys are used as `Space <digit>` shortcuts in the TUI.

```toml
[[repos]]
path = "/path/to/repo"
key = "1"
```

Repository-specific Prism config lives under the repository config path shown by `e` from the Repos panel. Common settings include `default_base`, layout width, worktree columns, merge method, OpenCode runtime settings, tools, and prompt templates.

Use `R` from Prism to edit repository order, keys, and tracked repositories.

```toml
default_base = "main"
merge_method = "squash"

# Prism starts local OpenCode servers on deterministic ports in this range.
opencode_port_base = 41000
opencode_port_span = 1000

# Default false keeps OpenCode servers warm after Prism exits.
opencode_shutdown_owned_servers = false

[layout]
sidebar_width = 56

[worktrees]
columns = ["url", "ci.status", "vars.localdev"]

[tools]
opencode = "opencode"

[prompt_templates]
review_fix = "Here are review comments on PR {pr_number}.\n\nIf they are applicable, fix them. Otherwise, say why not.\n\n---\n\n{comments}"
ci_failure = "Here are CI failures on PR {pr_number}.\n\nFix the failing checks. Use the log tails below as the primary clues.\n\nPR: {url}\nBranch: {branch}\nHead SHA: {head_sha}\n\n---\n\n{failures}"
```

Prism treats `main` as the default branch by default. The default branch is not polled or shown as a pull request branch.

Prism uses squash merges for pull requests by default. Set `merge_method` to `merge` or `rebase` if a repository requires a different GitHub merge method.

Prism manages one local OpenCode server per worktree session. `opencode_port_base` and `opencode_port_span` define the deterministic local port range used for those servers. By default Prism keeps servers warm after the TUI exits; set `opencode_shutdown_owned_servers = true` to send SIGTERM to OpenCode servers that Prism spawned during the session.

`[layout] sidebar_width` controls the Status/Repos/Worktrees sidebar width in terminal columns. Values are bounded to `20..=120`. When the terminal is too narrow, Prism reduces the configured width so the main panel keeps usable space; this preserves the board layout instead of strictly honoring a width that would hide the main panel.

`[worktrees] columns` controls the visible extra columns in the TUI worktree list. Columns are shown in the configured order after Prism's built-in worktree indicators. Missing values render as a compact placeholder so neighboring columns stay aligned.

Columns are read from `wt list --format=json`. Common names include `url`, `url_active`, `ci.status`, and `vars.<name>`, such as `vars.localdev`:

```toml
[worktrees]
columns = ["url", "url_active", "ci.status", "vars.localdev"]
```

The selected worktree detail panel shows all currently loaded `wt` column keys and values, sorted by key, so you can discover names before adding them to config. Use `C` from the Repos panel to open the repository config at the worktree column section, then save and return to Prism to reload.
