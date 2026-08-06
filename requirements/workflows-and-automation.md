# Workflows And Automation

## Product Model

- **Behavior**: Prism is an automation fabric for repository work. It can take
  work from provider intake and triage through planning, implementation,
  verification, human decisions, change-request stabilization, merge, and
  cleanup.
- **Behavior**: Users can define and run multiple named Workflow Definitions for
  a repository. Plan-oriented and end-to-end coding flows are bundled Workflow
  Definitions and components, not separate execution engines or run kinds.
- **Invariant**: A Workflow Definition is declarative, namespaced, typed, and
  versioned. It declares inputs, outputs, Steps, dependencies, conditions,
  policies, and required capabilities without embedding mutable run state.
- **Invariant**: Every Workflow Run has an identity independent of its trigger,
  repository, worktree, issue, change request, and definition name. Reused names
  or external identities cannot inherit another run's state.
- **Behavior**: A run may begin with one kind of input, such as an Issue, and
  produce additional typed Artifacts, such as a Plan, Worktree Session, Commit,
  Change Request, review report, or gate observation, without changing the
  identity of the original input.
- **Customization**: Users can select, parameterize, compose, copy, and replace
  bundled definitions and components. No built-in workflow order or prompt is a
  mandatory engine behavior.

## Definitions And Composition

- **Behavior**: An ordered Step list is concise authoring syntax for a dependency
  chain. Authors can also declare explicit dependencies, conditional branches,
  parallel fan-out, and joins; Prism resolves both forms to one acyclic graph.
- **Invariant**: Workflow graphs contain no arbitrary cycles. Repetition is
  expressed through a bounded retry policy, bounded fan-out, or a bounded
  Workflow Call, and every resulting attempt or child run remains visible in
  run history.
- **Invariant**: A retry repeats one logical operation against the same bound
  input revisions. A progressing iteration, such as repair followed by a new
  commit and Gate generation, is a bounded Workflow Call that consumes and emits
  explicit successor Artifacts rather than a retry with silently changed inputs.
- **Invariant**: Nested iterations inherit one persisted budget that descendants
  cannot reset. Definitions bound child depth, fan-out, total attempts, and
  mutation count; exhaustion produces a visible failed, input-required, or
  Approval outcome, never success.
- **Invariant**: Each Step has a stable definition-local identity, a primitive
  class, a named implementation, typed inputs and outputs, and declared
  capabilities. Dependencies refer to stable identities rather than list
  positions.
- **Invariant**: Conditions are side-effect-free expressions over declared run
  input and Artifact revisions and Step outcomes. Live provider or repository
  facts must first be captured as immutable observation Artifacts. Conditions
  cannot invoke a shell, network operation, agent, or other external evaluator.
- **Behavior**: Missing, stale, unavailable, and unknown condition inputs remain
  distinct. A definition must choose how those states branch or wait; Prism does
  not silently treat them as false, successful, or absent.
- **Behavior**: Before a run is accepted, Prism validates its resolved graph,
  references, input and output types, conditions, bounds, implementation
  availability, target requirements, capability policy, and reachable terminal
  outcomes. Validation errors identify the definition and Step responsible.
- **Behavior**: Reusable Workflow Components and Step Implementations have
  versioned typed interfaces. Composition and parameter binding are explicit;
  Prism does not merge definitions through positional or deep configuration
  inheritance.
- **Invariant**: Starting a run persists a fully resolved Definition Snapshot,
  including component and implementation revisions, prompt templates, bound
  parameters, conditions, retry and timeout policies, harness and model choices,
  declared capabilities, Admission Policy, execution-target requirements, and
  every reachable Workflow Call target revision and bound call policy.
- **Invariant**: Editing or deleting a source definition affects only future
  runs. Queued, paused, resumed, retried, and historical runs continue to use
  their Definition Snapshot unless the user explicitly starts a new run.

## Step Primitives

- **Constraint**: The workflow language has a small stable set of primitive Step
  classes: Action, Gate, Approval, Wait, Notification, and Workflow Call.
  Agent, command, Git, Worktrunk, and provider operations are typed Action Step
  implementations rather than new orchestration semantics.
- **Behavior**: An Action Step performs bounded work and emits typed Artifacts or
  Step outcomes. Normalized facts are payloads in immutable Artifacts rather than
  a separate mutable data channel. Built-in and custom Action implementations
  use the same attempt, capability, cancellation, timeout, and audit contract.
