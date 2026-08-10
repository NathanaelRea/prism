# Native Windows interactive smoke checklist

Run this checklist on x86-64 Windows 10 and Windows 11 before promoting a release. Use PowerShell 7, native Git, Worktrunk 0.71.0 (`git-wt.exe`), psmux 3.3.7, and OpenCode 1.17.20. No WSL, MSYS2, or Cygwin process may be in the Prism process tree.

## Automated baseline

```powershell
scripts/windows-check.ps1
scripts/windows-platform-smoke.ps1
```

Confirm the release `.zip` checksum, extract it into a path containing spaces and non-ASCII characters, and run the extracted `prism.exe --help`.

## Windows Terminal

- Start Prism from PowerShell 7 and confirm alternate-screen entry, Unicode rendering, mouse selection/click handling, focus events, bracketed paste, and resize redraws.
- Open and detach from an agent psmux session twice. Confirm Prism restores raw mode, cursor visibility, mouse mode, and the alternate screen after each return.
- Resize while attached to psmux. Confirm the attached ConPTY owns the new size and Prism redraws correctly after detach; do not infer psmux size from `resize-window` success alone.
- Send Ctrl+C to Prism and confirm a bounded clean shutdown. Repeat while attached to psmux and confirm the control event is handled by the foreground attachment rather than leaving Prism or agent descendants behind.
- Trigger a controlled panic and confirm cursor, input echo, mouse mode, bracketed paste, and the original screen are restored.
- Exercise a repository and worktree path containing a drive letter, spaces, Unicode, and a path longer than 260 characters.

## Classic console

- Repeat startup, keyboard input, resize, psmux attach/detach, Ctrl+C, and terminal restoration in the classic console where its ConPTY and Unicode capabilities permit.
- Record unsupported rendering behavior as a console limitation; it must not corrupt terminal state or crash Prism.

## Desktop integration

- Open an HTTP and HTTPS pull-request URL and confirm Windows opens the registered browser through `ShellExecuteW` without displaying a shell window.
- Confirm unavailable desktop notifications remain non-fatal. Native Windows toast notifications are not currently claimed because unpackaged-CLI attribution has not been proven.

Record the Windows build, terminal versions, psmux/OpenCode versions, checklist result, and any limitations with the release evidence.
