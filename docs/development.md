# Development

Run the local CI gate before pushing:

```sh
scripts/full-check.sh
```

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
- `pr_cache`, `pr_details_cache`: GitHub pull request summaries and detail payload caches.
- `event`, `startup_run`, `startup_phase`: observability events and startup timing records.
