# Coordinating Concurrent GitHub Pull Requests

Research performed 2026-08-02 against first-party GitHub documentation and,
for corroborating merge-train behavior, first-party GitLab documentation.

## Recommendation

For a busy protected branch, make GitHub's merge queue the only normal merge
path. Use an active ruleset to require pull requests, reviews and conversation
resolution as appropriate, stable required checks, and the merge queue. Rulesets
compose across repository and organization policy, whereas only one branch
protection rule applies at a time. [GitHub rulesets][rulesets]
[GitHub active branch rules API][rules-api]

Treat coordination as two stages:

1. **Readiness:** develop, review, and run ordinary pull-request CI for one
   exact PR generation. A PR may record **Merge when ready** intent early, but review
   and all other required gates should pass before it actually enters the
   queue. GitHub delays queue admission until requirements are met.
   [Using a merge queue][queue-use] [Auto-merge][auto-merge]
2. **Integration:** let the queue create and test a synthetic commit containing
   the latest base plus the PRs ahead in the queue. Only checks for the current
   merge-group SHA authorize merge. [Managing a merge queue][queue-manage]
   [Actions `merge_group` event][actions-events]

Do not keep every open PR rebased or merged with the latest base. A merely
`BEHIND` PR is not broken. Update it only to resolve a real conflict, consume a
base change needed for development or review, or, when no queue exists, perform
the final strict integration run for the next PR to merge.

## Prism Diagnosis

Prism currently has the right PR-local observation and guarded-mutation pieces,
but its planner collapses integration facts into blockers too early:

- `stabilization_observe::mergeability_facts` converts every GitHub merge state
  other than `CLEAN`, `HAS_HOOKS`, and `UNSTABLE` into
  `MergeabilityFacts::Blocked`. This makes `BEHIND`, an actual conflict, and a
  policy/review `BLOCKED` state indistinguishable to the planner.
- `stabilization_plan::blocker_priority` puts `MergeBlocked` before review and
  CI, and `work_kind_for_blocker` maps it to `Escalate`.
- `stabilization_execute::step_for_work` intentionally has no step for
  `Escalate`. The run therefore records an escalated state and stops. This is
  the reported "does nothing" behavior for a behind PR.
- `PolicyBlocker::MergeQueueRequired` is always added when the policy requires
  a queue. It is treated as `PolicyBlocked`, although it should select the
  integration strategy.
- `PullRequestFacts::queue_state` is observed but does not participate in
  planning. It is only useful after the existing merge mutation has already
  returned a pending outcome.

A narrow fix that maps `BEHIND` to "update branch" would remove the immediate
no-op but produce the fan-out problem: one base merge would mutate every open
PR, restart their checks, and potentially invalidate their approvals. The
long-term fix is a repository-scoped integration coordinator, not another
PR-local blocker priority.

## Target Prism Model

Keep Change Request Stabilization as a **PR-local readiness converger**. Add one
repository-scoped **integration lane** per canonical target repository and
target branch. This is the seam at which ordering, capacity, latest-base
integration, and merge belong.

The two modules have different authority:

| Module | Owns | Does not own |
|---|---|---|
| PR readiness converger | Implementation, local verification, review and CI observation, review/CI repair, guarded repair pushes, exact-head readiness | Queue position, updating every behind branch, merge ordering |
| Integration lane | Durable merge intent, ready backlog, dependency order, admission, provider queue or serialized fallback, integration generations, merge completion | Implementation or ordinary review/CI repair |

The lane's small conceptual interface is:

```text
reconcile(lane, optional_intent_change, wake_cause) -> durable lane view + next wake
snapshot(lane) -> cached durable lane view
```

Provider observation, effect journaling, queue capacity, exact-SHA guards,
fallback leases, retry, and crash recovery stay behind that interface. Auto
Flow callers publish or withdraw intent and project the returned candidate
state; they do not execute provider queue mechanics themselves.

### Preserve Orthogonal Facts

Do not replace the current single blocker with a larger single phase enum.
Persist these dimensions independently and derive a concise user status from
them:

