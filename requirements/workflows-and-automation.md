# Workflows And Automation

## Product Model

- **Behavior**: A Workflow is a prompt-first TOML file containing an optional
  `[defaults]` table and one or more `[[step]]` Agent prompts.
- **Invariant**: The Workflow filename stem is its default identity. Source does
  not require a schema version, qualified package ID, launch mode, capability
  declaration, typed port, Step class, implementation ID, or `skippable` flag.
- **Behavior**: A plain Step list is a linear sequence. The first Step is a root;
  each later Step without `depends_on` depends on the previous listed Step.
  `depends_on = []` creates another root.
- **Customization**: Explicit unique `id` and `depends_on` fields can form any
  acyclic graph. A referenced Step and every Step selected by `context` must have
  an explicit ID.
- **Invariant**: Compilation rejects cycles, missing references, duplicate IDs,
  invalid context ancestry, unresolved Triggers, unsupported explicit Agent
  overrides, and unknown source fields. Diagnostics identify source locations.
- **Invariant**: Starting a run persists an immutable snapshot of source bytes,
  compiled dependencies, prompts, harness/model/variant selection, context
  selection, and external Trigger executable digests. Edits affect future runs
  only.

## Steps And Triggers

- **Behavior**: A Workflow Step is one Agent prompt with an optional Trigger.
  Harness, model, and variant can be inherited from Workflow defaults or
  overridden by the Step.
- **Invariant**: Every Agent lifecycle starts a fresh Agent Session. The ordinary
  Workflow path never resumes or shares a native session between Steps or
  repeated runs of one Step.
- **Invariant**: Prism sends authored prompt text unchanged unless `context`
  explicitly selects predecessor final messages. Selected messages are appended
  as labeled plain-text sections; Prism adds no serialized evidence blob,
  provider state, Trigger state, or required JSON-output instruction.
- **Behavior**: A Trigger decision is `Run`, `Satisfied`, `Wait`, or `Fail`. Every
  decision carries a bounded human-readable summary; `Wait` also carries the
  earliest wake time.
- **Invariant**: `should_run_step` is observational. `pre_step_run` and
  `post_step_run` may mutate and receive stable run, Step, and attempt identity.
  Prepared state is persisted before Agent start, remains opaque to Workflow
  source, and is never included in the prompt.
- **Invariant**: `post_step_run` runs only after a successful Agent settlement
  and receives status, session identity, and final text. Agent success does not
  require an application JSON shape.
- **Behavior**: A Step without a Trigger runs once after dependencies are
  satisfied. A triggered Step may run again after later observations invalidate
  the current cycle.
- **Invariant**: A Step may omit `prompt` only for a check-only Trigger that can
  never return `Run`. A runtime `Run` decision for a promptless Step fails.
- **Constraint**: External Triggers are executable files with shebangs. Prism
  invokes one bounded process per phase using one versioned request on stdin and
  one bounded response on stdout, with bounded stderr for diagnostics.
- **Invariant**: External Triggers have the user's full OS authority. Prism pins
  executable bytes for active runs but does not claim to sandbox or pace direct
  network calls and subprocesses made by arbitrary Trigger code.

## Repeated DAG Evaluation

- **Behavior**: The worker evaluates eligible Steps in deterministic topological
  order. A dependent becomes eligible only when every triggered dependency is
  satisfied in the current cycle or every unconditional dependency completed
  earlier in the run.
- **Behavior**: Independent branches can continue while another branch waits.
  Agent execution and mutating hooks sharing one worktree are serialized.
- **Invariant**: After a successful Agent/post-Step lifecycle, transient Trigger
  observations are invalidated and a new cycle starts at the roots.
- **Invariant**: A wake also starts a new cycle at the roots, allowing an earlier
  Trigger to react while a later Step was waiting.
- **Invariant**: A run succeeds only when one full cycle has all triggered Steps
  `Satisfied` and all unconditional Steps completed. Individually stale
  observations can never be assembled into false success.
- **Behavior**: `Wait` persists the reason and wake time without occupying an
  Agent slot. TUI closure and worker restart do not lose the wake.
- **Invariant**: Every Agent start, including a failed start or manual retry,
  consumes the run's persisted `max_agent_runs` budget. Trigger checks, queue
  waits, and hook-only checks do not. Exhaustion produces `needs_input`.

## Persistence, Recovery, And Control

- **Constraint**: One on-demand per-user Prism Worker owns Workflow scheduling
  independently of the TUI.
- **Invariant**: The compact ledger persists Workflow snapshots, pinned Trigger
  revisions, runs and Agent budgets, compiled Steps and edges, Trigger
  decisions/summaries/wakes, lifecycle attempts/phases, prepared state, Agent
  process/session/final text, and concise events.
