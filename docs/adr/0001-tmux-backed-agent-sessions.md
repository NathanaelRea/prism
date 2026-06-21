# ADR 0001: Tmux-Backed Agent Sessions

## Status

Accepted

## Context

Prism needs agent-backed coding sessions to survive TUI redraws, panel focus
changes, terminal detach/reattach, and normal command navigation. A worktree
session should have a stable place where the user can inspect and continue the
agent, open lazygit, or drop into a shell without Prism owning every byte of the
interactive terminal protocol.

## Decision

Prism uses tmux as the backing runtime for interactive agent sessions.

Each agent session is a tmux session named from the repository hash, branch, and
generation. Window 1 runs the configured interactive agent command, window 2 is
lazygit, and window 3 is a shell in the same worktree. Prism attaches to these
windows instead of embedding the interactive agent inside the TUI process.

Plan mode also uses a dedicated tmux session so long-running plan execution can
detach from the board and keep terminal ownership simple.

## Consequences

- Session lifecycle code should preserve stable tmux naming, window indexes, and
  reattach behavior.
- Interactive terminal behavior belongs in the tmux module, not in generic
  process helpers.
- The tmux adapter exposes the Agent Session runtime identity: session name,
  window targets, prompt buffer naming, and readiness checks.
- Tests should cover command construction, naming, readiness, and lifecycle
  behavior without requiring broad TUI setup.
- Prism does not keep an embedded PTY agent runtime in `Session`; tmux is the
  only interactive Agent Session runtime.
- Supporting a non-tmux backend would be a new architecture decision, not a
  local refactor.