| Dimension | Suggested values |
|---|---|
| PR generation | Canonical PR identity, exact source head SHA, and target identity |
| Merge intent | `Unarmed`, `Armed`, `Withdrawn`, always bound to one PR generation |
| Review gate | `Unknown`, `Feedback`, `AwaitingApproval`, `Passed` |
| PR CI gate | `Unknown`, `Pending`, `Failed`, `Passed`, with exact tested SHA/context evidence |
| Merge relation | `Unknown`, `Behind`, `Conflicting`, `Mergeable`, `ProviderBlocked(reason)` |
| Policy gate | Exact observed revision plus `Unknown`, `Satisfied`, or concrete requirements/blockers |
| Integration strategy | `Unknown`, `NativeQueue`, or `SerializedFallback`; queue-required policy constrains this choice |
| Integration placement | `NotReady`, `Ready`, `Backlogged`, `Admitting`, `Queued`, `Validating`, `Removed`, `FallbackReserved` |
| Integration generation | Provider entry ID plus merge-group/train SHA, or fallback base/candidate SHAs |
| Terminal outcome | `Merged`, `Closed`, `Stopped`, `Failed` |

Useful user-facing projections are `Working`, `Waiting`, `NeedsUser`,
`MergeReady`, `Failed`, and `Merged`. A queued PR can remain annotated as
"ready at head H" while its top-level status is "waiting for queue checks at
generation G". Unknown or stale facts are waiting/safety states, never merge
permission.

### Actions And Ordering

CI, review, and merge should not be a total ordered checklist. Their partial
order is:

1. Establish a clean pushed PR generation and run cheap/local validation.
2. Run ordinary PR CI and code review concurrently for that exact generation.
3. Serialize head-changing repairs for one PR. If both review and CI have
   actionable failures, coalesce their current evidence into one repair when
   practical; otherwise handle review feedback before a separate CI repair,
   because the review change will invalidate the old CI result anyway.
4. Re-observe both gates after every push. Required approval and required PR CI
   must describe the final admitted PR generation.
5. Publish exact-head readiness to the integration lane. A merely `BEHIND` PR
   may do this when a native queue is available; a truly conflicting PR may
   not.
6. Admit ready generations according to lane order.
7. Run provider integration CI on the active merge-group/train generation, or
   run the one reserved serialized fallback integration.
8. Merge and verify provider-confirmed completion.

The planner may choose one mutating repair at a time, but it should return all
observed gate facts and all independent waits. Waiting for review must not hide
a CI failure, and pending CI must not hide new review feedback.

Recommended action vocabulary:

- `Observe` / `Reconcile`: always safe and idempotent.
- `RepairReviewAndCi`, `RepairReview`, `RepairCi`: PR-head mutations, one at a
  time per PR generation.
- `ResolveConflict`: distinct from branch-behind handling and expected to
  invalidate old gate evidence.
- `WaitForReview`, `WaitForPrCi`, `AwaitGuardedPush`: expected waits or explicit
  user seams, not failures.
- `PublishReadiness`, `WithdrawIntent`: handoff to the integration lane.
- `AdmitToNativeQueue`: exact-head guarded and idempotently reconciled.
- `WaitForIntegrationGeneration`: observe queue entry and exact merge-group or
  train SHA without occupying a worker in a blocking poll loop.
- `ReserveFallback`, `UpdateReservedCandidate`, `ValidateReservedCandidate`,
  `MergeReservedCandidate`: serialized fallback actions.
- `ReconcileUncertainMutation`: observe before retrying an enqueue/update/merge
  whose outcome is unknown.
- `Cleanup`: only after provider-confirmed merge.

### Native Queue And Fallback

When a native queue is available or required, enqueue ready PR generations and
let the provider bound speculative builds. Ten queued PRs do not imply ten
merge commits pushed into ten PR branches: the provider creates synthetic
integration generations only up to its configured build concurrency. Prism
must not update those PR branches merely because they are behind.

When no trustworthy native queue exists, use a lane-local backlog with one
`FallbackReserved` candidate. Only that candidate may be updated to the latest
base when strict policy requires it, then it must regain exact-head review and
CI readiness before a guarded merge. Other PRs continue review and ordinary CI
in parallel but remain unmodified. This intentionally trades integration
throughput for correctness and avoids update storms.

### Priority And Dependencies

Default to FIFO by the time an exact PR generation first becomes ready, not PR
creation time. This avoids an unreviewed PR blocking unrelated ready work.

Do not start with arbitrary numeric repo-level priority. Use these controls:

- Explicit dependencies are hard ordering edges and always outrank priority.
- A normal ready backlog is FIFO.
- An optional `Expedite` class may move a still-backlogged candidate ahead of
  normal candidates, with an audit reason.
- Never reorder a candidate already admitted to the provider queue. Queue
  jumping invalidates speculative work and should remain an incident-only,
  explicitly audited provider operation.
