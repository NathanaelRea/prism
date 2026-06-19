# ADR 0002: Dependency Policy

## Status

Accepted

## Context

Prism is a small terminal application with a broad external-tool surface:
`git`, `gh`, `tmux`, `wt`, `lazygit`, an agent command, and optional clipboard
tools. The Rust dependency set should stay small enough that builds remain
predictable and future reviews can reason about behavior without sorting through
unnecessary framework code.

At the same time, Prism should not hand-roll parsing or persistence when a
well-maintained crate gives safer behavior with less code.

## Decision

Prism keeps Rust dependencies conservative and purpose driven.

Add or retain a dependency when it removes meaningful risk or complexity for a
core concern, such as SQLite persistence, typed TOML parsing, or typed JSON
parsing. Prefer the standard library and existing local modules for command
execution, terminal control, UI rendering, and small utilities.

External command-line tools remain explicit runtime prerequisites or configured
tool paths. Do not hide required tools behind implicit downloads or background
installation.

## Consequences

- New crates need a clear reason tied to correctness, maintainability, or a
  capability Prism should not implement itself.
- Avoid broad frameworks for narrow problems, especially in process execution,
  terminal rendering, or configuration glue.
- Prefer typed parsers over ad hoc string parsing for structured formats.
- Dependency changes should be reviewed alongside product behavior, build impact,
  and test coverage.
