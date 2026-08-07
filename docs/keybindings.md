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
- `Space g M` launches the standard stabilization workflow, which rechecks the selected change request's gates before merging its exact head.
- `Space g c` launches stabilization for a selected change request with CI repair support.
- `Space g f` launches stabilization for a selected change request with review repair support.
- `o` from the Worktrees panel opens the selected Worktree Session's Worktrunk-configured HTTP(S) development URL. It remains available when listening is false, unknown, or stale; the details view reports that state.
- `Space g R` resolves all unresolved inline review conversations visible when the key is pressed while `0 Main` is focused.
- Unavailable `Space g` actions are shown in dark gray and ignored. Remote actions require a known change request and provider capability; repair actions also require a headless-capable harness.
- `W` opens `fzf` with manual Workflow Definitions compatible with the selected Repository,
  Worktree Session, or Change Request. The picker searches qualified ID, name, description, tags,
  and Step implementation metadata, then Prism collects any missing typed inputs before launch.
- `{` / `}` selects the previous/next Workflow Run linked to the current Worktree Session for
  independent parent, child, and iteration inspection.
- `Space W` opens workflow management for Workflow Definitions, Triggers, packages, extensions,
  skills, and templates. These destinations use the same operations as their CLI command families.
- `Space c` opens the unified configuration tree for global and repository settings, tracked
  repositories/keybindings, Worktrunk configuration, worktree columns, and Harness selection.
- `>` / `<` raises/lowers the selected worktree priority.
- `u` pauses or resumes the selected Workflow Run.
- `f` retries the selected failed Workflow Step as a new Attempt against the same input revisions.
- `B` restarts from the selected Step after previewing downstream invalidation.
- `s` is offered only for a Step whose Definition explicitly marks it skippable and previews the
  downstream invalidation before applying it.
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
