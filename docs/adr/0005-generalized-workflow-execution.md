# ADR 0005: Generalized Workflow Execution

## Status

Superseded by ADR 0006, Trigger-Driven Prompt Workflows.

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

The kernel owns exactly six Step classes: Action, Gate, Approval, Wait,
Notification, and Workflow Call. Reusable orchestration is a child Workflow
Definition, not a separate Workflow Component. Step Implementations are
replaceable executable extensions using a language-neutral, versioned JSON Lines
protocol. They run with the user's full OS authority; declared capabilities are
disclosure and policy inputs, not a sandbox boundary.

Standard protected Git, provider, Worktrunk, secret, and child-run effects use
brokered, intent-first host operations instead of being hidden inside prompts or
generic commands. An arbitrary extension can bypass those operations, so its
direct effects are labeled unbrokered and receive no fencing, intent-first, or
reconciliation guarantee. Plan-oriented and end-to-end coding behavior is
delivered as ordinary user-owned Workflow Definitions and extensions in the
Standard Pack, not engine-level run kinds or privileged resources.

The initial coordinator, Worker, and Execution Targets remain local. Their
contracts and identities are target-neutral so adding remote execution does not
redefine workflow semantics. Installed package resources are editable working
copies; runs pin immutable, content-addressed definitions, dependencies, schemas,
templates, package revisions, and extension executables.

This supersedes only the Plan-mode tmux clause in ADR 0001 and replaces the
Plan/Auto-specific run persistence wording in ADR 0002. Tmux remains the sole
interactive Agent Session runtime, and Harness capabilities remain explicit.
Because Prism is alpha, Plan Mode and Auto Flow commands, configuration, schemas,
and history are deleted without migration or compatibility readers.

## Consequences

- Workflow customization does not require adding compiled run or Step kinds.
- Waiting Gates, Approval Requests, and Triggers do not occupy execution slots.
- Definition snapshots and Standard brokered effects add durable audit and
  reconciliation boundaries; Prism does not extend those guarantees to direct
  effects performed by full-trust extensions.
- The Standard Pack uses the same public extension protocol and Rust SDK as a
  third-party package; there is no privileged implementation registry.
- Rust is the initial extension SDK. TypeScript and Wasm transports are deferred,
  and native dynamic-library plugins are rejected.
- Installed-resource changes affect future launches while retained runs remain
  executable from their pinned immutable closure.
- Arbitrary cyclic workflow graphs, live reinterpretation of paused runs,
  AI-generated authority, and legacy Plan/Auto history conversion are
  deliberately excluded.
