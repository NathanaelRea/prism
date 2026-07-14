# Keybindings

Prism uses a lazygit-style panel model.

- `1` focuses Status.
- `2` focuses Repos.
- `3` focuses Worktrees / Sessions.
- `[` switches Worktrees to all-repos mode; `]` switches it to repo-scoped mode.
- `Tab` cycles focus between panels.
- `0` focuses the main panel for the selected sidebar context.
- `h` / `l` or left/right switches horizontal views in the Repos main panel.
- `j` / `k` or up/down moves within the focused row panel.
- `g g` jumps to the top of the focused list.
- `G` jumps to the bottom of the focused list.
- `Enter` and `Space Space` share the same go-deeper behavior from Repos and Worktrees: Repos opens the selected repository's default tmux session, and Worktrees opens the selected agent session when valid. Status has no `Enter` action. From a focused Worktrees plan dashboard, `Enter` switches the worktree OpenCode runtime to the selected phase session and opens tmux.
- Default branch worktrees are not agent targets; `Enter` and `Space Space` show the same blocked message there.
- `Space Enter` opens tmux window 3: terminal.
- `Ctrl-/` also opens tmux window 3 where the terminal reports that key combination distinctly; use `Space Enter` as the reliable alternative.
- `Space g g` opens tmux window 2: lazygit.
- `Space g o` opens the selected pull request in a browser.
- `Space g P` pushes a guarded pending PR Stabilization repair commit and continues stabilization. If no pending push exists, Prism reobserves the selected Worktree Session and reports the current blocker/next work.
- `Space g M` merges the selected pull request.
- `Space g c` starts or appends a managed PR Stabilization CI repair for the selected worktree.
- `Space g f` starts or appends a managed PR Stabilization review repair for the selected worktree.
- `P` opens plan mode from the selected repo or worktree, selects a Markdown plan with `fzf`, and runs each phase through `opencode run`. Active plan runs render automatically in the Worktrees main panel for the selected worktree.
- `A` starts or focuses Auto Flow for the selected non-default worktree.
- `u` pauses/resumes the selected Auto Flow or plan run from Status or the Worktrees main panel; paused Auto Flow resumes only after a dialog describes the next step.
- `f` retries failed Auto Flow or Plan steps from the active dashboard.
- `B` retries Auto Flow or Plan execution from the selected step.
- `s` skips the selected Plan step from the active Plan dashboard.
- `p` or `Space g p` pulls the selected repository's default branch from the Repos or Worktrees panel.
- `Space 1` through `Space 9` jump to configured repositories.
- `r` opens the repository order dialog from the Repos panel. Use `Space` to mark repositories for removal, `J`/`K` to move them down/up, and `Enter` to save. Removals require a second confirmation.
- `R` edits repository order, key bindings, and tracked repositories in `repos.toml`.
- `c` creates a worktree session from the Repos panel.
- `x` aborts the selected OpenCode session from the Worktrees panel.
- `x` also aborts the selected Plan phase from the active Plan dashboard, or accepts `all` when prompted to abort all running phases.
- `e` edits the selected Prism repository config and reloads after save.
- `E` edits the Prism user config and reloads after save.
- `C` opens a picker of remote pull requests for the selected repository and creates or selects a local `pr/<number>` worktree for the chosen PR.
- `W` opens the in-app worktree column selector for the selected repository. Use `Space` to enable/disable a column, `J`/`K` to move an enabled column down/up, and `Enter` to save.
- `/` filters the focused Repos or Worktrees panel.
- `?` opens the in-app keybinding dialog.
- `D` archives the selected non-default worktree/session, hiding it from normal navigation while leaving files and branch intact.
- `U` opens a picker of archived worktrees for the selected repository and restores the chosen one.
- `X` permanently deletes the selected non-default worktree/session after explicit confirmation.
- `r` refreshes cached repository, worktree, PR, and agent state outside the Repos panel.
- `q` or `Ctrl-C` quits.

Most repository actions are only active from the Repos panel. Pulling the default branch and editing worktree columns are active from the selected repository context. Worktree actions are only active from the Worktrees panel.
