# Prism

```text
░▒▓█▓▒░ P ◤◥◣◢◤◥◣
▒▓█▓▒░▒ R ◥◣◢◤◥◣◢
▓█▓▒░▒▓ I ◣◢◤◥◣◢◤
█▓▒░▒▓█ S ◢◤◥◣◢◤◥
▓▒░▒▓█▓ M ◤◥◣◢◤◥◣
```

Prism is a terminal board for running agent-backed coding sessions across Git worktrees.

It gives you one place to manage local worktrees in different stages of progress, watch GH PR comments and CI status. It uses tmux as the backbone for persistent OpenCode runs in the background, and enables you to quickly switch between sessions.

The TUI uses Status, Repos, and Worktrees sidebars with a contextual main panel. `Enter` and `Space Space` move deeper through the board: Status operates the active dashboard or focuses Repos, Repos focuses Worktrees, and Worktrees opens the selected non-default branch agent session.

## Demo

![Prism demo](docs/prism-demo.gif)

## What it enables

It's basically a local dashboard to manage agent implementation. From planning, impl, PR, CI checks, CR comments and fixes. All this for different things in different stages or progress.

- Work on multiple worktrees at once without juggling terminal tabs.
- Keep each task in its own Git worktree and tmux-backed agent session.
- See repository, worktree, pull request, CI, and agent state in one TUI.
- Kick off repeatable agent flows for implementation, review repair, CI repair, and merge readiness.
- Kick off automatic flows from prompt or plan through reviewed and CI-validated ready to merge PR

## Configuration Highlights

Prism has per-repository config for TUI layout and worktree columns. For example:

```toml
[layout]
sidebar_width = 56

[worktrees]
columns = ["url", "ci.status", "vars.localdev"]
```

`sidebar_width` is reduced automatically on narrow terminals so the main panel remains usable. Worktree columns come from built-in Prism status and `wt list --format=json`; the selected worktree detail panel lists all loaded `wt` keys for discovery.

## Prerequisites

Prism expects these tools to be installed and available on your `PATH`:

- Rust/Cargo for install
- `git`
- GitHub CLI (`gh`)
- `tmux`
- WorkTrunk (`wt`)
- `opencode`
- `fzf`

## Install

```sh
./install.sh
```

## Start

```sh
prism
```

On first launch, Prism will ask you to add a repository. After that, use the in-app help when you need the full control list.

## Learn More

- [Keybindings](docs/keybindings.md)
- [Configuration](docs/config.md)
- [Auto Flow](docs/auto-flow.md)
- [Demo GIF notes](docs/prism-demo.md)
- [Development](docs/development.md)