- **Behavior**: An Agent Action invokes one recorded harness, model, prompt
  template, and tool policy with explicit Artifact inputs. Each attempt is
  isolated by default; a definition may explicitly resume a recorded compatible
  native session when the Harness supports that capability.
- **Invariant**: Prism never silently substitutes shared Agent context when
  isolated execution was requested, or isolated execution when continuation was
  requested. Unsupported continuation fails validation or the Step explicitly.
- **Customization**: Agent Steps can select a Harness and model, use a bundled or
  custom prompt template, bind typed inputs, declare expected outputs, and set
  bounded retry, timeout, and continuation policies.
- **Behavior**: A Command Action receives structured arguments, input, working
  scope, environment references, timeout, and expected outputs. Untrusted values
  are not interpolated into shell syntax implicitly.
- **Invariant**: On a confined Execution Target, Agent and Command Actions can
  modify only their granted workspace scope directly. Provider writes, push,
  merge, Git-ref mutation, Worktrunk lifecycle effects, secret delegation, and
  child-run creation use brokered implementations that persist intent,
  preconditions, reconciliation identity, and resource claims before dispatch.
- **Invariant**: A Gate Step observes or evaluates evidence and has no mutating
  repair or merge authority. Repair, push, label, merge, and cleanup effects are
  separate Action Steps whose dependencies make the authorization visible.
- **Behavior**: An Approval Step records a human decision. A Wait Step waits for
  time or an external fact. A Notification Step reports an event and never
  blocks. None of those concepts is represented by implicitly pausing or
  resuming another Step.
- **Behavior**: A Workflow Call starts a child run with explicit inputs and
  a uniqueness key and returns declared outputs. Parent and child retain
  independent identity, attempts, controls, and history while exposing their
  lineage. The child receives its own Definition Snapshot from the call target
  revision pinned by the parent.
- **Invariant**: A child run starts quarantined or receives a recorded delegated
  capability grant bound to the parent's Authority Grant, call uniqueness key,
  pinned child definition revision, exact inputs, and transitive capability
  envelope. Parent authority may come from an Admission Decision, a
  capability-authorizing Approval Decision, or a trusted manual Invocation
  Grant. Delegation cannot exceed that authority; every child Step grant
  intersects it again with child policy, actor authority, secrets, and target
  enforcement. A child receiving externally controlled content also binds the
  exact Admission Decision or delegated admission authority for that content.
- **Behavior**: Preview and Approval Requests show reachable child definitions
  and their transitive capability envelopes. Workflow Calls declare whether
  pause and cancel controls apply only to the parent, to its non-detached child
  lineage, or not at all; the default stops new scheduling throughout the
  non-detached lineage.
- **Customization**: Users can define custom implementations within a primitive
  class through typed agent templates, commands, or supported extension
  adapters. An unknown implementation or undeclared required capability fails
  closed.

## Runs, Attempts, And Artifacts

- **Invariant**: Run inputs and Step outputs are immutable, typed, revisioned
  Artifacts with recorded producer and consumer lineage. Mutable external
  entities are represented by observations tied to their canonical identity and
  exact observed revision.
- **Invariant**: Every Artifact records source trust and sensitivity provenance.
  Derived, summarized, classified, planned, or agent-produced Artifacts retain
  the untrusted provenance of every input; transformation never makes external
  content authoritative.
- **Invariant**: Steps do not coordinate through an untyped mutable context.
  Producing a changed plan, commit, issue snapshot, change-request observation,
  or review report creates a new Artifact revision and preserves the prior one.
- **Invariant**: Every Step Attempt records its exact input revisions,
  implementation and policy revisions, Execution Target, resource claims,
  lifecycle state, start and terminal reason, bounded output, produced Artifacts,
  and external-effect intents.
- **Invariant**: Retrying appends a Step Attempt and never overwrites earlier
  output, evidence, decisions, or effects. Conditions and downstream Steps bind
  to a specific successful output revision rather than an ambiguous latest
  value.
- **Invariant**: Prism persists an external-effect intent and idempotency or
  reconciliation identity before attempting a provider, Git, Worktrunk, or other
  non-repeatable mutation. A lost final signal is reconciled against authoritative
  state rather than blindly repeating the effect.
- **Invariant**: Push and merge intents pin canonical target identity, expected
  pre-state, desired post-state, exact head, and relevant policy and Gate
  revisions. Reconciliation distinguishes exact result applied, not applied with
  preconditions intact, externally satisfied, superseded or diverged, and
  indeterminate; only not-applied with intact preconditions retries
  automatically.
