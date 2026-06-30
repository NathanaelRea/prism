# Keybindings

Prism uses a lazygit-style panel model.

- `1` focuses Status.
- `2` focuses Repos.
- `3` focuses Worktrees / Sessions.
- `Tab` cycles focus between panels.
- `h` / `l` or left/right switches horizontal views in the Repos panel, and switches the Worktrees main panel between details and the active plan dashboard when a plan run exists.
- `j` / `k` or up/down moves within the focused row panel.
- `Space Space` opens tmux window 1 for the current plan phase from Status when available; otherwise it focuses the next useful panel or opens the selected agent from Worktrees when valid.
- `Space Enter` or `Ctrl-/` opens tmux window 3: terminal.
- `Space g g` opens tmux window 2: lazygit.
- `Space g o` opens the selected pull request in a browser.
- `Space g P` pushes the selected branch and creates a pull request if needed.
- `Space g M` merges the selected pull request.
- `Space g c` copies a CI-failure prompt.
- `Space g f` stages a review-fix prompt.
- `P` opens plan mode in tmux from the selected repo or worktree, selects a Markdown plan with `fzf`, and runs each phase through `opencode run`.
- `A` starts or focuses Auto Flow for the selected non-default worktree.
- `u` pauses/resumes the selected Auto Flow or plan run from Status; paused Auto Flow resumes only after a dialog describes the next step.
- `p` or `Space g p` pulls the selected repository's default branch from the Repos or Worktrees panel.
- `Space 1` through `Space 9` jump to configured repositories.
- `R` edits repository order, key bindings, and tracked repositories.
- `c` creates a worktree session from the Repos panel.
- `x` aborts the selected OpenCode session from the Worktrees panel.
- `e` edits the Prism repository config from the Repos panel and reloads after save.
- `/` filters the focused Repos or Worktrees panel.
- `?` opens the in-app keybinding dialog.
- `D` confirms and deletes the selected non-default worktree/session.
- `r` refreshes cached repository, worktree, PR, and agent state.
- `q` or `Ctrl-C` quits.

Most repository actions are only active from the Repos panel. Pulling the default branch is active from the Repos and Worktrees panels. Worktree actions are only active from the Worktrees panel.
