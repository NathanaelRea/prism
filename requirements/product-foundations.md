# Product Foundations

## Purpose And Scope

- **Behavior**: Prism is a terminal board for managing coding work across
  multiple Git repositories, worktrees, pull requests, and agent-backed sessions
  without requiring the user to leave the keyboard-driven workflow.
- **Behavior**: Prism supports the lifecycle from planning and implementation
  through local verification, pull-request stabilization, merge readiness, and
  worktree cleanup.
- **Behavior**: Prism can start from any directory; behavior is based on tracked
  repository roots rather than the process working directory.
- **Default**: An installation with no tracked repositories opens normally.
  Repository setup remains available in the application and is not forced at
  startup.

## Product Invariants

- **Invariant**: Prism state, caches, and run history live under the user's Prism
  configuration location, not in managed repositories, unless a feature
  explicitly creates a repository-owned artifact.
- **Invariant**: The Default Branch remains a Worktree Session but is not treated
  as a task branch or Agent Session target from the worktree list. It sorts ahead
  of task worktrees and is not polled for or decorated with pull-request state.
- **Invariant**: A logical prompt-starting action creates at most one intended
  OpenCode session and submits its prompt exactly once.
- **Invariant**: Repository, Worktree Session, Agent Session, Plan run, Auto Flow
  run, and PR cache identities remain isolated. Branch names and paths may be
  reused, but state from an old or concurrent identity must not appear on another
  one.
- **Invariant**: Refreshing remote state converges on the current GitHub state and
  does not replace known state with a transient false absence.
- **Invariant**: Destructive confirmations and warnings describe what the action
  will actually preserve or remove.

## Quality Attributes

- **Quality**: The board remains responsive while Git, GitHub, Worktrunk, tmux,
  database, and cleanup operations run. Slow external operations must not freeze
  interaction or visibly scroll/displace the board.
- **Quality**: Rendering remains aligned and usable across terminal sizes,
  ordinary Unicode and Nerd Font glyphs, Linux, and macOS terminals including
  Ghostty. State changes must not alter row height or column alignment.
- **Quality**: Invisible views and inactive repositories do not perform needless
  refresh work. Hidden worktrees perform no automatic GitHub polling.
- **Quality**: Failures are logged with enough context to diagnose the failed
  operation. User-visible status is actionable, temporary, and single-line where
  it could otherwise disturb layout.
- **Quality**: Error states distinguish integration or refresh failures from
  legitimate absence, such as a branch having no pull request.
- **Quality**: Critical Prism/tmux/OpenCode launch and prompt-delivery paths have
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
  use without silently deleting historical Plan or Auto Flow runs. Worktree
  deletion is not itself history deletion; independent retention cleanup applies
  only to eligible archived records, and database migrations preserve retained
  history.

## Technology Boundaries

- **Constraint**: Prism is a Rust terminal application using Ratatui and
  Crossterm for terminal lifecycle, typed events, layout, and rendering.
- **Constraint**: Tmux is the sole interactive Agent Session runtime. Agent
  sessions provide an agent window, lazygit window, and shell window in the same
  worktree.
- **Constraint**: Worktrunk provides worktree creation and its configured project
  hooks; Prism must not depend on changing the caller's shell directory.
- **Constraint**: OpenCode activity and completion are obtained from OpenCode's
  native session data, not by scraping rendered terminal lines.
- **Constraint**: OpenCode is the only supported observable agent backend.
  Configuring an unsupported default backend fails clearly rather than silently
  losing session status, messages, or completion data.
- **Constraint**: External runtime tools remain explicit prerequisites or
  configured paths; Prism does not silently download them.
- **Constraint**: Dependencies remain conservative and purpose-driven. Structured
  formats use typed parsers, while broad frameworks are avoided for narrow
  process or configuration concerns.