- **Behavior**: A run exposes queued, active, waiting, input-required, paused,
  completed, failed, cancelled, and recovery-required states, together with the
  exact Steps that determine the aggregate state.
- **Invariant**: Step state uses pending, runnable, active, waiting, blocked,
  input-required, skipped, completed, failed, cancelled, and recovery-required.
  Gate results and Approval Decisions are typed results, while admitted and
  timed-out are recorded events or terminal reasons rather than additional run
  states.
- **Behavior**: Users can pause or cancel a run, retry a failed Step, and resume
  from a safe persisted boundary. Skipping or replacing an output is available
  only when the definition permits it and Prism explains which downstream
  decisions and Artifacts it invalidates.
- **Invariant**: Archiving or deleting a Worktree Session, untracking a
  repository, or losing a local checkout does not silently delete retained run,
  attempt, Artifact, approval, or trigger history.
- **Invariant**: A Workflow Run can link zero, one, or many Worktree Session
  incarnations without being owned by any of them. Deleting a linked session
  blocks or cancels only pending and active Attempts whose exact scope requires
  it; unrelated branches can continue, and history retains the retired link.

## Gates And Evidence

- **Behavior**: CI, provider review, mergeability, merge conflicts, policy,
  local verification, and security policy are independently addressable Gates.
  Their ordering and dependencies come from the Workflow Definition, not a
  compiled checklist.
- **Invariant**: A Gate result identifies the exact subject and generation it
  evaluated, the evidence and observation quality used, its policy revision, and
  whether it is waiting, satisfied, unsatisfied, unknown, or unavailable.
- **Invariant**: Stale, partial, failed, unknown, or mismatched-generation
  evidence cannot satisfy a Gate that authorizes a mutation. Unsupported
  provider capability remains distinct from a failed or absent result.
- **Behavior**: Gate timeout policy explicitly chooses failure, a conditional
  branch, or an Approval Step. Timeout never implies success unless the resolved
  definition contains a visible policy accepting that risk.
- **Behavior**: Independent Gates may wait or evaluate concurrently. A merge
  Action can depend on all required Gate results without forcing CI, review, and
  mergeability into an artificial total order.
- **Behavior**: Agent self-review, second-model review, and security analysis
  produce review-report Artifacts. A separate Gate can evaluate those reports;
  the report or model does not grant itself additional execution authority.
- **Invariant**: A review configured as blocking has a policy Gate bound to the
  exact report revision in merge-readiness dependencies. Action completion means
  only that a report was produced, not that its findings passed.
- **Behavior**: A bundled second-model review uses an isolated attempt and a
  model identity distinct from implementation and self-review. If a user chooses
  the same model, Prism describes it as an additional same-model review rather
  than claiming independent second-model evidence.
- **Customization**: Bundled Gate implementations provide default observation
  and prompt templates, policies, polling, deadlines, and evidence renderers.
  Users can change settings, replace templates, or define compatible custom
  implementations.

## Triggers And Intake

- **Invariant**: A Trigger is a durable rule that creates Workflow Runs; it is
  not a long-lived Step and does not occupy an execution slot while waiting.
- **Behavior**: Prism supports manual, scheduled, and provider-event Trigger
  contracts. The local product may implement provider events through polling,
  while preserving canonical event identity so a future webhook transport does
  not change workflow semantics.
- **Invariant**: A manual invocation creates a durable occurrence with its own
  identity, actor, input digest, and optional caller-supplied idempotency key.
  Within one Trigger, reusing a key with the same input digest returns the same
  occurrence and run, while reuse with different input is rejected. Invoking
  without a key intentionally creates a new occurrence and run.
- **Behavior**: A Trigger selects a pinned definition revision or an explicit
  floating definition selector, binds typed inputs and parameters, declares an
  Admission Policy, and records the source manual invocation, provider event, or
  schedule occurrence on every run it creates. A floating selector resolves once
  into each run's Definition Snapshot.
- **Invariant**: Trigger delivery is deduplicated by Trigger identity, occurrence
  identity including a manual caller key when present or canonical Provider Item,
  Observation Revision, resolved Workflow Definition, and admission purpose.
  Restarts and overlapping polls cannot create duplicate runs for that key
  unless an explicit repeat policy requests a new run.
- **Customization**: Scheduled Triggers declare timezone, missed-occurrence
  handling, overlap policy, concurrency bounds, and enablement. Provider Triggers
  declare repository, event kind, deterministic filters, polling or event
  checkpoint, and fan-out bounds.
