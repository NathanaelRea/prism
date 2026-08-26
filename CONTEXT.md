# Prism Context

Prism is a terminal board for running agent-backed coding sessions across Git
worktrees. This file defines the project language reviewers should use when
discussing behavior, code, and docs.

## Domain Terms

### Repository

A repository is a Git working tree root discovered with `git rev-parse
--show-toplevel`. Prism treats the repository root as the source of branches,
worktrees, remote change-request state, and per-repository Prism state.

Per-repository Prism state is stored under the user Prism config directory, not
inside the repository root. The path is derived from the repository name and a
stable hash of the root path.

### Tracked Repository

A tracked repository is a repository listed in `~/.config/prism/repos.toml`.
Tracked repositories appear in the repos panel, keep their configured order, and
may have a single-character key used by `Space <key>` shortcuts.

Adding a repository through `--repo <path>` or the TUI discovers the Git root,
registers its Worktrunk identifier in the Worktrunk user config, and only then
adds it to `repos.toml` if it is not already tracked. This user-owned registration
does not create or modify the repository's `.config/wt.toml`. Removing a
repository from `repos.toml` stops Prism from tracking it; it does not delete the
repository or its Worktrunk user entry.

### Worktree Session

A worktree session is a Git worktree shown in the worktrees panel for a tracked
repository. Prism discovers sessions from `git worktree list --porcelain`.

The default branch worktree is still a session, but Prism treats it specially:
it sorts first and does not show pull request cache data for that branch.
Non-default worktree sessions usually represent active task branches.

Each Worktree Session has a persistent identity independent of its branch name
and path. Branch names and paths may be reused after deletion, but the new
worktree is a new session and cannot inherit the old session's active state.

Prism may attach metadata to a session, including prompt summary, agent state,
logs, hidden markers, change-request cache data, and links to Workflow Runs. A
run is bound to one selected worktree for mutation while it is active; deleting
that worktree retires the link without making the session the owner of history.

Each Worktree Session records the Harness used by its tmux Agent Session. A
global Harness change does not silently reinterpret an existing worktree;
users migrate, defer, or pin that association when opening it.

The Worktree Session module owns session identity, default-branch
classification, branch metadata facts, background-safe snapshots, and deletion
warnings. It may carry Agent Session and Change Request Cache facts for callers,
but it should not own tmux lifecycle behavior or provider refresh semantics.

Git's live worktree inventory is authoritative for whether a worktree exists and
which branch is attached. Worktrunk owns physical worktree path policy,
creation and removal effects, project hooks and approvals, stable template
values, tethered processes, development URLs, variables, custom columns, and
hook logs. Prism owns Worktree Session identity, destructive confirmation,
Prism-resource cleanup, workflow history, and observation freshness. It joins
Worktrunk observations by repository and exact normalized path; branch names
are not an identity fallback.

### Agent Session

An agent session is a persistent tmux session for a worktree session. The agent
window runs the configured interactive agent command, and companion windows
provide lazygit and a shell in the same worktree.

Agent sessions are named `prism-<branch>-<repository-hash>-<generation>` so the
branch is visible before the internal repository identity. Prism can reattach to
an existing agent session, create one when needed, or replace one that is not
running the expected agent.

The Agent Session module owns lifecycle decisions around generation freshness,
warmup jobs, observed running state, attach outcomes, delayed rewarm, and prompt
submission results. The tmux adapter remains the only interactive runtime and
owns tmux command construction, target names, and terminal attach details.

### Default Branch

The default branch is the base branch Prism uses to distinguish mainline work
from task branches. It defaults to `main` and can be configured globally or per
repository with `default_base`.

Prism does not poll or display change-request state for the default branch. Startup
setup also uses the default branch to decide whether the current checkout should
be moved into a separate worktree.

### Provider Item

A Provider Item is a provider-hosted Issue or Change Request with canonical
provider, host, repository, native identity, and an Observation Revision. Its
externally controlled fields are untrusted workflow input.

An Issue tracks proposed or ongoing work. A Change Request tracks a proposed
change to repository history. They remain distinct even when a Provider Adapter
obtains both through the same endpoint or data shape.

