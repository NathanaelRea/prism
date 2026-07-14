Output "__FRAMES_DIR__/"

Set Shell "bash"
Set Theme "Catppuccin Mocha"
Set FontFamily "JetBrainsMono Nerd Font"
Set FontSize 18
Set Width 1200
Set Height 760
Set CursorBlink false
Set TypingSpeed 1ms

Type "./prism --repo ."
Enter
Type "3"
Wait+Screen@30s /fix\/review-comments/
Type "j"
Type "j"
Sleep 1s
Screenshot "__OUTPUT__"