- **Behavior**: Provider query Actions can page through Issues or other supported
  work items and emit one typed Artifact per item. Prism durably records every
  discovered occurrence before advancing its poll checkpoint. Labeling,
  assignment, comment, close, and branch effects are separate
  capability-checked Actions.
- **Invariant**: Provider issue and label behavior is capability-based and
  provider-neutral. GitHub may be the first implementation, but unsupported
  GitLab or Forgejo capabilities are reported rather than emulated or treated as
  empty results.
- **Behavior**: A scheduled triage workflow can find untriaged Issues, classify
  them, apply authorized labels, and start a child implementation workflow for
  admitted items without requiring a special scheduler or triage execution
  engine.
- **Invariant**: Every discovered Provider Item receives an independent durable
  Admission Decision before mutation or implementation. A child Workflow Call
  uses a uniqueness key containing canonical item identity, admitted Observation
  Revision, child definition revision, and admission purpose, so separate polls
  cannot launch duplicate implementation runs.
- **Customization**: When a new Observation Revision arrives for a Provider Item
  with active work, policy chooses coalesce, supersede, queue, or explicit
  parallel handling. The conservative default permits only one active
  implementation per canonical item and admission purpose.

## Admission And Security

- **Invariant**: Issue bodies, titles, comments, review text, CI logs, repository
  files, and other externally controlled content are untrusted data. They cannot
  define Steps, alter the Definition Snapshot, select credentials, expand the
  run's capabilities, or become shell syntax through ordinary templating.
- **Default**: A Trigger can create a quarantined intake run for an explicitly
  configured provider repository. The run can perform read-only discovery and
  constrained classification, but its capability grant cannot cross into a
  repository workspace, general tools, secrets, provider writes, child calls,
  or other meaningful local or remote mutation.
- **Behavior**: Admission unlocks a precisely scoped capability envelope within
  that existing run. A user can admit one quarantined item through an Approval
  Step or explicitly trust a deterministic Admission Policy for hands-off
  operation.
  Trust policy can bind provider host and repository, Workflow Definition,
  authenticated actor or repository relationship, labels and their provenance,
  event kind, requested capability envelope, and input revision.
- **Invariant**: Agent-produced security classification is advisory evidence and
  may make policy stricter, but cannot by itself admit external content or grant
  capabilities. Repository approval alone does not implicitly admit every actor,
  label, workflow, or mutation.
- **Invariant**: Before admission, only normalized provider-authenticated facts
  named by trusted policy can satisfy an admission condition, including canonical
  host and repository identity, authenticated actor relationship, event kind,
  allowlisted label identity and provenance, and Observation Revision. Free-form
  external content and agent-derived values cannot supply admission authority.
- **Invariant**: After admission, untrusted or derived values can select only
  subjects and effect values bounded by trusted policy, such as an allowlisted
  label mapping; they never select credentials, capabilities, definition
  revisions, or Execution Targets.
- **Invariant**: An Admission Decision records the exact Admission Policy
  revision, authenticated facts, Observation Revision, capability envelope,
  actor or automatic policy authority, outcome, and expiration used to cross the
  quarantine boundary.
- **Invariant**: A trusted manual Invocation Grant records local actor identity,
  exact Definition Snapshot and inputs, capability envelope, target trust, and
  expiration. It authorizes local work but does not admit externally controlled
  content that lacks an Admission Decision.
- **Invariant**: New externally controlled content requires a new Admission
  Decision before it reaches a workspace-capable Step. A brokered Prism mutation
  may carry authority to a successor Observation Revision only when
  reconciliation proves that solely pre-authorized fields changed according to
  the recorded effect intent.
- **Invariant**: A material change to quarantined external content invalidates a
  prior item approval. A change to a trusted Admission Policy applies only after
  explicit confirmation and is recorded for runs it admits.
- **Invariant**: Every definition and Step Implementation declares its capability
  envelope, including repository and filesystem access, process execution,
  network and provider reads or writes, Git mutation, push, merge, Worktrunk
  effects, secrets, and child-workflow creation. Policy can narrow but cannot
  expand that declaration.
- **Invariant**: A Step Attempt receives a recorded capability grant no broader
  than the intersection of its declaration, definition and Admission Policies,
  actor authority, secret scope, and Execution Target enforcement. Compatibility
  alone never grants a capability.
- **Behavior**: Repository-owned definitions require trust for their exact
  revision and declared capability envelope before use. A material definition
  or capability change invalidates that trust; merely viewing or validating the
  file does not execute it.
