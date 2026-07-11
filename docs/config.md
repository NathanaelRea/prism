# Configuration

Tracked repositories live in `~/.config/prism/repos.toml`.

Global Prism settings live in `~/.config/prism/config.toml`. Press `E` in the TUI to edit this file and reload configuration.

Run `prism config example` to print a complete commented config template, `prism config schema` to print the JSON Schema used by TOML editor tooling, and `prism config paths` to inspect the active config paths and schema URL.

Each repository entry has a path and may have a digit key. Digit keys are used as `Space <digit>` shortcuts in the TUI.

```toml
[[repos]]
path = "/path/to/repo"
key = "1"
```

Repository-specific Prism config lives under the repository config path opened by `e`. Common settings include `default_base`, layout width, worktree columns, merge method, Auto Flow and PR Stabilization behavior, OpenCode runtime settings, tools, and prompt templates.

Per-repository Prism state also lives under that repository config directory, not inside the project repository. The state database is named `prism.db` and stores worktree session metadata, OpenCode runtime records, Plan Mode and Auto Flow runs, PR cache data, and observability records.

Use `R` from Prism to edit repository order, keys, and tracked repositories.

```toml
#:schema https://raw.githubusercontent.com/NathanaelRea/prism/main/schemas/config.schema.json

default_base = "main"
merge_method = "squash"

# Prism starts local OpenCode servers on deterministic ports in this range.
opencode_port_base = 41000
opencode_port_span = 1000

# Default false keeps OpenCode servers warm after Prism exits.
opencode_shutdown_owned_servers = false

[layout]
sidebar_width = 56

[ui]
icon_style = "unicode" # or "nerd-font"

[worktrees]
columns = []

[tools]
opencode = "opencode"

[prompt_templates]
review_fix = "Here are review comments on PR {pr_number}.\n\nIf they are applicable, fix them. Otherwise, say why not.\n\n---\n\n{comments}"
ci_failure = "Here are CI failures on PR {pr_number}.\n\nFix the failing checks. Use the log tails below as the primary clues.\n\nPR: {url}\nBranch: {branch}\nHead SHA: {head_sha}\n\n---\n\n{failures}"
repair_commit_review = "fix: cr"
repair_commit_ci = "fix: ci"
repair_commit_merge = "fix: merge"
```

The `#:schema` line is an optional TOML comment. Prism ignores it, while Taplo-compatible TOML language servers can use it for completions, descriptions, enum values, and type validation.

The `review_fix` template supports `{inline_comments}`, `{review_bodies}`, and `{pr_comments}` when feedback sources need separate placement. `{comments}` combines all three for compatibility. Copilot's review overview is excluded from review bodies; its unresolved inline comments remain actionable feedback.

Prism treats `main` as the default branch by default. The default branch is not polled or shown as a pull request branch.

Prism uses squash merges for pull requests by default. Set `merge_method` to `merge` or `rebase` if a repository requires a different GitHub merge method.

## Auto Flow and PR Stabilization

`[auto]` controls Auto Flow implementation automation and PR Stabilization gate behavior:

```toml
[auto]
merge = false
cleanup_after_merge = false
require_review_approval = false
push_initial = true
push_repairs = false
review_wait_enabled = true
ci_wait_enabled = true
```

`merge = false` makes successful stabilization stop at `ReadyForManualMerge`. Set it to `true` only when Prism may merge after all required gates pass and repository policy is known.

`push_initial = true` allows Auto Flow to push the initial implementation commit and open or refresh the PR. `push_repairs = false` keeps managed review and CI repair commits local as guarded pending pushes for user inspection; use `Space g P` to push them after review.

`require_review_approval = false` means review approval is not required unless repository policy requires it. When enabled, PR Stabilization treats missing approval as a blocker.

`review_wait_enabled` and `ci_wait_enabled` control whether Auto Flow waits for review and CI observations when those work items are selected.

Repair commit subjects are configured with prompt templates. If omitted, Prism uses these defaults:

```toml
[prompt_templates]
repair_commit_review = "fix: cr"
repair_commit_ci = "fix: ci"
repair_commit_merge = "fix: merge"
```

Prism manages one local OpenCode server per worktree session. `opencode_port_base` and `opencode_port_span` define the deterministic local port range used for those servers. By default Prism keeps servers warm after the TUI exits; set `opencode_shutdown_owned_servers = true` to send SIGTERM to OpenCode servers that Prism spawned during the session.

`[layout] sidebar_width` controls the Status/Repos/Worktrees sidebar width in terminal columns. Values are bounded to `20..=120`. When the terminal is too narrow, Prism reduces the configured width so the main panel keeps usable space; this preserves the board layout instead of strictly honoring a width that would hide the main panel.

`[ui] icon_style` controls TUI status glyphs. `unicode` is the portable default. `nerd-font` uses richer Nerd Font glyphs for pull requests, merge state, Git status, and CI, and requires a Nerd Font configured in your terminal.

`[worktrees] columns` controls the visible extra columns in the TUI worktree list. There are no extra columns enabled by default. Columns are shown in the configured order after Prism's built-in worktree indicators. Missing values render as a compact placeholder so neighboring columns stay aligned.

Columns are read from `wt list --format=json`. Common names include `url`, `url_active`, `ci.status`, and `vars.<name>`, such as `vars.localdev`:

```toml
[worktrees]
columns = ["url", "url_active", "ci.status", "vars.localdev"]
```

Use `C` in the TUI to open the selected repository's worktree column selector. The selector lists configured columns first and then discovered `wt` column keys, so you can enable/disable columns and move enabled columns up/down without editing TOML directly.

## Database Access

Use `prism db` commands to inspect a repository's local Prism state:

```sh
prism db
prism db path
prism db "select name from sqlite_schema where type = 'table' order by name"
prism db 'select id, status from plan_run order by updated_unix_ms desc'
```

Bare `prism db` opens an interactive `sqlite3` shell for the selected repository database. Prism initializes and migrates the database before launching the shell. This is direct writable SQLite access; quit Prism first if you are doing manual repairs to avoid lock contention or conflicting writes.

`prism db path` prints the selected repository database path and exits.

`prism db <query>` runs a read-only query and prints tab-separated rows for scripts. Write statements are rejected in query mode. Query mode uses Prism's built-in SQLite support and does not require the external `sqlite3` command.

When running outside the checkout you want to inspect, select the repository explicitly:

```sh
prism --repo /path/to/repo db
prism --repo /path/to/repo db path
```

If bare `prism db` reports that `sqlite3` is missing, install the SQLite command-line shell and make sure `sqlite3` is on your `PATH`.
