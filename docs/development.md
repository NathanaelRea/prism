# Development

Install the pinned SQLx CLI compatible with the crate's SQLx dependency:

```sh
cargo install sqlx-cli --version 0.8.6 --locked --no-default-features --features sqlite,rustls
```

After adding or changing a checked query or migration, refresh the committed
offline metadata against a migrated temporary database:

```sh
db="$(mktemp "${TMPDIR:-/tmp}/prism-sqlx.XXXXXX.db")"
workflow_db="$(mktemp "${TMPDIR:-/tmp}/prism-workflow-sqlx.XXXXXX.db")"
cargo sqlx migrate run --source migrations/repository --database-url "sqlite://$db"
cargo sqlx migrate run --source migrations/workflow --database-url "sqlite://$workflow_db"
# Build the compatible compile-time union used by checked queries.
for migration in migrations/workflow/*.sql; do
  sqlite3 "$db" ".read $migration"
done
cargo sqlx prepare --workspace --database-url "sqlite://$db" -- --all-targets
env DOCS_RS=1 PKG_CONFIG_ALLOW_CROSS=1 LIBSQLITE3_SYS_USE_PKG_CONFIG=1 \
  CC_aarch64_apple_darwin=clang \
  cargo sqlx prepare --workspace --database-url "sqlite://$db" \
    -- --all-targets --target aarch64-apple-darwin
rm -f "$db" "$db-wal" "$db-shm" "$workflow_db" "$workflow_db-wal" "$workflow_db-shm"
```

Review and commit the resulting `.sqlx` changes. `scripts/full-check.sh` verifies
that this metadata is current and then compiles/tests with `SQLX_OFFLINE=true`.

Run the local CI gate before pushing:

```sh
scripts/full-check.sh
```

Prism supports Linux, macOS, and native `x86_64-pc-windows-msvc`. On Linux, the gate runs native tests and Clippy, host-independent platform-policy tests, Darwin cross-Clippy for Apple Silicon, and Windows MSVC cross-Clippy. There are no architecture-specific macOS code paths warranting a duplicate Intel cross-check. Cross-compilation does not replace native macOS or Windows verification.

Run the focused native contracts on a prepared macOS host before the complete
suite:

```sh
scripts/platform-smoke.sh
scripts/full-check.sh
```

The smoke command requires real `opencode` and `tmux` executables. It selects the
`platform_smoke_native_` tests and the two real OpenCode/tmux integration tests.
The selected tests exercise native process, durability, Unix-socket, worker,
OpenCode, and tmux contracts without invoking a model. Deterministic policy,
errno classification, and fault-injection tests remain in the full suite except
where a staging test also proves a native durability primitive.

On native Windows with PowerShell 7, run the required root gate and pinned compatibility smoke:

```powershell
scripts/windows-check.ps1
scripts/install-windows-smoke-tools.ps1
npm install --global opencode-ai@1.17.20
scripts/windows-platform-smoke.ps1
```

The Windows gate covers formatting, SQLx-offline compilation, Clippy, normal tests, native process/IPC/security/persistence/storage contracts, and release-zip installation. The compatibility smoke uses native Git, psmux 3.3.7, Worktrunk 0.71.0 through `git-wt.exe`, and a real no-model OpenCode server. Interactive attach/detach, input, resize, restoration, and rendering remain a focused real-terminal check documented in [Windows interactive smoke](windows-interactive-smoke.md).

To synchronize the current worktree, including uncommitted files, to an
SSH-accessible Mac and run that smoke command without pushing a branch:

```sh
scripts/remote-macos-smoke.sh mac-builder prism-platform-smoke
```

`PRISM_MAC_HOST` and `PRISM_MAC_DIR` provide the same values without arguments.
The destination must already be a Git checkout or worktree. Its working files
are treated as a dedicated mirror: rsync deletes stale files while excluding
`.git` and `target`.

CI also runs no-model smoke coverage against a pinned real OpenCode binary on Linux, macOS, and Windows. To run the portable API test locally with an installed OpenCode:

```sh
PRISM_TEST_OPENCODE="$(command -v opencode)" \
  cargo test agent_runtime::opencode::tests::real_opencode_server_round_trips_prism_session_api \
    -- --ignored --exact
```

The smoke test starts `opencode serve`, waits for its health endpoint, and verifies Prism can create, list, retrieve, and persist a prompt in a session. It does not require provider credentials.

CI also exercises the full headless stack with the real Prism binary, OpenCode, and tmux or psmux in isolated runtime state. To run the Unix test locally:

```sh
PRISM_TEST_OPENCODE="$(command -v opencode)" \
PRISM_TEST_TMUX="$(command -v tmux)" \
  cargo test real_prism_opencode_tmux_stack_ensures_reusable_agent_session \
    -- --ignored --exact
```

The full-stack test creates a Git worktree, runs `prism agent ensure`, verifies the OpenCode-backed session, runs ensure again to check reuse, and cleans up the isolated server and runtime state. The Windows equivalent is `scripts/windows-platform-smoke.ps1`. Neither path invokes a model.

## Remote Compatibility

Remote adapter compatibility is intentionally separate from normal CI. The
weekly or manually dispatched `Remote compatibility` workflow runs fixture tests
against pinned local GitLab CE `18.2.0-ce.0` and Forgejo `11.0.1` API instances.
It also runs unauthenticated, read-only schema drift probes against GitHub.com,
GitLab.com, and Codeberg. It never creates or changes public data.

Run an individual local suite with Docker:

```sh
scripts/remote-compatibility.sh gitlab
scripts/remote-compatibility.sh forgejo
```

Run the public probes without credentials:

