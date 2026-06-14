# Prism

Prism is a terminal board for running agent-backed coding sessions across Git worktrees.

Use it to create branch worktrees, open persistent agent sessions, watch pull request state, and keep multiple coding tasks moving from one TUI.

## Install

```sh
./install.sh
```

Requires Rust/Cargo, `git`, `gh`, `tmux`, `wt`, and `opencode`.

## Use

Run `prism` from a Git repository.

Press `?` in the TUI for the full key list.

Common keys:

- `Space Space` opens the selected agent session.
- `Space Enter` opens tmux window 3: terminal.
- `Space g g` opens tmux window 2: lazygit.
- `Space g P` pushes the selected branch and creates a pull request if needed.
- `Space g M` merges the selected pull request.
- `Space g f` copies a review-fix prompt.
- `p` or `Space g p` pulls the default branch.
- `c` creates a worktree session.
- `e` edits the Prism repository config and reloads after save.
- `/` filters sessions.
- `D` confirms and deletes the selected session.
- `r` refreshes the board.
- `j` / `k` or arrow keys move selection.
- `q` or `Ctrl-C` quits.

## Configuration

Prism treats `main` as the default branch by default. The default branch is not
polled or shown as a pull request branch.

Set `default_base` in the user config or override it per repository in
`~/.config/prism/repos/<repo-name>-<hash>/config.toml`:

```toml
default_base = "develop"

[worktrees]
columns = ["url", "vars.localdev", "ci"]
```
