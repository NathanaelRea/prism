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

## Install

```sh
./install.sh
```

Requires Rust/Cargo, `git`, `gh`, `tmux`, `wt`, and `opencode`.

## Use

Run `prism` from anywhere. On first launch with no repositories configured, Prism opens and shows an add-repository dialog. You can also add/select one directly with `prism --repo <path>`.

Press `?` in the TUI for the full key list.

Common keys:

- `Space Space` or `Enter` opens the selected tmux session.
- `Space Enter` or `Ctrl-/` opens tmux window 3: terminal.
- `Space g g` opens tmux window 2: lazygit.
- `Space g P` pushes the selected branch and creates a pull request if needed.
- `Space g M` merges the selected pull request.
- `Space g f` copies a review-fix prompt.
- `p` or `Space g p` pulls the default branch.
- `1`-`9` switches repositories.
- `A` adds a repository by path.
- `R` edits repository order, key bindings, and tracked repositories.
- `c` creates a worktree session.
- `e` edits the Prism repository config and reloads after save.
- `/` filters sessions.
- `D` confirms and deletes the selected session.
- `r` refreshes the board.
- `j` / `k` or arrow keys move selection.
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

Reorder blocks to change left-panel order. Remove a block to stop tracking a repository.

Prism treats `main` as the default branch by default. The default branch is not
polled or shown as a pull request branch.

Prism uses squash merges for pull requests by default. Set `merge_method` to
`merge` or `rebase` if a repository requires a different GitHub merge method.

Set `default_base` in the user config or override it per repository in
`~/.config/prism/repos/<repo-name>-<hash>/config.toml`:

```toml
default_base = "develop"
merge_method = "squash"

[worktrees]
columns = ["url", "vars.localdev", "ci"]

[prompt_templates]
review_fix = "Here are review comments on PR {pr_number}.\n\nIf they are applicable, fix them. Otherwise, say why not.\n\n---\n\n{comments}"
```
