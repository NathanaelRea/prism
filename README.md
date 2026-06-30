# Prism

```text
░▒▓█▓▒░ P ◤◥◣◢◤◥◣
▒▓█▓▒░▒ R ◥◣◢◤◥◣◢
▓█▓▒░▒▓ I ◣◢◤◥◣◢◤
█▓▒░▒▓█ S ◢◤◥◣◢◤◥
▓▒░▒▓█▓ M ◤◥◣◢◤◥◣
```

Prism is a terminal board for running agent-backed coding sessions across Git worktrees.

Use it to create branch worktrees, open persistent agent sessions, watch pull request state, and keep multiple coding tasks moving from one TUI.

## Demo

![Prism demo](docs/prism-demo.gif)

Refresh the demo GIF with:

```sh
./scripts/screenshot.sh
```

See [docs/prism-demo.md](docs/prism-demo.md) for regeneration notes.

## Install

```sh
./install.sh
```

Requires Rust/Cargo, `git`, `gh`, `tmux`, `wt`, and `opencode`. Plan mode also requires `fzf`.

## Development

Run the local CI gate before pushing:

```sh
scripts/full-check.sh
```

To enforce the same gate as a pre-push hook, opt into the versioned hooks:

```sh
git config core.hooksPath .githooks
```

## Use

Run `prism` from anywhere. On first launch with no repositories configured, Prism opens and shows an add-repository dialog. You can also add/select one directly with `prism --repo <path>`.

Press `?` in the TUI for the full key list.

Common keys:

- `Space Space` focuses repos from the status panel, focuses worktrees from the repos panel, or opens the selected agent from the worktrees panel when valid.
- `Enter` focuses repos from the status panel, focuses worktrees from the repos panel, or opens the selected agent from the worktrees panel when valid.
- `1`, `2`, and `3` focus the status, repos, and worktrees panels.
- `Tab` cycles focus between panels.
- `h` / `l` or left/right switches horizontal views in the repos panel.
- `Space Enter` or `Ctrl-/` opens tmux window 3: terminal.
- `Space g g` opens tmux window 2: lazygit.
- `Space g o` opens the selected pull request in a browser.
- `Space g P` pushes the selected branch and creates a pull request if needed.
- `Space g M` merges the selected pull request.
- `Space g a` starts or focuses Auto Flow for the selected non-default worktree.
- `Space g c` copies a CI-failure prompt with failed run metadata and log tails.
- `Space g f` copies a review-fix prompt.
- `P` opens plan mode in tmux from the selected repo or worktree, selects a Markdown plan with `fzf`, and runs each phase through `opencode run`.
- `p` or `Space g p` pulls the selected repository's default branch from the repos or worktrees panel.
- `Space 1`-`Space 9` switches repositories using configured repo keys.
- `A` adds a repository by path from the repos panel.
- `R` edits repository order, key bindings, and tracked repositories.
- `c` creates a worktree session from the repos panel.
- `x` aborts the selected OpenCode session from the worktrees panel.
- `e` edits the Prism repository config from the repos panel and reloads after save.
- `/` filters the focused panel.
- `D` confirms and deletes the selected non-default worktree/session.
- `r` refreshes the board.
- `j` / `k` or up/down moves selection.
- `q` or `Ctrl-C` quits.

## Configuration

Tracked repositories live in `~/.config/prism/repos.toml`:

```toml
[[repos]]
path = "/work/project-a"
key = "1"

[[repos]]
path = "/work/project-b"
key = "2"
```

Reorder blocks to change repo panel order. Remove a block to stop tracking a repository.
Repository keys are used as `Space <key>` shortcuts in the TUI.

Prism treats `main` as the default branch by default. The default branch is not
polled or shown as a pull request branch.

Prism uses squash merges for pull requests by default. Set `merge_method` to
`merge` or `rebase` if a repository requires a different GitHub merge method.

Set `default_base` in the user config or override it per repository in
`~/.config/prism/repos/<repo-name>-<hash>/config.toml`:

```toml
default_base = "develop"
merge_method = "squash"
opencode_port_base = 41000
opencode_port_span = 1000
opencode_shutdown_owned_servers = false

[worktrees]
columns = ["url", "vars.localdev", "ci"]

[prompt_templates]
review_fix = "Here are review comments on PR {pr_number}.\n\nIf they are applicable, fix them. Otherwise, say why not.\n\n---\n\n{comments}"
ci_failure = "Here are CI failures on PR {pr_number}.\n\nFix the failing checks. Use the log tails below as the primary clues.\n\nPR: {url}\nBranch: {branch}\nHead SHA: {head_sha}\n\n---\n\n{failures}"
```

Prism manages one local OpenCode server per worktree session. `opencode_port_base`
and `opencode_port_span` define the deterministic local port range used for
those servers. By default Prism keeps servers warm after the TUI exits; set
`opencode_shutdown_owned_servers = true` to send SIGTERM to OpenCode servers
that Prism spawned during the session.

### Auto Flow

Auto Flow automates one clean, non-default worktree from an initial prompt through
implementation, local verification, PR creation, automated review repair, CI
repair, and a final merge gate. Start it from the TUI with `Space g a`, or from
the CLI:

```sh
prism --repo /work/project auto "implement the task"
prism --repo /work/project auto plan "draft and review a plan before coding"
```

The CLI resumes the most recent active Auto Flow run for that repository before
starting a new one. Auto Flow state is stored in Prism's per-repository SQLite
database under `~/.config/prism/repos/...`, not in the project checkout.

Safety defaults:

- launch is refused on the default branch, detached HEAD, or a dirty worktree
- every step attempt is persisted before the next external side effect
- local verification runs `checks.pre_push`, `checks.pre_pr`, and a
  non-mutating merge-conflict check
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

If Prism exits or the machine restarts, rerun `prism` or `prism auto` for the
repository. Active Auto Flow steps are reconciled from the persisted run; stale
running attempts are marked failed so they can be retried instead of being
silently forgotten.

Troubleshooting:

- run `prism config` to inspect effective Auto Flow and check settings
- run `prism debug logs` or start with `--print-logs --log-level debug` for
  runtime events
- use the dashboard controls to pause/resume, retry a failed step, retry from a
  selected step, abort, or dismiss a completed run
- when automation pauses after plan review, inspect `plan.md` in the worktree
  and resume the run when the plan is acceptable
