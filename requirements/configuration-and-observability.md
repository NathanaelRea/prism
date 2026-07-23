# Configuration And Observability

## Configuration Experience

- **Behavior**: `E` edits and reloads global settings, distinct from `e` editing
  the selected repository's settings.
- **Invariant**: Effective repository configuration applies built-in defaults,
  then global settings, then repository settings. Unspecified repository values
  inherit their effective global values.
- **Behavior**: Initial terminal presentation setup explains and offers Nerd Font
  icons or a Unicode fallback. It does not claim to detect font support
  automatically.
- **Default**: Unicode is the compatibility fallback when Nerd Font support is
  not selected.
- **Behavior**: Prism can generate a useful commented TOML configuration, publish
  a JSON Schema applicable to that configuration, and expose CLI commands that
  make configuration locations and options discoverable.
- **Behavior**: `prism db` provides an interactive way to inspect Prism's SQLite
  state comparable to `opencode db`.
- **Constraint**: Normal Prism operation does not require the external `sqlite3`
  executable. Build-time and runtime prerequisites are documented separately.
- **Customization**: Users can override executable paths for Git, GitHub CLI,
  tmux, Worktrunk, lazygit, fzf, and configured harnesses.
- **Behavior**: The TUI provides a global harness chooser for the fixed built-in
  IDs and configured generic harnesses, and can collect interactive and optional
  headless commands when creating a generic harness.
- **Invariant**: `opencode`, `codex`, `claude`, and `pi` are reserved harness IDs
  bound to their matching built-in adapters. Custom IDs use the generic adapter.
- **Behavior**: Startup validates tools required for the selected mode and names
  missing tools and relevant configuration locations. Optional tools are checked
  only when their actions require them.
- **Behavior**: `prism doctor` reports tool availability and versions, GitHub
  authentication, configured checks, selected harness capabilities, and discovered
  worktrees.

## Verification Commands

- **Customization**: Repositories can configure ordered `pre_push`, `pre_pr`, and
  `review_fix` command lists. Commands run in the affected worktree and stop at
  the first failure.
- **Behavior**: Auto Flow local verification runs pre-push and pre-PR commands
  plus a non-mutating merge-conflict check against the Default Branch. Review
  repairs additionally run review-fix commands.
- **Behavior**: An empty verification configuration is reported explicitly but
  does not fail solely because no commands were configured.

## Command Line And Database

- **Behavior**: `--repo <path>` accepts a path inside a Git working tree,
  resolves its root, and supplies repository context to repository-scoped
  commands. Repository-independent help and diagnostics remain available when no
  repository can be resolved.
- **Invariant**: `prism auto` resumes the most recent active run for the selected
  repository before considering a new prompt or plan.
- **Behavior**: Bare `prism db` initializes and migrates the selected database,
  then opens writable interactive access through `sqlite3`; `prism db path`
  prints its path; `prism db <query>` uses built-in read-only SQLite support and
  prints tab-separated rows.

## Diagnostics

- **Default**: Per-repository runtime logs rotate at 5 MiB and retain three
  rotated files.
- **Behavior**: Debug controls expose state paths, effective runtime facts,
  bounded recent logs, and startup timing. Log-level and stderr controls are
  available without changing normal output.
- **Invariant**: Cache observations distinguish never loaded, refreshing, stale,
  failed, confirmed absent, and present states. A transient failure does not
  erase known state, while confirmed absence requires affirmative evidence.