- A repaired or externally changed PR head is a new generation and normally
  rejoins at the tail.

This gives Prism repository-level ordering without making every merge on the
base dispatch branch updates across the repository.

### Migration Direction

The durable implementation can be introduced in tracer steps without changing
the target model:

1. Split `Behind`, `Conflicting`, provider `Unknown`, and policy/review
   `Blocked` observations. Stop mapping `BEHIND` and queue-required policy to
   `Escalate`.
2. Change PR stabilization from "first blocker wins" to an exact-generation
   readiness report plus one serialized head-mutating next action.
3. Persist merge intent and a target-branch integration lane. Make current
   `Merge` work publish readiness and wait on the lane instead of owning repo
   ordering.
4. Enrich provider adapters beyond the summary `QueueState`: preserve queue
   entry identity, source head, position/state, active integration generation
   SHA, removal reason, and freshness.
5. Add native queue admission/reconciliation first, then the one-candidate
   serialized fallback.
6. Replace long worker-held polling with event wakeups plus bounded polling and
   idempotent reconciliation.

Do not implement branch-update fan-out as an intermediate architecture. It
creates persisted behavior that the integration lane would immediately need to
remove.

## Why Eager Updates Are Counterproductive

GitHub explicitly describes strict, up-to-date checks as causing more builds,
because each target-branch change can require another branch update and build.
It describes merge queue as providing the same latest-base safety without
requiring authors to update and wait again. [Required checks][rules-available]
[Managing a merge queue][queue-manage]

An update is a branch mutation: GitHub's update operation merges or rebases the
base into the PR branch. Required checks from an earlier commit do not satisfy
the new latest SHA. With stale-review dismissal enabled, clicking **Update
branch**, pushing a diff-changing commit, or a base change that changes the diff
can also dismiss approvals. [GitHub CLI update branch][cli-update]
[Required-check SHA semantics][checks-troubleshoot]
[Review freshness rules][rules-available]

Across many PRs, eager updates therefore amplify each base merge into many new
heads, CI runs, mergeability calculations, possible conflicts, and review
invalidations. Most of that work is discarded when another PR merges first.
The queue instead speculates only for merge-ready PRs, bounds concurrent builds,
and rebuilds only the affected suffix when its assumptions change.
[Managing a merge queue][queue-manage]

Use Actions concurrency to cancel superseded **ordinary PR** runs for the same
workflow and PR/ref. Do not give distinct merge-group generations a shared key
that accidentally cancels a still-active queue build; each group ref and SHA is
a separate integration candidate. [GitHub Actions concurrency][actions-concurrency]

## Operating Flow

| State | Required evidence | Action |
|---|---|---|
| Draft/developing | Current head SHA; local and optional fast CI | Do not update merely because the base moved. Cancel superseded CI for old heads. |
| Review | Non-draft; review decision and thread state for the current diff | Finish review before actual queue entry. If this is mandatory, encode it in the ruleset rather than relying on bot convention. [Review rules][rules-available] |
| Ready/pre-queue | Effective active rules known; required PR checks complete on the latest relevant SHA | Set **Merge when ready** or enqueue with an expected head SHA. [Queue admission][queue-use] [GraphQL queue mutation][graphql-pulls] |
| Queued/building | Queue entry plus current merge-group ref/SHA; required checks pending on that SHA | Let the queue own latest-base integration and ordering. Do not update the PR branch just because it is behind. [Queue operation][queue-manage] |
| Removed/repairing | Removal reason and failed generation retained | Classify, repair or retry, satisfy readiness again, then normally re-enter at the tail. |
| Merged | Provider-confirmed merged state and merge commit | Stop all stale work for the PR and reconcile dependent work. |

GitHub Actions workflows that produce required queue checks must listen to both
events, because `merge_group` is separate from `pull_request`:

```yaml
on:
  pull_request:
  merge_group:
```

Without `merge_group`, a required check is never reported for the queue and the
merge fails. A required workflow should not be skipped at workflow level by a
path or branch filter, because its check remains pending; prefer an always
reported required job or aggregator whose internal jobs may conditionally skip.
Required job names should also be unique across workflows to avoid ambiguous
check results. [Required-check troubleshooting][checks-troubleshoot]
[Protected branches][protected-branches]

## Speculative CI And Batching