An Observation Revision identifies the exact provider state Prism evaluated. It
uses a provider-native revision only when that revision changes for every field
available to Trigger selection, admission, conditions, prompts, and effects,
including relevant comments and provenance. Otherwise Prism uses a composite
digest covering that complete field set.

### Change Request Cache

The Change Request Cache is Prism's local snapshot of provider-hosted
change-request state for a non-default branch. A change request is a GitHub or
Forgejo pull request, or a GitLab merge request. The cache includes canonical
provider, host, project, native identity, exact head SHA, summary fields,
independently observed details, polling timestamps, and errors.

The cache exists to keep the board responsive and avoid provider work on every
render. Failed refreshes preserve stale display state but cannot authorize a
mutation. Provider adapters refresh it outside the TUI thread.

The Change Request Cache module owns branch eligibility, refresh pollability, summary/detail
preservation rules, comment-count facts, render-change signatures, and refresh
errors. Callers should consume those facts instead of rebuilding timestamp,
signature, default-branch, or optional-detail rules.

### Provider Adapter

A Provider Adapter owns one hosting protocol: GitHub, GitLab, or Forgejo. It
discovers and normalizes supported Issue, Change Request, review, CI, policy,
and mutation facts while retaining provider-native identity and state. Codeberg
uses the Forgejo adapter with a built-in Host Profile; it is not a fourth
adapter.

Adapters declare each optional operation as supported, unsupported, conditional,
or unknown. Capability does not imply fresh evidence. Callers must separately
evaluate observation quality and cannot authorize mutation from stale, failed,
partial, or unknown observations.

### Host Profile

A Host Profile maps one canonical hostname to a Provider Adapter and its web/API
bases. GitHub.com, GitLab.com, and Codeberg have built-in profiles. Every other
host requires explicit configuration before Prism probes it or consults a
credential source.

### Workflow

A Workflow is a prompt-first TOML file whose filename stem is its default
identity. It declares optional Agent defaults and an acyclic graph of Agent
Steps. A plain `[[step]]` list is linear; explicit `id` and `depends_on` values
create roots, branches, and joins. Prism may also retain an AI-generated one-off
Workflow draft in user state for one exact Worktree Session incarnation; running
it still creates the ordinary immutable Workflow Run snapshot.

Workflow source has no required schema version, qualified package ID, launch
mode, capability list, typed port, Step class, implementation ID, or
`skippable` declaration. Simultaneously eligible unconditional Steps may run
concurrently in a shared worktree; this experimental mode intentionally permits
overlapping edits, while dependency joins preserve graph ordering. It may declare typed file, string, boolean, number, and
enum inputs with optional defaults. Canonical values are substituted into Agent
turns; files use normalized worktree-relative paths and are never read into a
prompt implicitly. First setup copies
editable defaults into the user's workflow directory once. Installed and trusted
repository packages may also
provide files through the same conventional `workflows/` layout.

### Step Trigger

A Step Trigger is a reusable lifecycle adapter attached to one Agent Step. Its
observational check returns Run, Satisfied, Wait, or Fail. Optional prepare and
finalize hooks run immediately before and after a successful Agent and can
perform mutations. Persisted prepared state remains opaque and never becomes
prompt text.

Built-in and fake Triggers use the same in-process interface. External Triggers
are full-trust shebang executables invoked once per phase through a small
versioned process protocol. They have the user's OS authority; content-addressed
retention stabilizes active runs but is not a sandbox.

### Prism Package

A Prism Package may distribute Workflows and Triggers using conventional
`workflows/` and `triggers/` directories, plus unrelated Prism resources. The
Workflow kernel does not resolve package closures, typed Artifact schemas,
implementation descriptors, or capability envelopes. Active runs pin the exact
Workflow source and external Trigger executable bytes they use.

### Trigger And Launcher

Trigger is the short form of Step Trigger: it decides whether one existing
Workflow Step should run and owns that Step's optional prepare/finalize hooks.
Waiting in a Trigger does not occupy an Agent slot.

