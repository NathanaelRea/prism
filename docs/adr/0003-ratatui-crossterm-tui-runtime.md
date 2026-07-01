# ADR 0003: Ratatui and Crossterm TUI Runtime

## Status

Accepted

## Context

ADR 0002 prefers local modules for terminal control and UI rendering. That kept
Prism's dependency set small, but the current TUI now owns terminal mechanics
that are risky to keep hand-rolled during the Ratatui migration:

- Raw mode and alternate-screen lifecycle through direct terminal commands.
- Byte-level keyboard decoding.
- Resize detection through terminal-size polling.
- Manual ANSI frame composition, style escapes, clipping, and cursor control.
- String-based render tests instead of terminal buffer assertions.

The migration is not an application-framework adoption. Prism's repositories,
tracked repositories, worktree sessions, agent sessions, PR cache state, Plan
mode, Auto Flow, view models, and action dispatch remain local domain code.

## Decision

Prism will add Ratatui and Crossterm as a specific exception to ADR 0002's
terminal-rendering preference:

- `ratatui = { version = "0.30.2", default-features = false, features = ["crossterm"] }`
- `crossterm = { version = "0.29.0", default-features = false, features = ["events"] }`

The exception covers terminal runtime and rendering mechanics only. The old
custom renderer will not be preserved behind a fallback feature flag; once the
Ratatui path replaces a responsibility, the corresponding local terminal code
should be deleted.

## Feature Review

`cargo tree -e features` shows these notable terminal/rendering surfaces:

- Prism directly enables Crossterm's `events` feature for typed terminal events.
- Ratatui's `crossterm` feature enables `std`, `ratatui-crossterm`,
  `ratatui-core`, and `ratatui-widgets`.
- `ratatui-crossterm` currently enables Crossterm's default feature set through
  the backend adapter, including `bracketed-paste`, `derive-more`, `events`, and
  `windows`.
- Crossterm's event stack pulls in `mio`, `signal-hook`, `signal-hook-mio`,
  `parking_lot`, `rustix`, and platform support crates.
- Ratatui core/widgets pull in width, segmentation, style, layout, and widget
  support crates including `unicode-width`, `unicode-segmentation`,
  `unicode-truncate`, `compact_str`, `lru`, `palette`, `strum`, and `time`.

The dependency surface is larger than the previous local renderer, but it is
bounded to terminal concerns and avoids optional Ratatui serialization,
alternate backends, macros, calendar widgets, palette features, and snapshot
testing crates.

## Responsibilities Removed From Local Code

As migration phases land, Ratatui and Crossterm should replace local code for:

- Raw mode and alternate-screen management.
- Typed key decoding and resize events.
- Terminal layout, clipping, style application, cursor visibility, and frame
  presentation.
- Testable terminal buffers for renderer tests.

## Responsibilities That Remain Local

Prism keeps local ownership of:

- Domain state and persistence.
- Action dispatch and background polling.
- Prism view models and formatting that are independent of terminal output.
- TUI-specific widgets and modal state.
- External-tool handoff behavior for tmux, lazygit, shells, and agent sessions.

## Consequences

- Dependency review for this migration is isolated from behavior changes.
- Terminal restore safety and event decoding move to maintained crates.
- Future UI tests can assert Ratatui buffers instead of ANSI string frames.
- The migration should delete replaced terminal code instead of maintaining dual
  renderers.
