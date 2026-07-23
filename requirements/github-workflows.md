# GitHub Workflows

## Pull Request State

- **Behavior**: Prism discovers pull requests created either through Prism or
  externally and caches their summary, review, check, comment, merge, and refresh
  state for responsive rendering after startup.
- **Behavior**: A repository without a suitable GitHub remote does not trigger
  PR, CI, or review-comment queries. Polling begins when a suitable remote exists.
- **Behavior**: The main panel hides the entire PR section when no PR exists. When
  present, PR number and title precede state, next action, merge, review, CI, and
  related gate facts.
- **Behavior**: PR state and next action use the same aligned key/value treatment
  as gate rows. Internal guard terms, base/head noise, and redundant section
  labels are omitted.
- **Behavior**: Review comments render as compact selectable rows that distinguish
  resolved state and root/inline origin; opening a row shows full detail.
- **Invariant**: Prism does not invent review severity when GitHub provides no
  reliable severity field.

## PR Actions

- **Behavior**: A push/PR action pushes the selected branch and creates a pull
  request when none exists. If both `origin` and `upstream` are valid targets,
  Prism asks which target to use.
- **Invariant**: Push and merge actions revalidate the selected repository,
  branch, remote, expected head, target branch, and required gates immediately
  before mutation. Unknown or stale policy blocks automatic merge.
- **Behavior**: Configured pre-push checks run before ordinary and repair pushes.
  Pull-request creation additionally runs pre-PR checks, while manual merge is
  refused for a dirty worktree and runs its configured safety checks.
- **Behavior**: Users can open the selected pull request in a browser.
- **Default**: Merge uses squash unless configured otherwise.
- **Customization**: Merge strategy and whether repository policy requires an
  approving review are configurable. Review is not required by default.
- **Behavior**: After GitHub confirms a merge, Prism offers explicit local
  worktree/session cleanup with Yes as the prompt default. Automatic cleanup
  remains disabled by default, and remote-branch deletion is not part of this
  cleanup requirement.

## Repair And Stabilization

- **Behavior**: PR Stabilization observes local Git state, cached GitHub state,
  repository policy, and the requested goal, then identifies one safe next
  blocker/action across review, CI, mergeability, waiting, and readiness.
- **Behavior**: Actionable review feedback consists of GitHub review bodies and
  inline review threads. Generic top-level summaries are context, not requested
  changes, by default.
- **Invariant**: Review text, comments, and CI logs are untrusted input. Prism
  clearly delimits them from its instructions and never grants filesystem,
  command, push, thread-resolution, or merge authority based on their contents.
- **Behavior**: Review-repair prompts include actionable inline feedback with
  file/line context; CI-repair prompts include PR identity, failing action facts,
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