A Launcher creates Workflow Runs from a schedule, provider event, or other
automatic source. Launchers are a separate future module; run creation is never
a Step Trigger responsibility.

### Workflow Run

A Workflow Run is one durable execution of a compiled Workflow snapshot. It owns
its exact repository, Worktree Session and Change Request association, repeated
evaluation cycle, Agent-run budget, lifecycle attempts, Trigger decisions and
wakes, Agent sessions and completed turns, controls, and aggregate outcome.

The immutable snapshot pins source bytes, dependencies, typed input declarations
and canonical bound values, initial prompts and follow-ups, harness/model/variant
choices, context selections, and external Trigger executable revisions. Editing
or deleting source changes future runs only.

### Workflow Step

A Workflow Step is one Agent lifecycle node with an initial prompt, optional
authored follow-ups, and an optional Trigger. A Step without a Trigger runs once;
a triggered Step can run repeatedly as fresh observations start new evaluation
cycles. A check-only triggered Step has no Agent prompt.

A Step Attempt records checking, preparing, Agent, and finalizing boundaries,
persisted prepared state, one fresh Agent Session identity, each completed turn,
final text, timing, and terminal reason. Follow-ups resume only that Attempt's
session. Retry appends an Attempt and never erases history or restores consumed
Agent budget.

### Prepared State

Prepared State is opaque Trigger-owned data persisted after a successful
pre-Step hook and before Agent start. It lets the Worker resume at a known phase
boundary and lets a finalize hook mutate only the exact provider or repository
subjects captured during prepare. It is not prompt context or Agent authority.

### Remote Request Coordinator

The Remote Request Coordinator is the Worker-owned queue through which every
Prism-owned provider observation and mutation passes. It owns per-host and
credential-profile pacing, durable cooldowns, retries, coalescing, fairness,
and bounded evidence freshness. Agent and full-trust custom-Trigger traffic is
outside this boundary.

### Prism Worker

The Prism Worker is one on-demand per-user daemon and local coordinator. It
hot-discovers Workflow and Trigger files, evaluates repeated DAG cycles, claims
lifecycle phases with leases/fencing, supervises fresh Agent Sessions and their
authored follow-up turns, preserves durable wakes, and serializes mutations to
one worktree. Closing the TUI does not stop it.

The Worker also owns one Remote Request Coordinator used by TUI refreshes,
interactive provider actions, and Workflow Triggers. Provider lanes, cooldowns,
backoff, coalesced observations, and subscriber wakes are user-wide rather than
per process.

The Prism Worker also observes interactive Agent Sessions and owns desktop
notification transition state, durable delivery intent, supersession, expiry,
and retry policy. Platform delivery remains behind adapters: Linux uses the
desktop notification service, while macOS forwards semantic notifications to
an active TUI terminal subscription.

### Change Request Stabilization

Change Request Stabilization is the default linear Workflow of merge-conflict,
review, CI, and final ready-to-merge Triggers. Repeated graph evaluation lets an
earlier blocker react while a later Trigger waits. Stabilization stops at fresh
exact-head provider readiness; it does not merge or clean up the worktree.

Actionable review feedback means feedback submitted through provider review
mechanisms, such as review bodies and inline review-thread comments. Top-level
change-request comments are not treated as review feedback by default.

### Startup Setup

Startup setup is Prism's first-run or misaligned-checkout prompt for a tracked
repository. When launched from a non-default branch, Prism can offer to switch
the main checkout back to the default branch and move the active branch into a
Worktrunk worktree.

Startup setup is intentionally conservative. It only prompts in a TTY, checks
that the branch can be moved, and refuses to move a dirty checkout.

## Review Expectations

- Use the terms above in code, docs, and reviews.
- Keep product behavior centered on repositories, provider items, Worktree
  Sessions, Agent Sessions, prompt Workflow Runs, lifecycle Attempts, and cached
  Change Request state.
- Prefer changes that preserve local state outside project repositories unless a
  feature explicitly needs repository-owned files.
- Treat default-branch behavior as a product boundary: task branch workflows
  should not accidentally apply to the default branch.