GitHub uses cumulative speculation. If `A`, `B`, and `C` are queued in that
order, candidate builds conceptually test `base+A`, `base+A+B`, and
`base+A+B+C`. Builds up to the configured build-concurrency limit can proceed
without waiting for each earlier PR to merge. If `B` fails, it is removed and
the later candidate is rebuilt without `B`. [Managing a merge queue][queue-manage]

GitLab merge trains independently document the same established design: the
cumulative pipelines run in parallel, and removing a failed entry cancels and
recreates later pipelines because the previous combined results are no longer
valid. [GitLab merge trains][gitlab-trains]

Keep these controls distinct:

| Control | Meaning | Recommended use |
|---|---|---|
| Build concurrency / `max_entries_to_build` | Number of speculative candidates requesting CI at once | Set to sustainable spare CI capacity; increasing it trades compute for lower queue latency. [Queue settings][queue-manage] |
| Grouping strategy | `ALLGREEN` requires each candidate in a group to pass; `HEADGREEN` permits earlier failing candidates when the cumulative head passes | Default to `ALLGREEN` for attribution and deterministic safety. Use `HEADGREEN` only as a conscious policy for accepted intermittent failures, not as a substitute for fixing flakes. [Queue rules API][rules-api] |
| Minimum/maximum entries to merge | Number committed to the base together after checks pass | This controls base updates or deployment cadence, not CI-build batching. Start with one unless batching deployments has measured value. [Queue merge limits][queue-manage] |
| Status-check timeout | How long absence of a successful required result is tolerated | Set from observed CI latency plus margin; expiry is a failure and removes the PR. [Queue settings][queue-manage] |

PR CI and queue CI answer different questions. By default, Actions checks out
the PR's current test-merge ref for a `pull_request` workflow, though a workflow
can explicitly choose the raw head. Queue CI checks a new merge-group SHA that
also includes the latest base and queued predecessors. Passing PR CI is an
admission signal, not reusable proof for queue CI. [Actions event SHA
semantics][actions-events] [Required-check commit selection][checks-troubleshoot]

## Signals To Preserve

Do not collapse coordination into one `mergeable` Boolean or one CI rollup.
GitHub exposes mergeability as `MERGEABLE`, `CONFLICTING`, or `UNKNOWN`, and a
separate merge-state status including `BEHIND`, `BLOCKED`, `DIRTY`, `DRAFT`, and
`UNSTABLE`. It also exposes review, auto-merge, queue entry, exact head/base OIDs,
and queue state separately. [GitHub GraphQL pull types][graphql-pulls]

| Signal family | Minimum useful fields | Why it matters |
|---|---|---|
| Identity/version | Repository, PR node/number, head ref and `headRefOid`, base ref and observed `baseRefOid` | Every gate observation must identify the code it describes. |
| Lifecycle | Open/closed/merged, draft, merge commit | Closed-unmerged and merged are different terminal outcomes. |
| Review | Required review decision, latest-push approval requirement, unresolved required threads | Review readiness can become false after a new reviewable push or diff-changing base movement. [Review rules][rules-available] |
| Merge relation | `mergeable`, `mergeStateStatus`, and whether the branch is merely behind or actually conflicting | `UNKNOWN` means calculation is pending; `BEHIND` is policy/context, not a failure by itself. [GraphQL merge states][graphql-pulls] |
| Effective policy | All active rules applying to the target branch, source level, required check contexts/apps, bypass ability, and observation time | Rulesets layer and the effective policy may come from repository or organization scope. The branch-rules endpoint returns all active applicable rules. [Ruleset layering][rulesets] [Rules API][rules-api] |
| PR CI | Check/status name, source app, status, conclusion, and exact tested SHA | Required checks must pass on the latest applicable SHA; check runs and commit statuses are SHA-scoped resources. [Check runs API][check-runs] [Commit statuses API][statuses] |
| Merge intent | Auto-merge enabled and expected PR head SHA | Intent may be recorded before readiness, but must not silently transfer to a changed head. |
| Queue entry | Entry ID, enqueued time, position, state, queue base/head commits, and jump flag | `QUEUED`, `AWAITING_CHECKS`, `MERGEABLE`, `UNMERGEABLE`, and `LOCKED` are distinct provider states. [GraphQL queue types][graphql-pulls] |
| Merge generation | Merge-group ref, `head_sha`, event/action, and active/destroyed status | This SHA, not the PR head SHA, is the unit authorized by queue CI. [Merge-group webhook][webhook-merge-group] |
| Failure | Failed/missing required contexts, timeout/conflict/policy reason, failed generation, attempt count | Downstream invalidation is different from the failed PR and should not consume that PR's retry budget. |
| Freshness | Fetch time plus the head, base, policy, entry, and merge-group identities observed together | Unknown, stale, and absent are different states; none should be converted to permission to merge. |

