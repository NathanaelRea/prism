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
- **Customization**: Users can override executable paths for Git, GitHub and
  GitLab CLIs, tmux, Worktrunk, lazygit, fzf, and configured harnesses.
- **Behavior**: The TUI provides a global harness chooser for the fixed built-in
  IDs and configured generic harnesses, and can collect interactive and optional
  headless commands when creating a generic harness.
- **Behavior**: When no global harness has been selected, first interactive
  startup offers the installed built-in harnesses and persists the selection
  before validating the selected harness.
- **Invariant**: `opencode`, `codex`, `claude`, and `pi` are reserved harness IDs
  bound to their matching built-in adapters. Custom IDs use the generic adapter.
- **Behavior**: Startup validates tools required for the selected mode and names
  missing tools and relevant configuration locations. Optional tools are checked
  only when their actions require them.
- **Behavior**: `prism doctor` reports tool availability and versions; GitHub
  and GitLab CLI authentication; Forgejo credential-source availability; the
  resolved remote host/provider, capabilities, and server version when
  discoverable; configured checks; selected harness capabilities; and discovered
  worktrees, without printing credential values.
- **Behavior**: Startup rejects Worktrunk versions below 0.58.0. Diagnostics
  report the detected Worktrunk version and minimum; observation failures use a
  bounded safe summary rather than raw command output or development URLs.
- **Default**: Desktop notifications are disabled. When enabled, category switches
  independently control input-required, completed, and failed/restart Agent
  Session transitions and may be overridden per repository.
- **Invariant**: Enabling or reloading notifications establishes current Agent
  Session states as a baseline and never reports persisted attention states.
- **Constraint**: Desktop notification delivery is best effort, nonblocking, and
  limited to accepted transitions while the dashboard event loop runs. Missing
  graphical services, queue pressure, and backend failures never change session
  state or fail a workflow.

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
- **Invariant**: Every SQLite connection enables foreign keys and
  `synchronous=FULL`; TUI/read-only connections additionally use `query_only`
  with no busy wait, while writers use a bounded busy wait. Repository databases
  require verified WAL support on a local filesystem.

## Diagnostics

- **Default**: Per-repository runtime logs rotate at 5 MiB and retain three
  rotated files.
- **Behavior**: Debug controls expose state paths, effective runtime facts,
  bounded recent logs, and startup timing. Log-level and stderr controls are
  available without changing normal output.
- **Behavior**: `prism debug integrity` reports full SQLite integrity and foreign
  key checks through a read-only path that never initializes, migrates, repairs,
  or records observability data.
- **Behavior**: The interactive TUI maintains an always-on, bounded in-memory
  flight recorder for the previous 60 seconds. `prism debug record` asks the
  running TUI to retain that history, capture 30 more seconds by default, and
  atomically write a JSONL artifact under the repository's `recordings`
  directory. The command supports bounded before/after overrides.
- **Invariant**: Flight-recorder producers never write SQLite or runtime logs,
  wait for recorder storage, or take ownership of the diagnostic ring. They use
  a bounded nonblocking channel and drop diagnostics under pressure; one
  dedicated thread owns the fixed-size ring, capture window, percentile
  summaries, and artifact writes.
- **Behavior**: Flight recordings use monotonic process-relative timestamps and
  include input-to-handled and input-to-frame latency; TUI tick, model, render,
  and terminal-write duration; queue depth and job-attributed drop/coalescing;
  attach, detach, focus, idle, suspend, and resume phases; SQLite open and
  operation timing with UI-thread attribution and an explicit upper bound for
  busy/locked failures; tmux target, generation, poll
  interval, and retry reason; post-idle completion bursts; and output query row
  and byte counts. Capture summaries report count, p50, p95, and max duration by
  operation.
- **Behavior**: Flight recordings include external subprocess start and terminal
  timing with safe logical tool and operation names, outcome, deadline and
  termination classification, and bounded output byte counts where Prism owns
  capture. Calls made inside TUI jobs include the job ID and static job type.
- **Behavior**: Direct local OpenCode HTTP calls include total, resolution,
  connection, write, first-byte, and read timing where each phase completes,
  together with method, status, timeout, and byte counts. OpenCode SSE records
  one connection lifecycle summary with handshake and stream timing, aggregate
  payload count and bytes, and a bounded terminal reason; it does not emit one
  flight event per payload.
- **Constraint**: GitHub CLI operations are opaque subprocesses. Flight recordings
  report whole-`gh` process timing and never claim DNS, HTTP, retry, or endpoint
  timing for requests made internally by `gh`.
- **Invariant**: External-call flight events never contain argv, dynamic URLs,
  query values, bodies, headers, environment values, repository paths, session
  IDs, branch names, or raw stderr.
- **Invariant**: Desktop notification diagnostics are throttled and contain only
  platform, failure category, and notification kind, never notification text.
- **Behavior**: Corruption-class SQLite failures trigger best-effort read-only
  `quick_check` and foreign-key diagnostics without replacing the original error
  or modifying the database.
- **Behavior**: Reliability boundaries use the existing event store with stable
  structured fields: supervised jobs record identity, generation, terminal
  outcome, elapsed time, and deadline; queue-pressure records are aggregated;
  SQLite failures expose classified primary and extended codes and busy time;
  atomic writes record category, stage, commit state, and durability; and TUI
  cleanup records its shutdown reason and active/unfinished job counts.
- **Constraint**: SQLite failures, supervised-job terminals, TUI queue pressure
  and shutdown cleanup, and atomic-write terminals are deferred rather than
  synchronously opening the observability database. They remain in the runtime
  log and are flushed to the event table after a later successful typed writer
  operation. The bounded deferred queue reports sparse overflow totals; overflow
  can omit database copies but not the original runtime-log evidence.
- **Invariant**: Structured diagnostics retain command-argument and free-form
  secret redaction. High-rate SSE drops are represented by sparse aggregate queue
  events rather than one synchronous event per drop.
- **Invariant**: Cache observations distinguish never loaded, refreshing, stale,
  failed, confirmed absent, and present states. A transient failure does not
  erase known state, while confirmed absence requires affirmative evidence.
- **Invariant**: Worktrunk schema 1 and schema 2 observations normalize into the
  same typed URL, listening, variable, and custom-column facts. Unknown schema
  versions fail closed and preserve the previous successful observation as
  stale; they are never interpreted as an empty result.
- **Invariant**: Worktrunk hook tails are read only from canonical regular files
  under `.git/wt/logs`, are bounded by bytes and lines, and have terminal control
  sequences removed. Raw URLs and hook bodies do not enter runtime logs, flight
  recordings, SQLite, or structured failure summaries.
