# Development

Run the local CI gate before pushing:

```sh
scripts/full-check.sh
```

Prism supports Linux and macOS. On Linux, the gate runs native tests and clippy,
the host-independent policy tests for both supported operating systems, and
Darwin cross-clippy for Apple Silicon. There are no architecture-specific code
paths warranting a duplicate Intel cross-check. Cross-compilation does not
replace native macOS verification.

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

To synchronize the current worktree, including uncommitted files, to an
SSH-accessible Mac and run that smoke command without pushing a branch:

```sh
scripts/remote-macos-smoke.sh mac-builder prism-platform-smoke
```

`PRISM_MAC_HOST` and `PRISM_MAC_DIR` provide the same values without arguments.
The destination must already be a Git checkout or worktree. Its working files
are treated as a dedicated mirror: rsync deletes stale files while excluding
`.git` and `target`.

CI also runs a no-model smoke test against a pinned real OpenCode binary on Linux and macOS. To run it locally with an installed OpenCode:

```sh
PRISM_TEST_OPENCODE="$(command -v opencode)" \
  cargo test opencode::tests::real_opencode_server_round_trips_prism_session_api \
    -- --ignored --exact
```

The smoke test starts `opencode serve`, waits for its health endpoint, and verifies Prism can create, list, retrieve, and persist a prompt in a session. It does not require provider credentials.

CI also exercises the full headless stack with the real Prism binary, OpenCode, and tmux on an isolated socket. To run it locally:

```sh
PRISM_TEST_OPENCODE="$(command -v opencode)" \
PRISM_TEST_TMUX="$(command -v tmux)" \
  cargo test real_prism_opencode_tmux_stack_ensures_reusable_agent_session \
    -- --ignored --exact
```

The full-stack test creates a Git worktree, runs `prism agent ensure`, verifies the OpenCode-backed tmux session, runs ensure again to check reuse, and cleans up the isolated server and socket. It does not invoke a model.

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

Prism's Worktrunk support floor is 0.58.0. CI installs and smoke-tests the pinned current version 0.71.0 on Linux and macOS using the versioned installer published with that upstream release. The smoke creates a repository whose path contains spaces, creates the `ci/real-smoke` branch, reads `wt list --format=json`, and removes the worktree while preserving the branch. It sets `WORKTRUNK_CONFIG_PATH` and `WORKTRUNK_WORKTREE_PATH` under a temporary directory so it never reads or mutates user configuration or approvals.

`PRISM_TEST_WORKTRUNK` is the real-tool smoke selector and contains the absolute path to `wt`. To exercise the same binary selection locally:

```sh
PRISM_TEST_WORKTRUNK="$(command -v wt)"
test -x "$PRISM_TEST_WORKTRUNK"
"$PRISM_TEST_WORKTRUNK" --version
```

Parser coverage does not require Worktrunk. Redacted fixtures under `tests/fixtures/worktrunk` cover the 0.58.0 schema-1 floor and the schema-1/schema-2 output documented for 0.71.0, including absent and null observations. Unknown schemas must fail closed.

To enforce the same gate as a pre-push hook, opt into the versioned hooks:

```sh
git config core.hooksPath .githooks
```

## TUI Architecture

Prism's TUI is split between local application state and Ratatui/Crossterm terminal mechanics:

- `src/tui.rs` owns Prism UI state, panel focus, selection, modal state, background polling, and action dispatch.
- `src/tui_runtime.rs` owns terminal lifecycle through Crossterm and Ratatui: raw mode, alternate screen, cursor visibility, event polling, resize events, drawing, and suspend/resume around tmux, lazygit, and shell handoff.
- `src/input.rs` maps typed Crossterm key events into Prism-level `Key` values. It should not read raw stdin bytes or inspect repository/worktree domain state.
- `src/view/` defines terminal-backend-independent view models and the Ratatui renderer that translates them into layouts, widgets, styles, overlays, and test buffers.

Keep domain behavior out of renderer widgets. Rendering should consume view models, while state transitions and command decisions remain testable through `Tui` methods without a real terminal.

Dialogs currently use typed nested loops in `src/tui.rs` instead of a single explicit `UiMode` state machine. This is an intentional Ratatui migration deviation: raw byte parsing is gone, dialog input uses Crossterm `KeyEvent`s, and those loops continue to tick background polling and redraw on resize. Consolidating help, prompt, confirm, and progress dialogs into a shared `UiMode` remains a future refactor if Prism adds richer modal editing or more dialog types.

## Prism Database Tables

Prism stores per-repository runtime state in `prism.db` under the user's Prism config directory. The most useful tables to inspect are:

- `task_metadata`, `hidden_session`, `agent_state`: worktree session metadata and local session state.
- `opencode_runtime`: OpenCode server/session records associated with worktrees.
- `plan_run`, `plan_step_run`, `plan_output_line`: persisted Plan Mode runs, step state, and bounded step output.
- `auto_run`, `auto_step_run`, `auto_output_line`, `auto_event`: persisted Auto Flow runs, attempts, output, and event history.
- `pr_cache`, `pr_details_cache`: provider-neutral change-request summary and
  detail caches; the historical table names are retained for migration safety.
- `event`, `startup_run`, `startup_phase`: observability events and startup timing records.