The platform's required-check decision is authoritative. GitHub accepts required
checks with `success`, `skipped`, or `neutral`, can require a particular GitHub
App as the source, and requires results on the latest applicable commit. An
orchestrator should display individual contexts but should not invent a looser
rollup. [Required checks][protected-branches]
[Required-check troubleshooting][checks-troubleshoot]

## Branch Update Policy

| Situation | Update branch? | Reason |
|---|---|---|
| Queue required; PR is only `BEHIND` | **No** | The queue tests latest base plus entries ahead without mutating the PR branch. [Managing a merge queue][queue-manage] |
| Review or development needs a newly merged API/schema | **Yes** | The base change is semantically part of the work, not queue housekeeping. Expect new SHA-bound CI and possibly renewed review. |
| Git reports a real merge conflict | **Yes** | Resolve deliberately, push one coherent result, and repeat readiness gates. Queue conflicts cause removal. [Queue failures][queue-manage] |
| Repair changes the PR | **Yes, by the repair commit** | Do not separately update unless needed; one push should invalidate old evidence once. |
| No merge queue; strict checks required; PR is next to merge | **Yes, once near merge** | Strict policy requires latest-base testing and GitHub warns that it causes more builds. Serialize this work rather than updating all candidates. [Strict checks][rules-available] |
| No queue; loose checks allowed | **Usually no** | Loose checks reduce builds but can miss incompatible changes; use only where that integration risk is accepted. [Loose checks][rules-available] |

If review freshness is important, prefer a policy that explicitly requires the
most recent reviewable push to be approved. Decide separately whether every old
approval must be dismissed. GitHub documents the former as a compromise that
avoids dismissing all prior reviews, while stale-review dismissal is safer
against unreviewed changes. [Review freshness rules][rules-available]

## Ordering And Priority

GitHub's normal queue order is FIFO. Preserve that as the default fairness and
predictability rule. Admission control is a cheaper priority mechanism than
reordering: arm urgent ready work before lower-priority work, but let already
queued work retain position. [Queue ordering][queue-manage]

The **jump to top** option should be incident-only. GitHub warns that it breaks
the speculative commit graph and triggers full rebuilds of in-progress entries,
which can reduce total merge velocity. Record who used it and why. Do not use it
for ordinary deadlines or to restore a repaired PR's old position.
[Queue priority behavior][queue-manage]

For dependencies, make prerequisite order explicit before admission. Enqueue a
dependent only when its PR can be evaluated and merged with predecessors in the
intended order; do not rely on repeated queue jumping to implement a stack.
Independent PRs need no manual ordering beyond FIFO.

## Failure, Removal, And Retry

GitHub removes a PR for failed required checks, check timeout, user request, an
unresolvable branch-protection failure, or conflict, and records the reason in
the PR timeline. It then recreates affected later merge groups without the
removed PR. [Queue failure behavior][queue-manage]

Apply this policy:

| Event | Response |
|---|---|
| Deterministic code/test failure | Remove or accept automatic removal, repair with a new commit, repeat review/check gates, and re-enter at the tail. |
| Transient runner/service/flaky failure | Prefer bounded job-level retry before a final failing conclusion. If already removed, re-enqueue only after service recovery; the new queue generation needs fresh CI. |
| Timeout or missing check | Diagnose trigger/reporting first, especially missing `merge_group`; retrying unchanged configuration only churns the queue. |
| Conflict or changed protection | Resolve or refresh policy, then repeat readiness. Do not bypass merely to preserve position. |
| User-requested removal | Treat as intentional and terminal until a new explicit merge intent is recorded. |
| Earlier entry removed | Mark downstream builds canceled/stale, not failed; wait for the queue's replacement groups. |

GitLab's mature merge-train behavior makes the stale-generation rule explicit:
a failed train pipeline cannot be retried after removal because its combined
commit is out of date; re-adding creates a new pipeline, while intermittent jobs
should retry inside the active pipeline. This is the correct generic model for
GitHub merge-group rebuilds too. [GitLab merge-train retry semantics][gitlab-trains]

Use a bounded transient retry count and backoff outside the queue. A repaired or
re-enqueued PR normally returns at the tail; use priority jump only under the
same incident policy as any other jump.

