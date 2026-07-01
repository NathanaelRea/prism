# Development

Run the local CI gate before pushing:

```sh
scripts/full-check.sh
```

To enforce the same gate as a pre-push hook, opt into the versioned hooks:

```sh
git config core.hooksPath .githooks
```

## TUI Architecture

Prism's TUI is split between local application state and Ratatui/Crossterm terminal mechanics:

- `src/tui.rs` owns Prism UI state, panel focus, selection, modal state, background polling, and action dispatch.
- `src/tui_runtime.rs` owns terminal lifecycle through Crossterm and Ratatui: raw mode, alternate screen, cursor visibility, event polling, resize events, drawing, and suspend/resume around tmux, lazygit, and shell handoff.
- `src/input.rs` maps typed Crossterm key events into Prism-level `Key` values. It should not read raw stdin bytes or inspect repository/worktree domain state.
- `src/view.rs` defines terminal-backend-independent view models such as `FrameModel`, row models, dashboard models, and dialog models.
- `src/rat_view.rs` is the Ratatui renderer. It translates view models into layouts, widgets, styles, overlays, and test buffers.

Keep domain behavior out of renderer widgets. Rendering should consume view models, while state transitions and command decisions remain testable through `Tui` methods without a real terminal.

Dialogs currently use typed nested loops in `src/tui.rs` instead of a single explicit `UiMode` state machine. This is an intentional Ratatui migration deviation: raw byte parsing is gone, dialog input uses Crossterm `KeyEvent`s, and those loops continue to tick background polling and redraw on resize. Consolidating help, prompt, confirm, and progress dialogs into a shared `UiMode` remains a future refactor if Prism adds richer modal editing or more dialog types.
