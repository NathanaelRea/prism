# Product Foundations

## Purpose And Scope

- **Behavior**: Prism is a terminal board and local automation fabric for
  managing coding work across multiple Git repositories, provider work items,
  worktrees, change requests, and agent-backed sessions without requiring the
  user to leave the keyboard-driven workflow.
- **Behavior**: Prism supports the lifecycle from provider intake and triage
  through planning, implementation, local verification, human decisions,
  change-request stabilization, merge readiness, and worktree cleanup.
- **Behavior**: Prism can start from any directory; behavior is based on tracked
  repository roots rather than the process working directory.
- **Default**: An installation with no tracked repositories opens normally.
  Repository setup remains available in the application and is not forced at
  startup.

## Product Invariants

- **Invariant**: Prism state, caches, immutable Workflow snapshots, and run
  history live under the user's Prism configuration location, not in managed
  repositories. A repository may contain explicitly trusted Workflow and
  Trigger files, but never Prism's mutable execution state.
- **Invariant**: The Default Branch remains a Worktree Session but is not treated
  as a task branch or Agent Session target from the worktree list. It sorts ahead
  of task worktrees and is not polled for or decorated with change-request state.
- **Invariant**: A logical prompt-starting action creates at most one intended
  harness session and submits its prompt exactly once through an adapter-supported transport.
- **Invariant**: Repository, Provider Item, Worktree Session, Agent Session,
  Workflow Run, Step Trigger, and Change Request Cache identities remain
  isolated. Names, branches, paths, and provider labels may be reused, but state
  from an old or concurrent identity must not appear on another one.
- **Invariant**: Refreshing remote state converges on the current provider state
  and does not replace known state with a transient false absence.
- **Invariant**: Destructive confirmations and warnings describe what the action
  will actually preserve or remove.

## Quality Attributes

- **Quality**: The board remains responsive while Git, provider, Worktrunk, tmux,
  database, Trigger, Workflow Run, and cleanup operations run. Slow external
  operations must not freeze interaction or visibly scroll/displace the board.
- **Quality**: Rendering remains aligned and usable across terminal sizes,
  ordinary Unicode and Nerd Font glyphs, Linux, and macOS terminals including
  Ghostty. State changes must not alter row height or column alignment.
- **Quality**: Invisible views and inactive repositories do not perform needless
  presentation refresh work. Hidden worktrees perform no automatic
  worktree-scoped provider polling; enabled Triggers remain governed by their
  own visible schedules and policies.
- **Quality**: Failures are logged with enough context to diagnose the failed
  operation. User-visible status is actionable, temporary, and single-line where
  it could otherwise disturb layout.
- **Quality**: Error states distinguish integration or refresh failures from
  legitimate absence, such as a branch having no pull request.
- **Quality**: Critical Prism/tmux/harness launch and prompt-delivery paths have
  deterministic integration coverage on supported platforms. Tests and demos
  must not read, mutate, or delete the user's real Prism state.
- **Quality**: Every status has a non-color cue, every action is keyboard
  reachable, and focused controls remain visually distinguishable.
- **Quality**: Supported operating systems, terminal capabilities, minimum
  terminal dimensions, and external-tool versions are documented as a testable
  support matrix. Unsupported environments fail clearly rather than degrading
  destructively.
- **Invariant**: Persisted prompts, agent output, credentials, and repository
  state use owner-only access where the platform supports it. Diagnostics redact
  prompt bodies, credentials, tokens, passwords, and secret-bearing arguments.
- **Invariant**: Untracking a repository, losing a repository path, archiving a
  worktree, and deleting a Worktree Session remove those identities from active
  use without silently deleting historical Workflow Runs, lifecycle Attempts,
  Trigger decisions, or Agent final text. Worktree deletion is not itself
  history deletion; independent
  retention cleanup applies only to eligible archived records. The alpha
  prompt-Workflow cutover backs up and replaces incompatible generalized
  Workflow state without importing or converting it.

## Technology Boundaries

- **Constraint**: Linux and macOS are the supported operating systems. Platform
  policy is deterministic for explicit Linux/macOS facts, while native syscall
  and integration behavior is verified on the corresponding host. Unsupported
  targets fail explicitly and do not use partial fallback behavior.
- **Constraint**: Prism is a Rust terminal application using Ratatui and
  Crossterm for terminal lifecycle, typed events, layout, and rendering.
- **Constraint**: Tmux is the sole interactive Agent Session runtime. Agent
  sessions provide an agent window, lazygit window, and shell window in the same
  worktree.
- **Constraint**: Managed Workflow Agent phases are supervised headless harness
  processes rather than tmux-owned interactive sessions. Durable Workflow
  identity, phase state, and history remain independent of one Worker process.
- **Constraint**: The installed Worktrunk executable owns physical worktree path
  policy, creation and removal effects, project hooks, approvals, stable
  templates, tethered processes, URLs, variables, columns, and hook logs.
  Prism integrates through typed machine-format subprocess operations, does not
  embed or fork Worktrunk, and must not depend on changing the caller's shell
  directory.
- **Constraint**: Prism supports Worktrunk 0.58.0 and newer and tests the real-tool
  compatibility path against Worktrunk 0.71.0 on Linux and macOS. Unsupported
  versions and machine-output schemas fail explicitly rather than appearing as
  absent environment state.
- **Invariant**: Git's live worktree inventory is authoritative for worktree
  existence and attached branches. Worktrunk observations decorate that
  inventory and cannot create or resurrect a Prism Worktree Session.
- **Constraint**: Harness activity and completion are obtained from supported
  adapter data, not by scraping rendered terminal lines.
- **Constraint**: Generic harnesses expose process liveness and bounded headless
  text only. Rich state is shown only when the selected adapter supports it.
- **Constraint**: External runtime tools remain explicit prerequisites or
  configured paths; Prism does not silently download them.
- **Constraint**: Dependencies remain conservative and purpose-driven. Structured
  formats use typed parsers, while broad frameworks are avoided for narrow
  process or configuration concerns.
