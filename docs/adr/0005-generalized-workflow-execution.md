# ADR 0005: Generalized Workflow Execution

## Status

Accepted

## Context

Plan Mode and Auto Flow encoded orchestration as separate run kinds and compiled
pipelines. Custom ordering, conditional or parallel work, durable human
decisions, scheduled provider intake, and future non-local execution require one
stable model without turning workflow source into an unrestricted scripting
runtime.

## Decision

Prism represents planning, coding, triage, and stabilization as versioned
Workflow Definitions resolved into immutable run snapshots. Ordered source is
shorthand for an acyclic dependency graph; bounded retries, fan-out, and child
runs provide controlled repetition. Durable scheduling and recovery operate on
Step Attempts, while typed Artifacts carry revisions, lineage, trust provenance,
and evidence between Steps.

Execution Targets enforce or disclose capability grants. Protected Git,
provider, Worktrunk, secret, and child-run effects pass through brokered,
intent-first implementations instead of being hidden inside prompts or generic
commands. Plan-oriented and end-to-end coding behavior is delivered as bundled
definitions and components, not engine-level run kinds.

The initial coordinator and Execution Targets remain local. Their contracts and
identities are target-neutral so adding remote execution does not redefine
workflow semantics.

This supersedes only the Plan-mode tmux clause in ADR 0001 and generalizes the
Plan/Auto-specific run persistence wording in ADR 0002. Tmux remains the sole
interactive Agent Session runtime, and Harness capabilities remain explicit.

## Consequences

- Workflow customization does not require adding compiled run or Step kinds.
- Waiting Gates, Approval Requests, and Triggers do not occupy execution slots.
- Definition snapshots and brokered effects add durable audit and reconciliation
  boundaries that ad hoc agent or command execution cannot bypass.
- Arbitrary cyclic workflow graphs, live reinterpretation of paused runs, and
  AI-generated authority are deliberately excluded.