- **Invariant**: Claimed phases use renewable leases and monotonically increasing
  fencing. A stale owner cannot commit a phase result or begin another effect.
- **Invariant**: A worktree mutation claim serializes Agents and mutating hooks.
  A lost claim with uncertain effects becomes `recovery_required` until safely
  reconciled.
- **Behavior**: A crash after prepared state is persisted resumes at the Agent
  phase. A repeatable standard prepare/finalize hook may reconcile and resume;
  an uncertain custom hook is not blindly repeated.
- **Behavior**: Runs expose queued, running, waiting, needs-input, paused,
  succeeded, failed, cancelled, and recovery-required outcomes. One visible Step
  row exposes checking, preparing, running Agent, finalizing, and waiting phases.
- **Behavior**: Users can pause, resume, cancel, and retry a failed or explicitly
  recovered run. Retry appends an Attempt and does not erase prior history or
  restore consumed Agent budget.
- **Invariant**: Cancelling supervises owned processes and records a known
  boundary. Completion is never reported while an Agent or hook outcome remains
  uncertain.
- **Invariant**: Incompatible generalized `workflow.db` state is backed up once
  and replaced by the prompt-Workflow schema epoch. Old definitions and history
  are not imported or reinterpreted, and both engines are not retained behind a
  feature flag.

## Discovery And Editable Defaults

- **Behavior**: Prism discovers user Workflows and Triggers under
  `$XDG_CONFIG_HOME/prism/{workflows,triggers}` and trusted repository resources
  under `.prism/{workflows,triggers}`. Installed packages use the same
  conventional directories.
- **Behavior**: Repository resources take precedence over user resources with
  the same filename identity. List and edit surfaces show source provenance.
- **Invariant**: Repository-owned executable resources are not used before
  repository trust. Active runs retain content-addressed source and executable
  bytes.
- **Default**: First setup creates the user workflow directory and copies
  `stabilize.toml`. A single setup marker means editing or deleting that file is
  never silently undone.
- **Invariant**: Prism never overwrites an existing Workflow. Later examples are
  copied only by an explicit, previewed user command.
- **Behavior**: A non-default `multi-model-review` example demonstrates parallel
  roots, explicit dependencies, different models/variants, predecessor context,
  and one implementation join.

## Shared Remote Coordination

- **Constraint**: All Prism-owned provider observations and mutations cross one
  Worker-owned Remote Request Coordinator. Callers do not implement provider
  rate-limit parsing, retries, cache coalescing, or polling timers.
- **Behavior**: The coordinator maintains one lane per canonical provider host
  and credential profile, starts with one operation in flight per lane, persists
  cooldowns, respects `Retry-After`, applies bounded backoff with jitter, and
  coalesces equivalent reads by subject revision and freshness.
- **Behavior**: Interactive mutations outrank active Workflow hooks, Workflow
  observations, and background refreshes. Aging prevents starvation.
- **Invariant**: Queue length, retries, response bytes, pagination, and
  observation age are bounded. Waiting for queue capacity or fresh evidence is a
  visible Trigger `Wait` and consumes no Agent slot.
- **Constraint**: Agent-issued provider CLIs and direct network activity inside
  full-trust custom Triggers are outside the coordinator.

## Standard Stabilization Workflow

- **Default**: `stabilize` contains four linear triggered Steps:
  `merge_conflict`, `needs_review`, `ci_failure`, and check-only
  `ready_to_merge`. It remains below 50 non-comment source lines.
- **Behavior**: `merge_conflict` runs when the exact head is behind or
  conflicting and prepares a structured base merge. Expected conflict markers
  are Agent input state, not a hook crash.
- **Behavior**: `needs_review` captures exact unresolved review thread IDs before
  the Agent and resolves only that captured set after successful settlement.
- **Behavior**: `ci_failure` runs for failed required checks and waits for queued
  or pending checks without consuming an Agent slot.
- **Invariant**: `ready_to_merge` never runs an Agent and is satisfied only by
  fresh exact-head CI, review, policy, mergeability, and branch-relation facts.
- **Invariant**: Stabilization stops when the selected Change Request is ready to
  merge. It does not merge, clean up the worktree, infer addressed thread IDs
  from Agent JSON, or automatically commit/push beyond authored prompts and
  Trigger hooks.

## Non-Goals

- Arbitrary cyclic source graphs; shared Agent Sessions; typed Artifact plumbing
  between ordinary Steps; child Workflow runtime classes; Approval, Gate,
  Notification, or Workflow Call Step classes; package closure and capability
  envelopes in the Workflow kernel; automatic merge/cleanup; remote execution
  targets; schedule/provider Launchers; and migration of generalized Workflow
  history are not requirements for this cutover.