- **Constraint**: Once an admitted Agent Action is allowed, its Harness remains
  the command- and tool-level sandbox and approval authority inside the granted
  workspace. Prism records the selected Harness policy but does not treat prompt
  text, agent output, or a Gate result as permission to bypass it or invoke a
  protected brokered effect.
- **Behavior**: A target that cannot technically confine an Agent or Command to
  its declared grant is visibly unconfined. Its capability grant is disclosure,
  not technical enforcement, and trusting it admits the exact implementation's
  full effective OS-user authority. Quarantined or unattended externally sourced
  work cannot use an unconfined implementation unless an Admission Policy
  explicitly accepts that named risk.
- **Invariant**: Credentials are referenced through configured secret handles,
  scoped to the minimum provider and operation capabilities available, and never
  copied into Definition Snapshots, prompts, Artifacts, ordinary output, or
  diagnostics.

## Human Decisions And Notifications

- **Behavior**: An Approval Step creates a typed Approval Request for Artifact
  acceptance, capability authorization, human attestation, or exact mutation
  approval. The request presents the affected repository and subjects, relevant
  Artifact and evidence revisions, requested capability envelope, and
  consequences of approval or rejection.
- **Invariant**: An Approval Decision identifies the request and Step Attempt,
  decision, evidence digest, actor, time, optional reason, and expiration. It
  authorizes or attests only what the selected request mode and shown revisions
  describe; changed inputs or expired evidence invalidate it.
- **Behavior**: Plan acceptance binds an exact Plan revision and implementation
  capability envelope without claiming the future edits are already known.
  Human testing emits pass, fail, or unable evidence tied to an exact commit,
  instructions, environment, and expiration; a Gate decides whether that
  attestation satisfies merge policy.
- **Behavior**: Approval and rejection are explicit outcomes that can lead to
  different conditional branches. Resume is a run control and never doubles as
  approval.
- **Invariant**: Approval Requests, Wait Steps, and Notification Steps persist
  while no worker owns them. Process restarts and TUI closure do not lose or
  implicitly satisfy them.
- **Behavior**: Notifications can report input required, admitted, completed,
  failed, timed out, or recovery-required events through configured channels.
  Delivery is best effort and never changes a Gate, Approval, Step, or run
  outcome.
- **Constraint**: The local product records a local actor identity while keeping
  actor and decision records suitable for future authenticated server users.
  Multi-user roles, quorum, and delegation are not required for local execution.

## Execution And Recovery

- **Invariant**: Durable scheduling claims executable Step Attempts rather than
  whole runs. Gate polling sleeps, Approval Steps, Wait Steps, queued child work,
  and notifications waiting for delivery do not consume an active execution
  slot.
- **Constraint**: Claims use renewable leases and monotonically increasing
  fencing tokens. A stale owner cannot commit run state or dispatch a new effect
  after ownership is lost; an effect dispatched before lease loss remains
  potentially in flight and must be reconciled before another owner acts.
- **Behavior**: Execution policy supports global, repository, workflow,
  implementation, and Trigger concurrency bounds plus explicit resource claims
  for canonical repository identity, Worktree Session incarnation, fully
  qualified Git ref, Change Request, provider mutation lane, and user-defined
  exclusive resources.
- **Invariant**: An Attempt declares its complete claim set before execution and
  acquires it atomically or under a deterministic no-hold-and-wait order. Waiting
  for resources consumes no active execution slot. Mutable-resource reads
  conflict with writes; immutable commit or observation reads may run
  concurrently.
- **Invariant**: Conflicting mutation claims are serialized. A Step revalidates
  its resource, exact input revision, capability grant, and required Gates
  immediately before mutation.
- **Behavior**: A lost claim on a read-only, idempotent, or authoritatively
  reconcilable attempt can recover automatically according to its persisted
  policy. An attempt with uncertain non-repeatable effects becomes
  recovery-required until reconciliation or an explicit decision makes the next
  action safe.
- **Invariant**: Pause stops new scheduling and preserves active attempt facts;
  cancellation requests bounded termination where safe. Neither control reports
  completion before active effects and child runs reach a known state.
- **Invariant**: Losing a workspace-writing claim quarantines that Execution
  Workspace against all new read and write claims. Quarantine ends only after the
  target proves process termination and Prism captures or reconciles the
  resulting workspace generation; otherwise recovery uses an isolated workspace
  or an explicit human disposition.
- **Default**: Prism ships with one local coordinator and local Execution Targets.
  Closing the TUI does not stop managed work or Trigger evaluation while the
  local worker is running.
