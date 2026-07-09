# Prism Context

Prism is a terminal board for running agent-backed coding sessions across Git
worktrees. This file defines the project language reviewers should use when
discussing behavior, code, and docs.

## Domain Terms

### Repository

A repository is a Git working tree root discovered with `git rev-parse
--show-toplevel`. Prism treats the repository root as the source of branches,
worktrees, GitHub pull request state, and per-repository Prism state.

Per-repository Prism state is stored under the user Prism config directory, not
inside the repository root. The path is derived from the repository name and a
stable hash of the root path.

### Tracked Repository

A tracked repository is a repository listed in `~/.config/prism/repos.toml`.
Tracked repositories appear in the repos panel, keep their configured order, and
may have a single-character key used by `Space <key>` shortcuts.

Adding a repository through `--repo <path>` or the TUI discovers the Git root and
adds it to `repos.toml` if it is not already tracked. Removing a repository from
`repos.toml` stops Prism from tracking it; it does not delete the repository.

### Worktree Session

A worktree session is a Git worktree shown in the worktrees panel for a tracked
repository. Prism discovers sessions from `git worktree list --porcelain`.

The default branch worktree is still a session, but Prism treats it specially:
it sorts first and does not show pull request cache data for that branch.
Non-default worktree sessions usually represent active task branches.

Prism may attach metadata to a session, including prompt summary, agent state,
logs, hidden markers, and pull request cache data. That metadata is keyed by
repository and branch.

The Worktree Session module owns session identity, default-branch
classification, branch metadata facts, background-safe snapshots, and deletion
warnings. It may carry Agent Session and PR Cache facts for callers, but it
should not own tmux lifecycle behavior or GitHub refresh semantics.

### Agent Session

An agent session is a persistent tmux session for a worktree session. The agent
window runs the configured interactive agent command, and companion windows
provide lazygit and a shell in the same worktree.

Agent session names are derived from a stable repository hash, a safe branch
name, and a generation number. Prism can reattach to an existing agent session,
create one when needed, or replace one that is not running the expected agent.

The Agent Session module owns lifecycle decisions around generation freshness,
warmup jobs, observed running state, attach outcomes, delayed rewarm, and prompt
submission results. The tmux adapter remains the only interactive runtime and
owns tmux command construction, target names, and terminal attach details.

### Default Branch

The default branch is the base branch Prism uses to distinguish mainline work
from task branches. It defaults to `main` and can be configured globally or per
repository with `default_base`.

Prism does not poll or display pull request state for the default branch. Startup
setup also uses the default branch to decide whether the current checkout should
be moved into a separate worktree.

### PR Cache

The PR cache is Prism's local snapshot of GitHub pull request state for a
non-default branch. It includes summary fields, details such as comments and
checks, polling timestamps, a signature used to detect changes, and any refresh
error.

The cache exists to keep the board responsive and to avoid polling GitHub on
every render. Refresh logic should preserve that separation: UI renders cached
state, while lifecycle or GitHub code refreshes it.

The PR Cache module owns branch eligibility, refresh pollability, summary/detail
preservation rules, comment-count facts, render-change signatures, and refresh
errors. Callers should consume those facts instead of rebuilding timestamp,
signature, default-branch, or optional-detail rules.

### Plan Mode

Plan mode is Prism's workflow for executing Markdown implementation plans as
numbered steps through OpenCode. The user selects or passes a plan file, Prism
infers phase count from headings like `Phase 1`, and then builds each step as a
task such as `Implement plan-better.md phase 6`.

Plan runs are modeled as persistent Prism state: a `PlanRun` owns the selected
repository or worktree scope, plan file, mode, and aggregate status, while
`PlanStepRun` records track per-phase prompts, OpenCode session/process
metadata, latest message/tool/todo state, and bounded output lines. Prism stores
that state in its own SQLite database under the user's Prism config directory,
not in the project repository.

Plan phases use OpenCode's `medium` agent variant by default. The selected
variant is persisted on each `PlanStepRun` and shown in the plan dashboard so
historical phase output remains explainable.

The TUI `P` launcher creates a persisted plan run for the selected repository or
worktree, starts sequential execution in the background, and renders the active
run in the main panel from SQLite snapshots. The tmux-backed plan workflow
remains a compatibility path for CLI/debug use while dashboard parity continues
to improve.

### Auto Flow

Auto Flow is Prism's persisted workflow for taking one clean, non-default
Worktree Session through implementation, local verification, PR creation, review
repair, CI repair, and eventual merge or cleanup. Implementation can come from a
prompt, an existing Markdown plan, or a drafted `plan.md` that pauses for user
approval before execution.

An Auto Flow run is stored in Prism's per-repository SQLite database as an
`AutoRun` plus ordered `AutoStepRun` attempts. The `step_key` identifies the
conceptual boundary, while each attempt gets its own monotonic sequence and
output rows so repeated verification, review, and CI repairs remain auditable
after restart.

Plan-backed Auto Flow delegates implementation to a linked Plan Mode run instead
of duplicating each phase as an Auto Flow step. The Auto pipeline records one
`RunPlan` step with a linked `PlanRun`, waits for that plan run to finish, and
then continues with local verification and the rest of the PR pipeline.

### PR Stabilization

PR Stabilization is Prism's core workflow for taking an existing pull request
from its current observed state to all required gates passing. It starts after
Auto Flow creates or updates a pull request, or when a user asks Prism to manage
a review, CI, or mergeability repair for an existing Worktree Session. Auto Flow
delegates pull request gate decisions to PR Stabilization instead of owning a
separate linear PR checklist.

Prism treats PR Stabilization as derived work rather than a fixed checklist. It
observes local Git state, cached pull request state, repository policy, and the
configured Auto Flow goal, derives the current blocker, and chooses one safe next
work item such as review repair, CI repair, waiting for checks, or ready for
manual merge.

Managed repair work remains auditable in Prism state. A managed repair may ask an
agent to prepare a change, verify it, and create a local repair commit. The
commit can then wait in a pending-push state for user review. If a guarded review
repair is pushed, Prism may resolve only the exact GitHub review threads that the
repair was based on.

Actionable review feedback means feedback submitted through GitHub review
mechanisms, such as review bodies and inline review-thread comments. Top-level
pull request comments are not treated as review feedback by default.

A pending repair push is guarded by the repair commit and observed branch state.
If the commit is pushed outside Prism, Prism can mark the push satisfied and
re-observe. If the branch moves away from the guarded commit, Prism invalidates
the pending push and replans instead of pushing blindly.

### Startup Setup

Startup setup is Prism's first-run or misaligned-checkout prompt for a tracked
repository. When launched from a non-default branch, Prism can offer to switch
the main checkout back to the default branch and move the active branch into a
Worktrunk worktree.

Startup setup is intentionally conservative. It only prompts in a TTY, checks
that the branch can be moved, and refuses to move a dirty checkout.

## Review Expectations

- Use the terms above in code, docs, and reviews.
- Keep product behavior centered on repositories, worktree sessions, agent
  sessions, and cached PR state.
- Prefer changes that preserve local state outside project repositories unless a
  feature explicitly needs repository-owned files.
- Treat default-branch behavior as a product boundary: task branch workflows
  should not accidentally apply to the default branch.
