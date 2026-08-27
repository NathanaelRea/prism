# Native Windows feasibility spikes

This standalone crate exercises the phase 0 contracts in `plan-windows.md` without changing Prism's supported-target declaration. The root crate still intentionally rejects Windows. The spike crate is separate so it can run natively while that boundary remains in place.

Run it from PowerShell 7 on x86-64 Windows 10/11:

```powershell
scripts/windows-phase0-spikes.ps1
```

The script downloads the pinned psmux 3.3.7 x64 archive, verifies its SHA-256 checksum, checks formatting and Clippy, and runs all spikes. Set `PRISM_WINDOWS_SPIKE_PSMUX` to an existing `psmux.exe` to skip the download; it must report version 3.3.7.

The executable covers:

- a `process-wrap` Tokio Job Object containing a nested Job Object, `CTRL_BREAK_EVENT` graceful cancellation, forced whole-job termination, kill-on-drop, descendant liveness, and creation-time identity mismatch;
- concurrent framed request/response and subscriber traffic over authenticated `interprocess` Tokio local sockets with a current-user/LocalSystem pipe DACL;
- authenticated, size-bounded, nonblocking loopback UDP datagrams for the flight recorder;
- psmux detached creation, rename, Unicode prompt delivery, capture, `resize-window` command acceptance, real ConPTY attach/resize/detach, and cleanup;
- `ReplaceFileW`, file flushing, replacement identity through `file-id`, and old-handle behavior;
- `fs4` nonblocking exclusivity, crash release, and lock-file replacement behavior; and
- protected current-user/LocalSystem ACLs on a runtime directory and file.

Helper modes beginning with `--` are implementation details used to create real process and crash boundaries. Run the executable without arguments.
