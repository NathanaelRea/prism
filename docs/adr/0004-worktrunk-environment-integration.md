# ADR 0004: Worktrunk Environment Integration

## Status

Accepted

## Context

Prism creates Worktree Sessions through Worktrunk and displays selected values
from `wt list`. Those subprocess calls previously lived in lifecycle and TUI
code, which spread command syntax, output parsing, and compatibility behavior
across unrelated modules.

Prism needs Worktrunk's path policy, project hooks, approvals, stable template
values, tethered processes, and environment observations to remain consistent
with the user's standalone `wt` commands. Prism also has identity and workflow
state that Worktrunk does not own.

## Decision

The installed `wt` executable is the integration seam. `src/worktrunk.rs` is the
only production module that constructs Worktrunk commands, parses their machine
output, or classifies compatibility errors. Callers use typed requests,
outcomes, and failures. Each operation has a distinct static process descriptor
for observability.

We will not fork or vendor Worktrunk and will not add a Worktrunk Rust crate.
Using the executable ensures Prism and standalone `wt list` consume the same
user and project configuration. A backend trait is not introduced until a
second concrete adapter exists.

Git's live worktree inventory remains authoritative for physical existence and
the attached branch. Worktrunk owns physical path policy and lifecycle effects,
including hooks and approvals. Prism owns Tracked Repository identity,
Worktree Session identity and incarnation, Agent Sessions, pull request state,
and managed workflow history. Worktrunk observations can decorate a live Git
worktree but cannot create or resurrect a Prism Worktree Session.

## Consequences

- Worktrunk command and schema compatibility changes are isolated to one module.
- Lifecycle remains the coordinator and consumes typed Worktrunk operations.
- TUI jobs remain responsible for scheduling and responsiveness, not command
  construction or JSON interpretation.
- The external process boundary retains bounded output capture and avoids shell
  evaluation.
- A missing or unsupported Worktrunk capability fails explicitly; Prism does not
  silently fall back to a reduced Git-only implementation.