- **Constraint**: Run, Artifact, actor, claim, resource, and Execution Target
  identities do not depend on local absolute paths or one process's memory. The
  execution contract carries required capabilities, inputs, secret handles,
  heartbeats, cancellation, bounded output, and results so remote targets can be
  added without redefining workflow semantics.
- **Constraint**: A mutable Execution Workspace has its own target-neutral
  identity, canonical repository identity, exact base revision, target affinity,
  lease, and guarded result-promotion rules. It links a Worktree Session
  incarnation only when backed by that user-visible managed worktree. Execution
  policy binds Artifact sensitivity, secret and provider scope, and mutation
  authority to an explicitly trusted target identity or trust class, and
  verifies producer identity and result integrity before accepting outputs.
- **Constraint**: Remote runners, hosted coordination, transport, authentication,
  multi-user authorization, and server deployment are not required by the local
  product contract.

## Bundled Workflows

- **Behavior**: Prism ships versioned components for planning, plan approval,
  implementation, local verification, self-review, second-model review,
  change-request creation, review and CI observation, bounded repair, merge
  readiness, merge, and cleanup.
- **Behavior**: The bundled plan-oriented workflow can select or create a Plan
  Artifact, optionally request evidence-bound approval, and implement its phases
  sequentially or through explicit dependency-safe parallel child runs.
- **Behavior**: The bundled end-to-end coding workflow composes the same public
  components. Users can reorder independent Gates, insert security or human
  review, change models and prompts, remove optional Steps, alter policies, or
  replace the definition entirely.
- **Invariant**: A Plan is an Artifact consumed by Steps, not a nested execution
  engine. Its validated phase manifest has stable phase identities,
  dependencies, inputs, and bounds. Dynamic phase fan-out creates child runs
  from one Step Implementation revision pinned by the parent Definition Snapshot
  and never mutates the parent's graph.
- **Invariant**: Merge readiness is a Gate or composition of Gates; merge itself
  is a guarded Action. A definition cannot conceal a merge mutation inside a
  passive Gate implementation.
- **Default**: Bundled mutation and admission defaults remain conservative:
  externally sourced implementation is quarantined, uncertain evidence fails
  closed, managed repair pushes require explicit authorization, and automatic
  merge and cleanup are opt-in.

## Authoring And Operations

- **Customization**: Workflow, component, Step Implementation, Trigger, and
  Admission Policy definitions can live in the user's Prism configuration or in
  an explicitly trusted repository-owned Prism configuration location.
- **Invariant**: Built-in, global, and repository definitions have explicit
  namespaces and revisions. Name collisions never silently replace a referenced
  definition or rewrite a Trigger selector; an explicitly floating selector may
  resolve a newer revision only according to its recorded policy.
- **Behavior**: Prism publishes a schema and generates useful commented examples
  for definitions. Authoring feedback includes structural validation, type and
  reference errors, capability requirements, resolved defaults, and source
  locations.
- **Behavior**: Users can preview the fully resolved Definition Snapshot and a
  side-effect-free execution plan before launch. The preview explains selected
  implementations, dependencies, conditions, possible branches, concurrency,
  retries, timeouts, required trust, capabilities, and target compatibility.
- **Behavior**: The TUI focuses on selecting definitions, starting and observing
  runs, inspecting the resolved graph and Artifacts, satisfying Approvals,
  controlling execution, and navigating history. A visual workflow-definition
  editor is not required.
- **Behavior**: Run views explain why each Step is pending, runnable, active,
  waiting, skipped, blocked, input-required, failed, or completed, and show
  bounded attempt output without collapsing retries or child runs.
- **Behavior**: CLI, TUI, and future API surfaces use one control contract for
  launch, pause, resume, cancel, retry, approval, rejection, recovery, and
  history. Machine-readable list, status, validation, and preview output is
  versioned.
- **Quality**: Workflow history is sufficient to reconstruct what definition,
  inputs, evidence, authority, model, prompt, target, and external effects
  produced a result without requiring the original source files to remain.
- **Quality**: Trigger scans, Gate waits, and large run histories remain bounded
  and do not block TUI interaction. Provider pagination, backoff, rate limits,
  and queue pressure are visible and cannot be mistaken for an empty result.
- **Quality**: Deterministic contract tests cover graph validation, condition
  handling, definition snapshots, attempt recovery, effect reconciliation,
  admission, approval invalidation, provider capability gaps, and resource-lock
  conflicts without using real credentials or user state.
