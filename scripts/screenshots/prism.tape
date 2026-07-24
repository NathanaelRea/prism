Output "__FRAMES_DIR__/"

Set Shell "bash"
Set Theme "Catppuccin Mocha"
Set FontFamily "JetBrainsMono Nerd Font"
Set FontSize 18
Set Width 1600
Set Height 1140
Set CursorBlink false
Set TypingSpeed 1ms

Type "./prism --repo ."
Enter
Sleep 500ms
Type "3"
Wait+Screen@30s /fix\/review-comments/
Type "j"
Sleep 250ms
Type "j"
Wait+Screen@10s /rerun.*smoke.*check/
Sleep 500ms
Screenshot "__OUTPUT__"
