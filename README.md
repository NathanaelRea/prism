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

## Demo

![Prism demo](docs/prism-demo.gif)

## What it enables

It's basically a local dashboard to manage agent implementation. From planning, impl, PR, CI checks, CR comments and fixes. All this for different things in different stages or progress.

- Work on multiple worktrees at once without juggling terminal tabs.
- Keep each task in its own Git worktree and tmux-backed agent session.
- See repository, worktree, pull request, CI, and agent state in one TUI.
- Kick off repeatable agent flows for implementation, review repair, CI repair, and merge readiness.
- Kick off automatic flows from prompt or plan through reviewed and CI-validated ready to merge PR
- Send review-fix and CI-failure prompts from `Space g f` and `Space g c` directly into the selected agent session.

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
