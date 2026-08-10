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
- `Enter` and `Space Space` share the same go-deeper behavior from Repos and Worktrees: Repos opens the selected repository's default tmux session, and Worktrees opens the selected agent session when valid. Status has no `Enter` action.
- Default branch worktrees are not agent targets; `Enter` and `Space Space` show the same blocked message there.
- `Space Enter` opens tmux window 3: terminal.
- `Ctrl-/` also opens tmux window 3 where the terminal reports that key combination distinctly; use `Space Enter` as the reliable alternative.
- `Space g g` opens tmux window 2: lazygit.
- `Space g P` pushes the selected task branch after verifying that its branch, head, and destination have not changed.
- `Space g o` opens the selected change request in a browser.
- `Space g M`, `Space g c`, and `Space g f` launch the editable `stabilize` Workflow for the selected Change Request. Stabilization repairs conflicts, review feedback, and CI as needed, then stops at ready-to-merge without merging or cleaning up.
- `o` from the Worktrees panel opens the selected Worktree Session's Worktrunk-configured HTTP(S) development URL. It remains available when listening is false, unknown, or stale; the details view reports that state.
- `Space g R` resolves all unresolved inline review conversations visible when the key is pressed while `0 Main` is focused.
- Unavailable `Space g` actions are shown in dark gray and ignored. Remote actions require a known change request and provider capability; repair actions also require a headless-capable harness.
- `W` and `Space W` open one flat `fzf` picker of hot-discovered prompt Workflows for the selected Worktree Session. `Enter` runs the selected Workflow and `Ctrl-E` edits its source. The picker shows source scope and path.
- `{` / `}` selects the previous/next Workflow Run linked to the current Worktree Session.
- `Space c` opens the unified configuration tree for global and repository settings, tracked
  repositories/keybindings, Worktrunk configuration, worktree columns, and Harness selection.
- `>` / `<` raises/lowers the selected worktree priority.
- `u` pauses or resumes the selected Workflow Run.
- `f` retries the selected failed Workflow Step as a new Attempt without restoring consumed Agent budget.
- `p` or `Space g p` pulls the selected repository's default branch from the Repos or Worktrees panel.
- `Space 1` through `Space 9` jump to configured repositories.
- `r` opens the repository order dialog from the Repos panel. Use `Space` to mark repositories for removal, `J`/`K` to move them down/up, and `Enter` to save. Removals require a second confirmation.
- `c` creates a worktree session from the Repos panel.
- `x` cancels the selected Workflow Run when one is shown; otherwise it aborts the selected agent
  session from the Worktrees panel when its adapter supports native session cancellation.
- `M` migrates the selected Worktree Session to the current global harness, including a worktree previously pinned with `Keep`.
- Choice dialogs keep unavailable actions visible in dark gray. Their keys are ignored without closing the dialog.
- `C` opens a picker of remote change requests for the selected repository and creates or selects a local worktree using the request's head branch name. An existing local branch is reused without resetting it.
- `L` from the Worktrees panel opens the selected repository's Worktrunk hook-log picker and a bounded sanitized log tail. A matching branch label affects ordering only and does not assert session identity or process liveness.
- `/` filters the focused Repos or Worktrees panel.
- `?` opens the in-app keybinding dialog.
- `D` archives the selected non-default worktree/session, hiding it from normal navigation while leaving files and branch intact.
- `U` opens a picker of archived worktrees for the selected repository and restores the chosen one.
- `X` permanently deletes the selected non-default worktree/session after explicit confirmation.
- `r` refreshes cached repository, worktree, change-request, and agent state outside the Repos panel.
- `q` or `Ctrl-C` quits.

Most repository actions are only active from the Repos panel. Pulling the default branch and editing worktree columns are active from the selected repository context. Worktree actions are only active from the Worktrees panel.
