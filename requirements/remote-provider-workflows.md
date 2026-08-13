# Remote Provider Workflows

## Provider Observations

- **Behavior**: Provider Adapters normalize GitHub Change Requests, GitLab merge
  requests, and Forgejo pull requests while retaining canonical host, project,
  native identity, exact head SHA, and provider-native facts.
- **Invariant**: Every observation has an Observation Revision that changes for
  every provider-controlled field available to display or Workflow Triggers. A
  provider-native revision is used only when it has that property; otherwise
  Prism computes a composite digest.
- **Behavior**: Change-request observations distinguish never loaded, current,
  stale, partial, failed, confirmed absent, and present. A failed refresh retains
  stale display state but cannot satisfy a Trigger or authorize mutation.
- **Invariant**: Unsupported provider capabilities remain distinct from empty,
  failed, stale, or unknown evidence. GitLab and Forgejo gaps are reported and
  never treated as satisfied.
- **Behavior**: Review observations expose actionable unresolved provider review
  bodies and inline threads with stable native thread IDs. Generic top-level
  comments are not actionable review feedback by default.
- **Behavior**: CI observations distinguish required and optional checks and bind
  their state to the exact current head. Queued, pending, passing, failing,
  unavailable, and unsupported are distinct.
- **Behavior**: Policy observations include required reviews, required checks,
  conversation resolution, strict up-to-date requirements, merge queues,
  mergeability, and branch relation when supported.

## Shared Remote Request Coordinator

- **Constraint**: One module owned by the per-user Prism Worker coordinates all
  Prism-owned provider observations and mutations, including TUI refreshes,
  interactive actions, Trigger checks, and Trigger post-Step mutations.
- **Behavior**: Callers request normalized operations shaped as observation with
  freshness/priority or mutation with priority. Provider adapters translate
  provider-specific data; callers do not manage rate-limit headers, retry loops,
  cache coalescing, or poll timers.
- **Invariant**: A lane is keyed by canonical provider host and credential
  profile. Initially each lane allows one in-flight provider operation and a
  configurable minimum delay between starts across all repositories and runs.
- **Invariant**: Lane cooldowns survive Worker restart. `Retry-After` and
  provider reset facts move the durable next-start time; retryable failures use
  bounded exponential backoff with jitter.
- **Behavior**: Equivalent observations coalesce by operation key, exact subject
  revision, and freshness. All Workflow Steps and TUI subscribers interested in
  one operation wake from its single result.
- **Behavior**: Priority is interactive mutation, active Workflow hook, active
  Workflow observation, then background refresh. Aging prevents starvation.
- **Invariant**: Queue length, response bytes, pages, retries, and accepted
  observation age are bounded. Queue pressure is visible and cannot be mistaken
  for an empty result.
- **Behavior**: A queued or temporarily unavailable observation becomes Trigger
  `Wait` with an actionable summary and earliest wake, such as `waiting for
  GitHub request slot` or `checks running; poll in 20s`. It does not consume an
  Agent slot.
- **Constraint**: For CLI-backed adapters Prism paces each logical adapter
  operation but cannot inspect requests made internally by the CLI. Direct HTTP
  adapters acquire permission for every actual HTTP request. Agent-issued
  provider commands and arbitrary custom-Trigger network calls are outside this
  coordinator.

## Triggered Stabilization

- **Invariant**: Trigger observations bind the exact selected Change Request and
  exact head. A mismatched, stale, partial, failed, unavailable, or unknown head
  cannot be reported as satisfied.
- **Behavior**: `merge_conflict` is satisfied when the current head is up to date
  and mergeable, waits for retryable unknown state, and runs when behind or
  conflicting. Its prepare hook fetches the configured base with structured Git
  arguments and starts a merge in the selected worktree. Expected conflicts are
  prepared Agent state.
- **Behavior**: `needs_review` runs when actionable unresolved review threads
  exist, waits while required review is pending or unavailable, and is satisfied
  when review policy passes. Prepare persists exact unresolved thread IDs and
  observation revision; finalize resolves only that captured set after Agent
  success.
- **Behavior**: `ci_failure` runs for failed required checks, waits for pending or
  temporarily unavailable required checks, and is satisfied only when required
  checks pass on the exact current head. Optional failures remain visible but do
  not replace required-check facts.
- **Behavior**: `ready_to_merge` is check-only. It is satisfied only when fresh
  exact-head CI, review, provider policy, mergeability, and branch-relation facts
  all permit merge. Legitimate external progress waits; unsupported or
  non-retryable states Prism cannot safely classify fail.
- **Invariant**: Stabilization does not merge, delete a branch, or clean a
  worktree. It stops at ready-to-merge; the explicit board merge action is a
  separate provider mutation.
- **Invariant**: Review text and CI logs are untrusted data. Trigger prepared
  state is not inserted into the Agent prompt, and Agent output does not grant
  provider mutation authority.

## Change-Request Actions

- **Behavior**: The board can push a selected branch, create a Change Request,
  open it in a browser, and request an explicit merge action independently of
  stabilization. The provider handles that merge request natively: it enters a
  configured provider merge queue when required and merges directly otherwise.
- **Invariant**: Push and merge revalidate repository, branch, remote, expected
  head, target branch, and required provider policy immediately before mutation.
  Unknown or stale policy fails closed.
- **Behavior**: Merge uses squash by default and provider-enforced exact-head
  protection when available. If the provider cannot close the observation/
  mutation race, Prism refuses or requires an explicit risk decision.
- **Behavior**: After provider-confirmed merge, local worktree cleanup remains an
  explicit separate action. Automatic cleanup and remote branch deletion are not
  part of stabilization.

## Provider Exceptions

### GitHub

- **Constraint**: `gh` remains the credential and CLI transport broker where
  used. Merge uses `--match-head-commit`. Pagination or policy limitations are
  reported as incomplete evidence.

### GitLab

- **Constraint**: `glab` is required only for GitLab repositories. Global merge
  request IDs remain mutation identity; project-local IIDs are display labels.
  Pipeline evidence is selected for exact source SHA, and unavailable tier or
  policy evidence remains unknown.

### Forgejo And Codeberg

- **Constraint**: HTTPS uses certificate-validating platform TLS. Tokens come
  only from configured environment-variable names. Guarded merge sends
  `head_commit_id`. Unsupported conversation resolution and merge queues remain
  visible capability gaps.
- **Invariant**: Codeberg is a built-in Forgejo Host Profile, not a separate
  provider protocol.

## Verification

- **Quality**: Provider fixtures run without credentials or network access and
  cover exact-head review, CI, policy, mergeability, branch relation, retry, and
  unsupported capability states.
- **Quality**: Compatibility jobs test pinned GitLab and Forgejo APIs separately
  from normal CI. Public-host drift probes are unauthenticated, read-only, and
  never emit response bodies, credentials, or mutation requests.
