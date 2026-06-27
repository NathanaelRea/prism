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

The TUI `P` launcher creates a persisted plan run for the selected repository or
worktree, starts sequential execution in the background, and renders the active
run in the main panel from SQLite snapshots. The tmux-backed plan workflow
remains a compatibility path for CLI/debug use while dashboard parity continues
to improve.

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
