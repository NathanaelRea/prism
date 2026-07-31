# Remote Change-Request Workflows

## Change-Request State

- **Behavior**: Prism discovers GitHub pull requests, GitLab merge requests, and
  Forgejo pull requests created either through Prism or
  externally and caches their summary, review, check, comment, merge, and refresh
  state for responsive rendering after startup.
- **Behavior**: A repository without a known or explicitly configured hosting
  adapter does not trigger change-request, CI, or review queries. Unknown hosts
  are never probed and cached display state is retained as stale.
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
- **Behavior**: Configured pre-push checks run before ordinary and repair pushes.
  Pull-request creation additionally runs pre-PR checks, while manual merge is
  refused for a dirty worktree and runs its configured safety checks.
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
  repository policy, and the requested goal, then identifies one safe next
  blocker/action across review, CI, mergeability, waiting, and readiness.
- **Behavior**: Actionable review feedback consists of provider review bodies and
  inline review threads. Generic top-level summaries are context, not requested
  changes, by default.
- **Invariant**: Review text, comments, and CI logs are untrusted input. Prism
  clearly delimits them from its instructions and never grants filesystem,
  command, push, thread-resolution, or merge authority based on their contents.
- **Behavior**: Review-repair prompts include actionable inline feedback with
  file/line context; CI-repair prompts include change-request identity, failing action facts,
  and a useful bounded failure-log tail.
- **Invariant**: Starting a review or CI repair creates exactly one new harness
  session for the selected worktree and delivers the prompt only there.
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

- **GitHub**: `gh` remains the credential and transport broker. Merge uses
  `--match-head-commit`. Summary and policy pagination limitations are reported
  as incomplete or unknown evidence rather than false facts.
- **GitLab**: `glab` is required only for GitLab repositories. Global merge-request
  IDs remain mutation identity while project-local IIDs remain display labels.
  Pipeline evidence is selected for the exact source SHA; hidden tier or policy
  evidence remains unknown and blocks automatic merge.
- **Forgejo and Codeberg**: HTTPS uses certificate-validating platform TLS transport.
  Tokens come only from a configured environment-variable name. Guarded merge
  sends `head_commit_id`. Conversation resolution and merge queues are unsupported
  and unavailable in the UI and managed repair paths.
- **Codeberg**: Codeberg is a built-in Forgejo host profile, not a separate
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