## Race And Staleness Semantics

1. **Guard intent with the PR head.** GitHub GraphQL queue and auto-merge inputs
   accept `expectedHeadOid`; `gh pr merge` has `--match-head-commit`; direct REST
   merge accepts `sha` and returns conflict on mismatch. Always supply the guard
   used by the chosen path. [GraphQL mutations][graphql-pulls]
   [GitHub CLI merge][cli-merge] [REST merge][rest-pulls]
2. **Key CI by generation SHA.** PR head, PR test-merge commit, and queue
   merge-group commit have distinct roles and may have different identities. A
   success for one SHA never authorizes another. [Actions event SHA semantics][actions-events]
   [Required-check commit selection][checks-troubleshoot]
3. **Invalidate suffixes, not history.** A removal, priority jump, or changed
   predecessor creates a different cumulative commit for later entries. Retain
   old results for diagnosis but exclude them from current readiness.
   [Queue rebuild behavior][queue-manage]
4. **Treat provider `UNKNOWN` as pending.** GitHub computes mergeability in the
   background and exposes an explicit unknown state; it is neither conflict nor
   permission. [Pull request mergeability][rest-pull-get]
5. **Reconcile after events.** Use `pull_request` enqueue/dequeue and
   `merge_group` checks-requested events as Actions wakeups; GitHub Apps can also
   consume the merge-group destroyed webhook. Process deliveries idempotently,
   then refresh authoritative PR, queue, checks, and active branch rules before
   acting. [Actions PR/merge-group events][actions-events]
   [Merge-group webhook][webhook-merge-group] [Active rules API][rules-api]
6. **Do not bypass the queue as a race workaround.** A direct urgent merge or
   queue jump invalidates speculative work behind it. If an emergency path is
   retained, make it explicit, rare, audited, and followed by queue
   reconciliation. [Queue priority behavior][queue-manage]

## Suggested Defaults

- Active ruleset: require PR, required review/thread resolution where expected,
  stable required checks from expected apps, and require merge queue.
- Review: complete before actual queue entry; permit **Merge when ready** as
  earlier intent only.
- Branch-behind: informational under merge queue, not an automatic update task.
- Queue strategy: FIFO, `ALLGREEN`, no routine jumps.
- Build concurrency: bounded to measured CI capacity.
- Merge batch: one initially; increase only for a measured deployment or
  base-update benefit.
- Retry: bounded transient retries; code failures require repair; re-entry at
  tail; no reuse of old merge-group results.
- Automation: every observation and side effect carries expected PR head SHA;
  every queue check carries the exact active merge-group SHA.

## Sources

[queue-manage]: https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/configuring-pull-request-merges/managing-a-merge-queue
[queue-use]: https://docs.github.com/en/pull-requests/collaborating-with-pull-requests/incorporating-changes-from-a-pull-request/merging-a-pull-request-with-a-merge-queue
[auto-merge]: https://docs.github.com/en/pull-requests/collaborating-with-pull-requests/incorporating-changes-from-a-pull-request/automatically-merging-a-pull-request
[rulesets]: https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-rulesets/about-rulesets
[rules-available]: https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-rulesets/available-rules-for-rulesets
[rules-api]: https://docs.github.com/en/rest/repos/rules#get-rules-for-a-branch
[protected-branches]: https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-protected-branches/about-protected-branches
[checks-troubleshoot]: https://docs.github.com/en/pull-requests/collaborating-with-pull-requests/collaborating-on-repositories-with-code-quality-features/troubleshooting-required-status-checks
[actions-events]: https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows#merge_group
[actions-concurrency]: https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/control-workflow-concurrency
[webhook-merge-group]: https://docs.github.com/en/webhooks/webhook-events-and-payloads#merge_group
[graphql-pulls]: https://docs.github.com/en/graphql/reference/pulls
[check-runs]: https://docs.github.com/en/rest/checks/runs
[statuses]: https://docs.github.com/en/rest/commits/statuses#get-the-combined-status-for-a-specific-reference
[cli-merge]: https://cli.github.com/manual/gh_pr_merge
[cli-update]: https://cli.github.com/manual/gh_pr_update-branch
[rest-pull-get]: https://docs.github.com/en/rest/pulls/pulls#get-a-pull-request
[rest-pulls]: https://docs.github.com/en/rest/pulls/pulls#merge-a-pull-request
[gitlab-trains]: https://docs.gitlab.com/ci/pipelines/merge_trains/
