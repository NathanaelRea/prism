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

Adding a repository through `--repo <path>` or the TUI discovers the Git root and
adds it to `repos.toml` if it is not already tracked. Removing a repository from
`repos.toml` stops Prism from tracking it; it does not delete the repository.

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
logs, hidden markers, change-request cache data, and links to Workflow Runs and
Artifacts. A run can link zero, one, or many Worktree Session incarnations;
deleting one retires the link without making that session the owner of history.

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

### Workflow Definition

A Workflow Definition is a named, versioned declaration of how Prism should take
typed inputs through Steps to declared outcomes. It defines dependencies,
conditions, policies, and required capabilities. An ordered list is shorthand
for a dependency chain; explicit dependencies can form an acyclic graph.

Bundled planning and end-to-end coding workflows are ordinary Workflow
Definitions. They use the same execution and history model as user-authored
definitions.

### Workflow Component

A Workflow Component is a reusable, versioned portion of a Workflow Definition.
It exposes typed parameters, inputs, and outputs and is included through explicit
composition rather than positional inheritance.

A Step Implementation is the reusable behavior selected by one Step. Agent
prompts, commands, provider operations, Gates, and notifications can all have
built-in or user-defined implementations within their primitive class.

### Trigger

A Trigger is a durable rule that creates Workflow Runs from manual input, a
schedule, or a provider event. It binds a Workflow Definition, inputs,
parameters, and an Admission Policy. Waiting for a Trigger does not create a
running Step.

### Workflow Run

A Workflow Run is one durable execution of a fully resolved Workflow Definition.
It owns its initial inputs, Definition Snapshot, Steps, attempts, Artifacts,
decisions, child-run lineage, and aggregate outcome.

A Definition Snapshot is the immutable resolved definition used by one run.
Changing the source definition does not reinterpret the run or its history.

### Workflow Step

A Workflow Step is one declared node in a Workflow Definition. Its primitive
class is Action, Gate, Approval, Wait, Notification, or Workflow Call, and its
dependencies and conditions determine when it can run.

A Step Attempt is one auditable execution of a Step against exact input
revisions. Retries create new attempts rather than replacing prior evidence or
output.

### Artifact

An Artifact is an immutable, typed, revisioned input or output of a Workflow Run.
Issues, Plans, Worktree Sessions, Commits, Change Requests, review reports, and
observations can be represented as Artifacts while retaining their own canonical
identities. Artifact lineage records which Step Attempt produced and consumed a
revision.

Artifact provenance records trust and sensitivity inherited from all sources.
Deriving, summarizing, or reviewing an Artifact does not make untrusted input
authoritative.

### Plan

A Plan is an Artifact with human-readable instructions and a validated manifest
of bounded phases, stable phase identities, dependencies, and declared inputs.
Plan phases can parameterize child Workflow Runs but do not alter a running
parent definition.

### Gate

A Gate is a read-only Workflow Step that decides whether exact evidence for an
exact subject revision satisfies a policy. CI, provider review, mergeability,
merge conflicts, local verification, and security policy are independent Gates;
their dependencies come from the Workflow Definition rather than an inherent
checklist order.

A Gate cannot repair code, push, label, merge, or clean up. Those effects belong
to Action Steps that visibly depend on the required Gate result.

### Approval

An Approval Step creates an Approval Request for Artifact acceptance, capability
authorization, human attestation, or an exact mutation. An Approval Decision is
the durable response bound to the exact inputs and evidence presented; resuming
a paused run is not approval.

### Admission Policy

An Admission Policy decides whether an externally sourced item may cross from
read-only intake into code execution or meaningful mutation. It grants authority
only from named provider-authenticated facts and trusted local policy;
agent-produced risk or security analysis is evidence, not authority to admit
itself.

An Admission Decision records one policy evaluation against an exact Observation
Revision and capability envelope. A child receiving that external content has
its own decision or delegated admission authority no broader than its parent's.

An Authority Grant is the recorded basis for a run's capabilities. It can come
from an Admission Decision, a capability-authorizing Approval Decision, or a
trusted manual Invocation Grant; delegation to a child can only narrow it.

### Execution Target

An Execution Target is a worker environment capable of executing particular
Step Implementations and capabilities. Local processes are the initial targets;
target identity and workflow history do not assume that execution always occurs
in the coordinator's process or at one local path.

An Execution Workspace is a target-affine checkout with its own target-neutral
identity, repository identity, and exact base revision. It may link a Worktree
Session incarnation but remains a leased workflow resource, not the identity of
the Workflow Run using it.

### Prism Worker

The Prism Worker is one on-demand per-user daemon and local coordinator. It
discovers tracked repository databases, evaluates Triggers and Workflow Runs,
claims runnable Step Attempts transactionally, renews their leases, assigns them
to compatible Execution Targets, and supervises their durable outcomes. Closing
the TUI does not stop it. It is not a login service and does not automatically
restart interrupted work after the daemon or machine stops.

The Prism Worker also observes interactive Agent Sessions and owns desktop
notification transition state, durable delivery intent, supersession, expiry,
and retry policy. Platform delivery remains behind adapters: Linux uses the
desktop notification service, while macOS forwards semantic notifications to
an active TUI terminal subscription.

Managed executor database connections install claim-bound SQLite guards. The
guards reject run, Step, Artifact, event, and process writes unless the
connection still owns the current unexpired fencing token. Executor loops also
revalidate ownership before harness, verification, Git, provider, and cleanup
effects. Resume and retry requests made while an executor is releasing persist a
requeue intent so runnable work cannot be stranded by the release race.

### Change Request Stabilization

Change Request Stabilization is the composition of observation, Gate, repair,
and guarded Action Steps used to move a Change Request toward a declared goal.
Review, CI, policy, mergeability, and merge-relation evidence remain independent,
even when a bundled component presents one most useful current blocker.

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
  Sessions, Agent Sessions, Workflow Runs, Artifacts, and cached Change Request
  state.
- Prefer changes that preserve local state outside project repositories unless a
  feature explicitly needs repository-owned files.
- Treat default-branch behavior as a product boundary: task branch workflows
  should not accidentally apply to the default branch.
