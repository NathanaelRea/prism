# Plans And Auto Flow

## Plan Mode

- **Behavior**: Plan Mode selects a Markdown plan, infers its numbered phases,
  and executes those phases through OpenCode while exposing current status and
  useful execution detail in the selected worktree's main panel.
- **Behavior**: Plan progress shows the current phase, per-step status, and short
  bounded log snippets rather than an unbounded output tail.
- **Invariant**: Every Plan run has independent persisted identity, phase state,
  output, history, selected OpenCode variant, and completion state. Multiple runs
  may coexist for one worktree without inheriting one another's state.
- **Behavior**: Interrupted Plan runs can resume after Prism restarts. Users can
  pause, resume, dismiss/archive/delete, and move among historical runs, then
  return the main panel to normal worktree content.
- **Behavior**: Standalone Plan Mode remains available separately from Auto Flow.
  Its dashboard supports pause/resume, retry, skip, abort, and entering the
  selected phase's OpenCode session.
- **Behavior**: Starting a Plan asks `Run steps in parallel? [y/N]:`.
- **Default**: Plan steps run sequentially unless the user opts into parallel
  execution. OpenCode's `medium` agent variant is used unless another variant is
  selected.
- **Behavior**: Opting into parallel execution declares that the selected plan
  was authored with independent, parallel-safe phases. Prism does not infer or
  validate phase dependencies; when Prism asks an agent to author a parallel
  plan, the prompt requires phases suitable for concurrent execution.
- **Behavior**: Interactive Plan discovery offers Markdown files under the
  selected scope, excluding Git metadata, and supports selecting the inferred
  phase range. Persisted output is bounded, while dismissed runs remain
  available as history until retention cleanup applies.

## Auto Flow

- **Invariant**: A new Auto Flow starts only for a clean, attached, non-default
  Worktree Session.
- **Behavior**: Auto Flow can start implementation from a prompt, an existing
  plan, or a drafted plan that pauses for approval before execution.
- **Behavior**: Auto Flow persists its ordered attempts and can resume after
  interruption. Retries remain auditable and never overwrite prior attempt
  output.
- **Behavior**: The dashboard reflects the actual workflow as a nested checklist,
  including linked Plan phases and real validation or repair loops, rather than a
  fixed aspirational checklist.
- **Behavior**: Before an automatically managed step that requires approval,
  Prism explains the pending action and visibly identifies the affected
  worktree.
- **Behavior**: If work completed but its final signal was lost, the user can
  mark or advance the step without rerunning completed implementation.
- **Invariant**: Plan-backed Auto Flow links to one persisted Plan run instead of
  duplicating its phases as Auto Flow steps.
- **Behavior**: After implementation and local verification, Auto Flow delegates
  pull-request gate decisions to PR Stabilization and proceeds toward the
  configured merge-readiness goal.
- **Default**: Auto Flow may push the initial implementation and create its pull
  request, but managed repair commits remain local behind a guarded pending push.
- **Default**: Automatic merge and automatic post-merge cleanup are disabled.
  Review and CI waiting are enabled, while approving review is not required
  unless repository policy or configuration requires it.
- **Invariant**: Draft-plan mode never overwrites an existing `plan.md`; an
  existing plan must be selected explicitly and contain recognizable numbered
  phases.
- **Invariant**: Auto Flow persists an attempt before its external side effect,
  bounds local-verification, review-repair, and CI-repair loops, and resumes from
  persisted boundaries rather than discarding prior attempts.
- **Customization**: Review and CI waiting can be disabled and their polling
  intervals and maximum waits configured. Review timeout continuation is
  separately configurable.
