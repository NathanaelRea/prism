# Repositories And Board

## Tracked Repositories

- **Behavior**: Users can add a repository by selecting a path within its Git
  working tree; Prism resolves and tracks the repository root without duplicating
  an existing entry.
- **Behavior**: Users can reorder tracked repositories and remove them from
  tracking. Removing tracking does not delete the repository, and requires
  confirmation before the new ordering is saved.
- **Customization**: Repository order, membership, default branch, and optional
  single-character repository shortcuts are persisted configuration.
- **Behavior**: Startup reconciles configured repositories and Git's live
  worktree inventory with persisted Prism state. Repositories or worktrees that
  no longer exist are removed from the active view.
- **Behavior**: A dirty Default Branch checkout does not block ordinary startup.
  Prism only refuses an operation when that operation would need to modify a
  checkout whose state makes the change unsafe.
- **Behavior**: When launched in a TTY from a clean non-default main checkout,
  startup may offer to restore the Default Branch there and move the active
  branch into a Worktrunk worktree. It does not make this offer non-interactively
  and refuses to move a dirty checkout.
- **Default**: GitHub polling for an inactive repository is reduced to no more
  often than once every 60 seconds.
- **Behavior**: Users can pull the selected repository's configured Default
  Branch. Before creating a Worktree Session, Prism detects when that branch is
  behind its remote and offers to update it; declining does not block creation.
- **Behavior**: From a repository, users can select an open pull request and
  open it as a Worktree Session. Prism reuses an existing matching worktree or
  fetches the pull request into a deterministic local branch, then retains the
  selected pull-request summary on the focused session.

## Navigation

- **Behavior**: The left side presents vertically stacked numbered panels:
  `[1]` home/status, `[2]` repositories, and `[3]` worktrees. `[2]` is selected
  on startup.
- **Behavior**: The home view presents Prism identity and project/help links. The
  repository and worktree views update the contextual main panel for their
  current selection.
- **Behavior**: `j`/`k`, arrows, Tab, and Shift-Tab provide forward and reverse
  vertical/focus traversal. Horizontal controls change contextual views rather
  than cycling through numbered panels.
- **Behavior**: From any numbered panel, `0` focuses its corresponding main-panel
  content. Main-panel content is scrollable when it exceeds available space.
- **Behavior**: Repositories and worktrees can be selected with the mouse as well
  as the keyboard.
- **Behavior**: Search filters repositories by label, path, or shortcut and
  filters Worktree Sessions by branch, repository, prompt summary, path, or
  displayed Worktrunk values. Changing filters preserves a valid selection when
  possible.
- **Behavior**: Enter on a repository attaches to that repository's Default
  Branch Agent Session rather than merely moving focus to `[3]`.

## Worktree List

- **Behavior**: `[3]` offers visible repository-scoped and all-repositories modes,
  switched with `[` and `]`. Repository-scoped rows omit redundant repository
  names.
- **Default**: The last chosen worktree-list mode is remembered globally.
- **Behavior**: Global worktrees sort by user priority, then repository name,
  then branch/worktree name. Neutral priority is represented by `.`.
- **Customization**: Users can change worktree priority and enable, disable, and
  reorder worktree columns with a reusable ordering dialog. No URL column is
  enabled implicitly.
- **Behavior**: Worktree rows use stable compact columns and state-dependent
  symbols/colors for configured facts such as activity, pull request, CI,
  review comments, merge conflicts, and Worktrunk data. Success indicators are
  green and merge conflicts have a distinct recognizable indicator.
- **Behavior**: The Default Branch sorts first when present in `[3]`, suppresses
  task-only activity and GitHub indicators, and is not an attach target from that
  panel. Suppressed indicators retain their column width so other rows remain
  aligned.
- **Behavior**: Contextual help for repository and worktree views explains their
  symbols and columns, is scrollable, and does not obscure search results.
- **Default**: Active-repository pull-request summaries refresh no more often
  than every 15 seconds, inactive summaries every 60 seconds, selected details
  every 30 seconds, and Default Branch status every 60 seconds. Regaining
  terminal focus requests a refresh, and only one refresh per target is in
  flight at a time.

## Visual Interaction

- **Behavior**: Panel headings always include their number. The Prism logo,
  active panel border, dialogs, and primary accents use cyan.
- **Behavior**: Focused selection uses dark cyan. Unfocused selection remains
  visible through its bars/treatment and bold selected text without looking
  focused.
- **Invariant**: Selection treatment occupies exactly one row, preserves status
  symbol readability, and never shifts surrounding content.
- **Behavior**: Choice dialogs place options on separate lines with consistent
  accents and no redundant instruction clutter.
- **Behavior**: Confirmation prompts use conventional `[Y/n]` or `[y/N]`
  notation, require an entered answer followed by Enter, and report invalid input
  inline without opening a second prompt.
- **Customization**: Each confirmation caller chooses and displays its default.
