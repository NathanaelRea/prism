# Remote Provider Workflows

## Issue Intake And Triage

- **Behavior**: Provider Adapters can expose canonical Provider Item identity,
  title, body, lifecycle, author and repository relationship, labels and their
  provenance, assignees, timestamps, native revisions, and event checkpoints
  when the provider supports those facts. Prism derives a stable Observation
  Revision digest over every externally controlled field available to Trigger
  selection, admission, conditions, prompts, or effects unless a native revision
  is proven to change for that complete field set.
- **Invariant**: Pull or merge requests returned by an issue-shaped provider API
  retain their Change Request identity and are never admitted as Issues merely
  because they share an endpoint or response shape.
- **Behavior**: Provider capabilities independently declare issue discovery,
  event observation, labels, assignment, comments, and lifecycle mutation.
  Missing capability is visible to Workflow validation and Gate evaluation and
  is not treated as an empty result.
- **Invariant**: Issue observations distinguish never loaded, current, stale,
  partial, failed, confirmed absent, and present. Refresh failure preserves the
  prior observation and cannot create a false Trigger occurrence or authorize a
  mutation.
- **Behavior**: Scheduled or event-backed Triggers can select Issues through
  deterministic provider facts and start provider-neutral triage workflows.
  Each run records canonical Issue identity and exact Observation Revision;
  mutation and implementation additionally record its Admission Decision.
- **Invariant**: Issue classification and security analysis receive externally
  authored content as delimited untrusted data. Their output is an Artifact and
  retains that provenance; it never expands the run's Admission Policy, provider
  token scope, or execution capabilities.
- **Behavior**: Label, assignment, comment, and close Actions show their intended
  mutation, use exact provider identity, and reconcile an uncertain result
  before retry. A child workflow starts through a Workflow Call rather than a
  provider mutation Action. Bulk triage remains bounded and auditable per
  affected Issue.

## Change-Request State

- **Behavior**: Prism discovers GitHub pull requests, GitLab merge requests, and
  Forgejo pull requests created either through Prism or
  externally and caches their summary, review, check, comment, merge, and refresh
  state for responsive rendering after startup.
- **Behavior**: A repository without a known or explicitly configured hosting
  adapter does not trigger issue, change-request, CI, or review queries. Unknown
  hosts are never probed and cached display state is retained as stale.
- **Behavior**: The main panel hides the entire change-request section when none
  exists. When present, display number and title precede state, next action, merge, review, CI, and
  related gate facts.
- **Behavior**: Change-request state and next action use the same aligned key/value treatment
  as gate rows. Internal guard terms, base/head noise, and redundant section
  labels are omitted.
- **Behavior**: Review comments render as compact selectable rows that distinguish
  resolved state and root/inline origin; opening a row shows full detail.
- **Invariant**: Prism does not invent review severity when a provider supplies no
  reliable severity field.

## Change-Request Actions

- **Behavior**: A push/change-request action pushes the selected branch and
  creates a pull or merge request when none exists. If both `origin` and `upstream` are valid targets,
  Prism asks which target to use.
- **Invariant**: Push and merge actions revalidate the selected repository,
  branch, remote, expected head, target branch, and required gates immediately
  before mutation. Unknown or stale policy blocks automatic merge.
- **Behavior**: Push, change-request creation, repair, and merge Actions depend on
  the named verification and policy Gates declared by their resolved Workflow
  Definition. Manual merge is refused for a dirty worktree and runs its
  configured safety Gates.
- **Behavior**: Board commands for push, change-request creation, and manual merge
  launch named Standard Pack Workflow Definitions that compose child definitions
  and public Step Implementations, so their safety Gates, attempts, effects, and
  recovery use the same history and control model as triggered work.
- **Invariant**: Automatic merge uses a provider-enforced exact-head precondition
  and authoritative repository policy when available. If the provider cannot
  close the race between observation and mutation, the merge fails closed or
  requires an Approval Request that describes the residual risk.
- **Behavior**: Users can open the selected change request in a browser.
- **Default**: Merge uses squash unless configured otherwise.
- **Customization**: Merge strategy and whether repository policy requires an
  approving review are configurable. Review is not required by default.
- **Behavior**: After the provider confirms a merge, Prism offers explicit local
  worktree/session cleanup with Yes as the prompt default. Automatic cleanup
  remains disabled by default, and remote-branch deletion is not part of this
  cleanup requirement.

## Repair And Stabilization

- **Behavior**: Change-request stabilization observes local Git state, cached provider state,
  repository policy, and the requested goal. Its independent review, CI,
  mergeability, merge-relation, and policy Gates can identify one most useful
  current blocker without converting those facts into one ordered checklist.
- **Behavior**: Actionable review feedback consists of provider review bodies and
  inline review threads. Generic top-level summaries are context, not requested
  changes, by default.
- **Invariant**: Review text, comments, and CI logs are untrusted input. Prism
  clearly delimits them from its instructions and never grants filesystem,
  command, push, thread-resolution, or merge authority based on their contents.
- **Behavior**: Review-repair prompts include actionable inline feedback with
  file/line context; CI-repair prompts include change-request identity, failing action facts,
  and a useful bounded failure-log tail.
- **Invariant**: Starting a review or CI repair creates exactly one Agent Action
  attempt through its recorded Harness and delivers the prompt only to that
  attempt or its explicitly selected continuation session.
- **Behavior**: Prism records exactly which review threads informed a managed
  repair. After the guarded repair commit is pushed, it may resolve only those
  threads.
- **Invariant**: A pending repair push is guarded by its repair commit and
  observed branch state. An externally satisfied push is recognized; a diverged
  branch invalidates the pending push and causes replanning rather than a blind
  push.
- **Behavior**: Repository policy observation includes required approving
  reviews, required checks, conversation resolution, strict up-to-date rules,
  and merge-queue requirements. Required-check failures block readiness;
  optional-check failures remain visible without replacing required-check facts.

## Provider Exceptions

### GitHub

- **Constraint**: `gh` remains the credential and transport broker. Merge uses
  `--match-head-commit`. Summary and policy pagination limitations are reported
  as incomplete or unknown evidence rather than false facts.

### GitLab

- **Constraint**: `glab` is required only for GitLab repositories. Global merge-request
  IDs remain mutation identity while project-local IIDs remain display labels.
  Pipeline evidence is selected for the exact source SHA; hidden tier or policy
  evidence remains unknown and blocks automatic merge.

### Forgejo And Codeberg

- **Constraint**: HTTPS uses certificate-validating platform TLS transport.
  Tokens come only from a configured environment-variable name. Guarded merge
  sends `head_commit_id`. Conversation resolution and merge queues are unsupported
  and unavailable in the UI and managed repair paths.

### Codeberg

- **Invariant**: Codeberg is a built-in Forgejo host profile, not a separate
  protocol. Version, paging limits, Actions, and log availability are runtime
  observations.

## Compatibility And Rollout

- **Quality**: Provider contract fixtures run without network access and retain
  no credentials or private response data.
- **Quality**: Scheduled or manually dispatched compatibility jobs exercise
  pinned local GitLab and Forgejo API versions separately from normal CI. A
  missing required tool, Docker daemon, or image is reported as an explicit
  skip; an incompatible started service fails its job.
- **Invariant**: Public-host drift probes for GitHub.com, GitLab.com, and
  Codeberg are unauthenticated and read-only. They never create, update, resolve,
  merge, or delete public data and never emit response bodies or headers.
- **Quality**: Drift records contain only fixed host/provider identity, safe
  version/schema/capability metadata, outcome, bounded response byte count,
  latency, and observation time. Network unavailability is distinct from
  reachable schema drift.
