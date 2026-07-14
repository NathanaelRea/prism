# Auto Flow

Auto Flow automates one clean, non-default Worktree Session through implementation, local verification, commit, and pull request creation. After a PR exists, Auto Flow delegates gate decisions to PR Stabilization.

PR Stabilization observes local Git state, cached GitHub pull request state, repository policy, and Auto Flow configuration. It derives the current blocker and chooses one safe next work item instead of following a fixed review-to-CI checklist.

Start or focus it from the TUI with `A` on a selected non-default worktree. New runs ask how to source implementation work:

- prompt: enter an initial prompt and run the current one-shot implementation step
- plan file: select an existing Markdown plan and run its phases before local verification
- draft plan: enter a task prompt, draft `plan.md`, pause for review/approval, then run the approved plan phases

Start or resume Auto Flow from the CLI with:

```sh
prism --repo /work/project auto "implement the task"
prism --repo /work/project auto run-plan plan.md
prism --repo /work/project auto plan "draft and review a plan before coding"
```

Compatibility notes:

- `P` remains standalone Plan Mode for running plan phases without the Auto Flow PR pipeline.
- `A` is the TUI Auto Flow shortcut; the former leader-based Auto Flow shortcut has been removed.
- `auto plan` drafts, reviews, and waits for approval, then executes the approved `plan.md` by phase; `auto plan-first` and `auto intensive` are aliases.

The CLI resumes the most recent active Auto Flow run for that repository before starting a new one. Auto Flow state is stored in Prism's per-repository SQLite database under `~/.config/prism/repos/...`, not in the project checkout.

Safety defaults:

- launch is refused on the default branch, detached HEAD, or a dirty worktree
- every step attempt is persisted before the next external side effect
- implementation continues through local verification and commit, then pauses before push; other agent and approval boundaries still pause for review
- worktrees with active Auto Flow runs are highlighted in the worktree list so paused/running/failed runs are visible from the sidebar
- local verification runs `checks.pre_push`, `checks.pre_pr`, and a non-mutating merge-conflict check
- PR Stabilization handles review feedback, CI failures, pending checks, review approval, repository policy, mergeability, manual readiness, and auto-merge readiness as derived blockers
- managed review and CI repairs verify locally, create a repair commit, and enter guarded pending-push by default
- pending repair commits are not pushed automatically unless `auto.push_repairs = true`; inspect the commit diff and use `Space g P` to push through the guard
- `auto.merge = false` and `auto.cleanup_after_merge = false` by default

Useful per-repository options:

```toml
[auto]
merge = false
cleanup_after_merge = false
require_review_approval = false
push_initial = true
push_repairs = false
review_wait_enabled = true
review_reviewer_identities = ["Copilot", "github-copilot"]
review_max_wait_seconds = 300
review_poll_interval_seconds = 30
review_continue_on_timeout = true
ci_wait_enabled = true
ci_max_wait_seconds = 1800
ci_poll_interval_seconds = 30

[prompt_templates]
auto_create_plan = "Create an implementation plan at `{{plan_path}}` for: {{task}}"
auto_review_plan = "Review and edit `{{plan_path}}` for: {{task}}"
auto_implement = "Implement this task, then stop without committing: {{task}}"
auto_fix_local_verify = "Fix these local verification failures, then stop without committing: {{context}}"
auto_fix_review = "Resolve this review feedback, then stop without committing: {{context}}"
auto_fix_ci = "Fix this CI failure, then stop without committing: {{context}}"
repair_commit_review = "fix: cr"
repair_commit_ci = "fix: ci"
repair_commit_merge = "fix: merge"
```

Auto Flow prompt templates support `{{task}}`, `{{plan_path}}`, `{{mode}}`, `{{variant}}`, `{{agent_profile}}`, `{{context}}`, and `{{branch}}` where applicable.

The selected Worktree Session main panel shows the derived PR Stabilization state: blocker, next work, CI/review/merge/policy gates, guard state, and any pending repair commit. The Auto Flow dashboard remains the audit view for persisted step attempts and output.

Pending push behavior:

- `PendingPush` means Prism created a local repair commit and is waiting for user inspection.
- `Space g P` reobserves the selected Worktree Session before pushing.
- If the PR branch already contains the guarded commit, Prism marks the push satisfied and replans.
- If local or remote branch state moved away from the guard, Prism invalidates the pending push and replans instead of pushing blindly.
- Review repair pushes resolve only the guarded review thread IDs captured when the repair work was planned.

If Prism exits or the machine restarts, rerun `prism` or `prism auto` for the repository. Active Auto Flow steps are reconciled from the persisted run; stale running attempts are marked failed so they can be retried instead of being silently forgotten.

Troubleshooting:

- run `prism config` to inspect effective Auto Flow and check settings
- run `prism debug logs` or start with `--print-logs --log-level debug` for runtime events
- use the dashboard controls to pause/resume, retry a failed step, retry from a selected step, abort, or dismiss a completed run
- inspect pending repair commits with your normal git tools before pressing `Space g P`
- if a linked plan phase completed in OpenCode but Prism missed the final signal, focus the Auto Flow status view, select the phase, press `Space p`, then choose `s` to skip/accept that linked phase without rerunning it
- when draft-plan automation pauses after plan review, inspect `plan.md` in the worktree and resume the run when the plan is acceptable
