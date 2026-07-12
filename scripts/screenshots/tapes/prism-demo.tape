Output "__OUTPUT__"
__FRAMES_OUTPUT__

Set Shell "bash"
Set Theme "Catppuccin Mocha"
Set FontFamily "JetBrainsMono Nerd Font"
Set FontSize 16
Set Width 1400
Set Height 920
Set Framerate 24
Set CursorBlink false
Set TypingSpeed 35ms

# Scene 1: create the focused CI worktree from the repository panel.
Type "./prism --repo ."
Enter
Wait+Screen@30s /no worktrees/
Sleep 500ms
Screenshot "__FRAMES_DIR__/01-opening.png"
Type "2"
Type "c"
Wait+Screen /Branch name for prism-shop:/
Type "feat/ci-plan"
Enter
Wait+Screen /Initial prompt/
Enter
Wait+Screen@60s /No PR detected/

# Scene 2: ask the real, locally configured OpenCode client to draft the plan.
Enter
Wait+Screen@60s /ctrl.p commands/
Type "Create plan-ci.md that investigates the repository CI/CD actions and proposes implementation steps."
Enter
Wait+Screen@60s /requested sandbox action completed successfully/
Ctrl+B
Type "d"
Wait+Screen@30s /Worktrees repo/
Sleep 500ms
Screenshot "__FRAMES_DIR__/02-worktree.png"

# Show the generated plan from the worktree's real terminal window.
Type " "
Enter
Sleep 500ms
Type "cat plan-ci.md"
Enter
Wait+Screen@30s /Phase 3: Verify The Change/
Sleep 1s
Screenshot "__FRAMES_DIR__/03-plan-drafted.png"
Ctrl+B
Type "d"
Wait+Screen@30s /Worktrees repo/

# Scene 3: run the selected plan with Prism's plan mode.
Type "P"
Wait+Screen@30s /Step name \[phase\]:/
Enter
Wait+Screen /How many total steps \[3\]:/
Enter
Wait+Screen /Where to start \[1\]:/
Enter
Wait+Screen /Run phases in parallel/
Enter
Wait+Screen@120s /status done/
Sleep 500ms
Screenshot "__FRAMES_DIR__/04-plan-complete.png"

# Scene 4: open real LazyGit and make the initial local implementation commit.
Type "3"
Type " "
Type "g"
Type "g"
Wait+Screen@60s /Files/
Wait+Screen@30s /Press <enter> to get started/
Enter
Type "a"
Wait+Screen@30s /Staged changes/
Type "c"
Wait+Screen /Commit summary/
Type "Implement checkout CI fixture"
Enter
Wait+Screen@60s /Implement checkout CI fixt/
Sleep 500ms
Screenshot "__FRAMES_DIR__/05-lazygit-commit.png"
Ctrl+B
Type "d"
Wait+Screen@30s /No PR detected/

# Scene 5: Auto Flow opens the PR and handles each trusted-repository gate automatically.
Type "A"
Wait+Screen /Implementation source/
Enter
Wait+Screen /Initial prompt:/
Type "Prepare the checkout pull request."
Enter
Wait+Screen@60s /auto: 1/
Type "3"
Wait+Screen@60s /ReviewFeedbackFound/
Sleep 500ms
Screenshot "__FRAMES_DIR__/06-failed-pr.png"

# Scene 6: review and CI repairs are verified, pushed, and merged without dialogs.
Wait+Screen@60s /CiFailed/
Sleep 500ms
Screenshot "__FRAMES_DIR__/07-review-pushed.png"

Wait+Screen@60s /ReadyToAutoMerge/
Sleep 500ms
Screenshot "__FRAMES_DIR__/08-ci-pushed.png"

Wait+Screen@60s /state[ ]+Merged/
Sleep 1s
Screenshot "__FRAMES_DIR__/09-merged.png"
Sleep 1s