```sh
scripts/remote-drift-probe.sh all
```

The scripts print `SKIP` and succeed when a required tool, Docker daemon, or
pinned image is unavailable. A service that starts but violates its expected API
shape fails the compatibility run. See [Remote Hosting](remote-hosting.md) for
the recorded metadata and security boundaries.

## Worktrunk Compatibility

Prism's Worktrunk support floor is 0.58.0. CI installs and smoke-tests the pinned current version 0.71.0 on Linux, macOS, and Windows. The Windows gate verifies the upstream archive checksum and installs its executable as `git-wt.exe` to avoid collision with Windows Terminal's `wt.exe`. The smoke creates a repository whose path contains spaces, creates the `ci/real-smoke` branch, reads machine JSON from Worktrunk, and removes the worktree while preserving the branch. It sets `WORKTRUNK_CONFIG_PATH` and `WORKTRUNK_WORKTREE_PATH` under a temporary directory so it never reads or mutates user configuration or approvals.

`PRISM_TEST_WORKTRUNK` is the real-tool smoke selector and contains the absolute path to `wt`. To exercise the same binary selection locally:

```sh
PRISM_TEST_WORKTRUNK="$(command -v wt)"
test -x "$PRISM_TEST_WORKTRUNK"
"$PRISM_TEST_WORKTRUNK" --version
```

On Windows, use `$env:PRISM_TEST_WORKTRUNK = (Get-Command git-wt.exe).Source` instead. Parser coverage does not require Worktrunk. Redacted fixtures under `tests/fixtures/worktrunk` cover the 0.58.0 schema-1 floor and the schema-1/schema-2 output documented for 0.71.0, including absent and null observations. Unknown schemas must fail closed.

To enforce the same gate as a pre-push hook, opt into the versioned hooks:

```sh
git config core.hooksPath .githooks
```

## TUI Architecture

Prism's TUI is split between local application state and Ratatui/Crossterm terminal mechanics:

- `src/tui/mod.rs` owns Prism UI state, panel focus, selection, modal state, background polling, and action dispatch.
- `src/tui/runtime.rs` owns terminal lifecycle through Crossterm and Ratatui: raw mode, alternate screen, cursor visibility, event polling, resize events, drawing, and suspend/resume around tmux, lazygit, and shell handoff.
- `src/tui/input.rs` maps typed Crossterm key events into Prism-level `Key` values. It should not read raw stdin bytes or inspect repository/worktree domain state.
- `src/view/` defines terminal-backend-independent view models and the Ratatui renderer that translates them into layouts, widgets, styles, overlays, and test buffers.

Keep domain behavior out of renderer widgets. Rendering should consume view models, while state transitions and command decisions remain testable through `Tui` methods without a real terminal.

Dialogs currently use typed nested loops in `src/tui/mod.rs` instead of a single explicit `UiMode` state machine. This is an intentional Ratatui migration deviation: raw byte parsing is gone, dialog input uses Crossterm `KeyEvent`s, and those loops continue to tick background polling and redraw on resize. Consolidating help, prompt, confirm, and progress dialogs into a shared `UiMode` remains a future refactor if Prism adds richer modal editing or more dialog types.

## Prism Database Tables

Prism stores per-repository runtime state in `prism.db` under the user's Prism config directory. The most useful tables to inspect are:

- `task_metadata`, `hidden_session`, `agent_state`: worktree session metadata and local session state.
- `opencode_runtime`: OpenCode server/session records associated with worktrees.
- `plan_run`, `plan_step_run`, `plan_output_line`: persisted Plan Mode runs, step state, and bounded step output.
- `auto_run`, `auto_step_run`, `auto_output_line`, `auto_event`: persisted Auto Flow runs, attempts, output, and event history.
- `pr_cache`, `pr_details_cache`: provider-neutral change-request summary and
  detail caches; the historical table names are retained for migration safety.
- `event`, `startup_run`, `startup_phase`: observability events and startup timing records.

The generalized worker owns the separate user-scoped `workflow.db` in the Prism
config directory. It contains definition snapshots, runs, steps, fenced
attempts, output, artifacts, approvals, effects, triggers, resource claims,
import journals, audit events, and control-plane metrics. Repository migrations
must never be run against this database, and workflow migrations must never be
run against `prism.db`.

## Workflow Database Diagnostics And Recovery

Start with `prism debug info`. Its `control_plane.*` facts report the latest
writer wait and transaction times, reader/writer pool use, scheduler candidates,
unsupported runnable work, due gates and triggers, output truncation, and effects
requiring reconciliation. A growing writer wait with short transactions usually
means writes should be batched or polling reduced; do not increase the SQLite
writer count. A long transaction points to work that should be moved outside the
transaction. Reader saturation should be established from repeated samples
before changing the internal four-reader limit.

When migration or import fails:

1. Stop the Prism worker so no new workflow mutation can begin.
2. Preserve `workflow.db` and any `-wal`/`-shm` companions before investigation.
3. Keep the owner-only `*.pre-sqlx-backup` adoption backup. Never delete the
   original database or edit `_sqlx_migrations` by hand.
4. Record the complete error and inspect `pragma quick_check`,
   `pragma foreign_key_check`, and the migration/import journal on a copy.
5. Restore only from the preserved copy or backup after identifying the failed
   boundary. Unknown, future, and corrupt schemas intentionally fail closed.

Waiting workflows do not own tasks or execution slots. Pressure is represented
by scheduler candidates and durable backlog, while active attempts are bounded
independently by implementation, target, provider, repository, and resource
claims. Output and heartbeats are batched; output truncation is explicit rather
than an invitation to grow an unbounded buffer.
