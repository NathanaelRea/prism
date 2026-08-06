# Worktree And Agent Sessions

- **Invariant**: Each Worktree Session persists the harness associated with its tmux Agent Session, scoped by worktree path and incarnation so reused branches and paths cannot inherit it.
- **Behavior**: If that harness differs from the global default when opened, Prism offers `Migrate`, `Later`, and `Keep`. Migration retires the old tmux generation without deleting native session history; Later asks on the next open; Keep pins the old harness until explicit migration.

## Creation

- **Invariant**: Prism owns persistent Worktree Session identity and incarnation;
  Worktrunk owns physical worktree creation, path policy, hook execution, and
  approvals. Observed Worktrunk state is associated only by repository and exact
  normalized worktree path, never by branch-name fallback.
- **Behavior**: `c` creates a Worktree Session from the repositories panel using
  the currently selected repository.
- **Behavior**: Creation opens a dialog that identifies the target repository and
  accepts a branch name plus an optional multiline initial prompt.
- **Behavior**: Submission keeps the dialog responsive and shows progress until
  worktree creation and Agent Session startup complete.
- **Behavior**: Prism creates the worktree through Worktrunk without switching the
  caller's shell. If repository project commands require approval, Prism offers
  approval both when adding the repository and when a later creation attempt
  reveals new or changed commands.
- **Behavior**: A non-empty initial prompt is submitted to the interactive
  harness session only when its adapter has a reliable transport. An empty prompt opens the
  session without synthesizing or submitting text.
- **Behavior**: Successful creation attaches to the resulting tmux session so
  the user can inspect or edit the agent interaction immediately.
- **Behavior**: Worktrunk-configured development URLs and listening observations
  decorate a live Worktree Session. Prism opens known HTTP(S) URLs but does not
  own or infer the development process; long-running servers remain Worktrunk
  `post-start`/`tether` responsibilities.

## Agent Session Lifecycle

- **Behavior**: Enter on a Worktree Session attaches to its persistent tmux Agent
  Session. Prism reuses a healthy matching session and replaces a stale or
  incompatible runtime when necessary.
- **Invariant**: Prism associates native adapter session identity with each
  active Worktree Session. Worktrees using the same harness in one repository
  share one OpenCode server. Worktree deletion shuts that server down only when
  Prism owns it and no retained Worktree Session references it.
- **Default**: Prism allocates OpenCode servers from a deterministic range
  beginning at port 41000 and spanning 1000 ports. One server is allocated per
  repository and harness. Owned servers remain warm when Prism exits unless
  shutdown is enabled, in which case the shared owned server is terminated.
- **Invariant**: Active harness session association is resolved independently
  per worktree. Creating `/new` or changing activity in one worktree cannot
  retarget prompts or status in another.
- **Behavior**: When the selected harness supports observation, the main panel
  shows the selected worktree's current agent workflow status, latest user
  message, and up to five latest assistant messages. Harnesses without
  observation retain stable layout and explicitly degrade to process state.
- **Invariant**: Agent completion is derived from the final assistant message's
  completion timestamp. A newer user prompt invalidates prior completion; errors
  supplement rather than redefine completion.
- **Behavior**: Users can enter a selected Agent Action's corresponding native
  session in tmux when its recorded Harness supports interactive resume and the
  Workflow Definition explicitly permits continuation.

## Hiding And Deletion

- **Behavior**: Users can hide/archive a worktree without deleting its branch,
  files, dirty changes, or runtime history. Hidden worktrees sort below active
  worktrees and are excluded from automatic worktree-scoped provider polling.
- **Behavior**: Unarchive presents a discoverable picker; users do not need to
  remember the archived worktree name.
- **Behavior**: Deletion presents a confirmation and clearly highlights dirty
  state and other risks. On confirmation, Prism removes active session
  associations, the worktree, branch when applicable, tmux session, and other
  Prism-owned runtime resources as one coherent operation.
- **Invariant**: Deletion retires the Worktree Session identity and its run and
  Artifact links without deleting Workflow Runs, attempts, Artifacts, or
  decisions. A pending Attempt scoped to that exact incarnation cannot start; an
  active scoped Attempt must reach a cancelled or recovery-required boundary
  before physical deletion proceeds. Unrelated run branches can continue.
- **Invariant**: Reusing a deleted worktree path or branch name creates a fresh
  Worktree Session identity and cannot resurrect old Change Request, Workflow
  Run, or Agent Session state.
- **Behavior**: Deletion preflights risks, records progress, and is retryable.
  Partial failure reports which resources were removed or preserved and retains
  enough identity and history to retry without applying cleanup to a new session.
- **Invariant**: Permanent deletion delegates physical removal and pre/post
  removal hooks to Worktrunk with branch deletion disabled. Prism separately
  retains its expected-incarnation and expected-branch-OID guard before deleting
  the branch or retiring Prism-owned state.

## Worktrunk Evidence

- **Behavior**: Users can inspect a bounded, sanitized tail of Worktrunk hook
  output for a selected repository. Branch labels affect display preference
  only and never establish Worktree Session identity.
- **Invariant**: Hook-log presence is not evidence that a process is running or
  that a hook succeeded. Log bodies are not persisted in Prism state or copied
  into structured diagnostics; URL listening is the reachability observation.

## Process Safety

- **Behavior**: Quitting is immediate. Detached Agent Sessions and the per-user
  Prism Worker continue running; permanent deletion in progress blocks quitting.
- **Invariant**: Tmux hosts interactive Agent Sessions and terminal tools, not
  managed Workflow Step workers.
- **Behavior**: Long-running creation, deletion, Git, provider, tmux, Trigger,
  and Workflow Step operations expose progress, cancellation where safe, and an
  actionable retry path. Cancellation and restart do not silently duplicate
  external side effects.
