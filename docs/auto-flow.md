# Auto Flow

Auto Flow automates one clean, non-default worktree through implementation, local verification, pull request creation, automated review repair, CI repair, and a final merge gate.

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
- local verification runs `checks.pre_push`, `checks.pre_pr`, and a non-mutating merge-conflict check
- review repair uses `checks.review_fix` before the normal verification gate
- `auto.merge = false` and `auto.cleanup_after_merge = false` by default

Useful per-repository options:

```toml
[auto]
merge = false
cleanup_after_merge = false
review_wait_enabled = true
review_reviewer_identities = ["Copilot", "github-copilot"]
review_max_wait_seconds = 300
review_poll_interval_seconds = 30
review_continue_on_timeout = true
ci_wait_enabled = true
ci_max_wait_seconds = 1800
ci_poll_interval_seconds = 30
```

If Prism exits or the machine restarts, rerun `prism` or `prism auto` for the repository. Active Auto Flow steps are reconciled from the persisted run; stale running attempts are marked failed so they can be retried instead of being silently forgotten.

Troubleshooting:

- run `prism config` to inspect effective Auto Flow and check settings
- run `prism debug logs` or start with `--print-logs --log-level debug` for runtime events
- use the dashboard controls to pause/resume, retry a failed step, retry from a selected step, abort, or dismiss a completed run
- when draft-plan automation pauses after plan review, inspect `plan.md` in the worktree and resume the run when the plan is acceptable
