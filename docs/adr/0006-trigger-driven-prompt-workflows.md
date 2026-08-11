# ADR 0006: Trigger-Driven Prompt Workflows

## Status

Accepted

Supersedes ADR 0005, Generalized Workflow Execution.

## Context

ADR 0005 generalized Prism workflows around six Step classes, typed Artifact
ports, package-qualified implementations, child runs, capability envelopes, and
a long-lived extension supervisor. That model made the common stabilization
workflow difficult to author and caused Agent prompts, provider observations,
and orchestration mechanics to become coupled.

The product needs a smaller source interface without giving up durable waits,
restart safety, fresh Agent Sessions, or acyclic graphs. It also needs one
user-wide owner for Prism-issued provider requests so a TUI refresh and several
Workflow Runs do not independently poll the same provider state.

## Decision

A Workflow is a prompt-first TOML file whose filename is its default identity.
It contains optional Agent defaults and a `[[step]]` list. A list is linear by
default; explicit `id` and `depends_on` fields form an arbitrary acyclic graph.
The compiled, content-addressed run snapshot pins authored initial prompts and
follow-ups, typed input declarations and canonical bound values, selected
harness/model/variant values, dependencies, context selections, and external
Trigger executable bytes.

A Step is an Agent lifecycle with an optional **Trigger** lifecycle adapter. It
starts one fresh Agent Session and may submit ordered authored follow-up turns to
that session. A Workflow may declare file, string, boolean, number, and enum
inputs with optional typed defaults and substitute their canonical text through
`{{name}}` in Agent turns. File inputs are constrained by relative globs and
resolve to normalized relative paths, never implicit contents. The Trigger
observes whether its Step should `Run`, is `Satisfied`,
should `Wait`,
or must `Fail`. It may prepare state before an Agent and reconcile work after a
successful Agent. The prepared state is opaque and is never inserted into the
prompt. Check-only Triggers may omit a prompt and must never return `Run`.

The graph remains acyclic. Repetition comes from evaluation cycles: after a
successful Agent lifecycle or a durable wake, transient Trigger observations are
invalidated and evaluation restarts at the roots. A run completes only when one
whole cycle has every triggered Step satisfied and every unconditional Step
completed. Every lifecycle attempt starts a fresh native session and consumes
one persisted run budget unit; its authored follow-ups consume no additional
unit and never continue another lifecycle's session.

Built-in and fake Triggers implement one in-process `StepTrigger` interface.
External Triggers are full-trust shebang executables invoked once per lifecycle
phase with one versioned JSON request on stdin and one bounded response on
stdout. They have the user's OS authority. Prism pins their bytes but does not
claim to sandbox direct subprocess or network activity.

The per-user Prism Worker owns the compact Workflow ledger, lifecycle phase
leases/fencing, process supervision, worktree mutation claims, durable wakes,
and a shared remote request coordinator. Every Prism-owned provider observation
or mutation crosses that coordinator. A **Launcher**, not a Trigger, is any
future module that creates runs from schedules or provider events.

Default workflows are editable files copied to the user's workflow directory
once. A setup marker prevents Prism from resurrecting a deleted default, and
Prism never silently overwrites an existing workflow. Incompatible generalized
Workflow databases are backed up once and replaced without importing old runs
or interpreting old sources.

## Consequences

- The normal source contract has no schema version, package-qualified ID,
  launch mode, typed ports, Step class, implementation ID, capability list,
  condition, `skippable`, child run, approval, or Artifact coordination.
- Agent prompts and follow-ups are sent as authored except for declared typed
  input substitution. Only the initial prompt may receive explicitly selected
  predecessor final messages as labeled plain-text sections; no evidence JSON,
  implicit file contents, or required structured result is added.
- Trigger checks and provider queue waits consume no Agent slot. A continually
  runnable Trigger ends in `needs_input` when its Agent budget is exhausted.
- Persisting prepared state before Agent start permits restart at the Agent
  phase. Completed turns are persisted so a restart between turns can resume the
  next follow-up; an interrupted turn or uncertain non-repeatable external hook
  becomes `recovery_required` instead of being repeated blindly.
- The old generalized extension protocol, Standard Pack semantics, package
  closure, and TUI hierarchy can be deleted after callers move to this kernel;
  they are not compatibility contracts for new runs.
- Automatic merge, cleanup, remote execution targets, and schedule/provider
  Launchers are outside this decision.
