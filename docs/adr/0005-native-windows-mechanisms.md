# ADR 0005: Native Windows Mechanisms

## Status

Accepted

Supersedes ADR 0004's decision that the supported operating-system set is permanently closed to Linux and macOS. Native `x86_64-pc-windows-msvc` is now part of the supported package contract and is guarded by the required Windows CI, compatibility smoke, and release-archive jobs.

## Context

Prism intends to support native `x86_64-pc-windows-msvc` without WSL, Cygwin, or MSYS2. Its Unix implementations own process-tree supervision, local IPC, best-effort recorder datagrams, file locks and identity, atomic persistence, private runtime paths, and the tmux command boundary. A successful cross-compile cannot establish the Windows behavior these contracts require.

Phase 0 established a standalone native spike crate under `spikes/windows`. It remains as focused mechanism and psmux contract evidence. `.github/workflows/windows-feasibility.yml` runs the crate on `windows-2022`; `scripts/windows-phase0-spikes.ps1` is the equivalent local entry point. The root package now contains the selected production backends and `.github/workflows/ci.yml` runs the complete native Windows gate.

The spikes use real process and crash boundaries rather than mocks. psmux is downloaded as an x64 archive at version 3.3.7 with SHA-256 `60ff7b236f64184921cef3c1ff2611aa5a36fcc7ed8e2a58e968b8ded57f6028`.

## Decision

Windows backends will remain in their capability-owning modules. Prism will not add a broad `Platform` trait.

### Process supervision and identity

Use ProcessKit 3.3.1 as Prism's general process supervisor on every supported operating system. ProcessKit owns spawn-time containment, asynchronous stdin and output draining, cancellation and timeout escalation, leader reaping, and kill-on-drop. On Windows its `process-control` backend assigns children to a Job Object before they can escape containment. Prism does not maintain a parallel general-purpose raw-child or Job Object supervisor.

Two long-lived process exceptions have narrower ownership contracts. The prompt worker is an exception to kill-on-drop: `Command::spawn_detached` starts it with null stdio and, on Unix, an independent session, returns its PID immediately for telemetry, and lets ProcessKit reap it on Unix. A shared OpenCode server must also survive the short-lived CLI that first ensures an agent session. Unix launches that server in an independent session and retains startup teardown authority until its identity and health are committed. Windows launches a detached hidden Prism supervisor with `CreateProcessW`, `bInheritHandles = FALSE`, and a retained process handle. Preventing handle inheritance is required so the durable supervisor cannot keep a calling PowerShell capture pipe open. The supervisor owns the actual OpenCode process through the normal ProcessKit Job Object; the persisted PID and start identity refer to the supervisor. Startup failure terminates through the retained handle, successful commit closes it, and later recovery validates the exact identity and current Prism image before shutdown. This is a lifecycle adapter for one durable service, not an alternate child-process framework.

Other native exceptions remain narrow and do not replace the general supervisor. First, attached-terminal execution on Unix retains the code required to create a foreground process group, transfer controlling-terminal ownership with `tcsetpgrp`, restore ownership, and reap the attached leader; Windows attached execution remains ProcessKit-managed with Prism's console Ctrl-C ownership guard. Second, restart recovery retains direct process identity observation and, where the operating system exposes it through documented interfaces, argv observation. On Windows this uses `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE)`, `GetProcessTimes`, image-path validation, and a nonblocking handle wait; the full argv is unavailable through that contract. Unix uses its native process metadata, including argv. Prism persists the observed start identity with the PID and rejects reuse or unverifiable identity. Windows recovery cleanup therefore fails closed when full command intent would be required; it does not scrape undocumented PEB structures or claim full argv verification. Persisted cleanup never adopts an external PID into a new ProcessKit group because adoption could alter containment before identity intent is established.

### Worker IPC and ownership

Use `interprocess` 2.4.3 Tokio local sockets. They map to Unix sockets on existing hosts and named pipes on Windows while retaining Prism's external framed protocol. Windows listeners receive a protected DACL granting full access only to the current user and LocalSystem. Every connection must also authenticate with a cryptographically random per-run secret before parsing a request or subscription frame.

Use an adjacent, permanent `fs4` lock file for single-owner election. Endpoint names are disposable; lock-file paths are not. Rebinding a name after listener drop and concurrent request/response and subscriber clients are native spike contracts.

### Flight recorder transport

Use nonblocking IPv4 loopback UDP datagrams on Windows with a random per-run secret in every packet, a fixed protocol version, and a 4 KiB event limit. Producers perform one nonblocking send and explicitly drop oversize or backpressured telemetry. Receivers bind only to `127.0.0.1`, use a fixed-size receive buffer, and reject packets with an invalid secret or version.

Do not use `interprocess` message-mode named pipes for the recorder. In 2.4.3, Tokio message reading is disabled because Mio does not expose the `ERROR_MORE_DATA` behavior needed to preserve message boundaries. Byte-stream local sockets would require independent framing and subscriber lifecycle semantics and would not preserve the recorder's deliberately lossy datagram contract.

The secret, not an unguessable UDP port, is the authentication boundary. Recorder loss remains non-fatal and bounded as it is on Unix.

### Session runtime

Keep the external command boundary and use psmux on Windows. Version 3.3.7 is the phase 0 compatibility pin. The spike proves detached session creation and rename, UTF-8 capture, Unicode buffer load/paste, command-based resize acceptance, real ConPTY attachment, terminal-driven resize, detach, kill, and namespace-scoped cleanup from a Rust parent.

psmux 3.3.7 intentionally accepts `resize-window` as a no-op because the attached terminal owns its dimensions. Prism must not treat a successful Windows `resize-window` exit as evidence that the dimensions changed. Windows attach drives size through ConPTY, and detached capture observes psmux's actual reported dimensions. This is an explicit session-runtime policy difference to cover in the later command contract suite, not a stderr-string heuristic.

`portable-pty-psmux` 0.9.6 is used only by the disposable spike to create a real ConPTY client on a headless runner. Its ConPTY flags match psmux's terminal expectations; upstream `portable-pty` 0.9.0 sets `PSEUDOCONSOLE_INHERIT_CURSOR` and blocks headless startup waiting for a cursor-position reply. The fork is not selected as a Prism production dependency; Prism continues to execute psmux as an external process.

### Persistence, identity, and locking

Stage replacement bytes adjacent to the target, flush the staging file, atomically commit with `ReplaceFileW`, and flush the resulting target handle. Use `file-id` 0.2.3's volume/file identity and retain identity checks across replacement. The spike proves that the path changes identity and contains the new bytes while a pre-commit open handle continues to observe the old bytes.

Use `fs4` 1.1.0 for nonblocking cross-process file locks. The spike proves exclusivity and release after process death. Replacing a lock file can create a new lock domain even while an old handle remains locked, so lock files are permanent adjacent coordination objects and must never be atomically replaced.

Windows does not provide a directly equivalent, documented parent-directory `fsync` contract. Prism therefore guarantees a flushed adjacent staging file, atomic `ReplaceFileW` for an existing destination (or write-through `MoveFileExW` for its first generation), and `FlushFileBuffers` through the committed file handle. It does not claim a parent-directory flush. Managed configuration, lock, and staging paths reject reparse points (including final configuration symlinks), lock files remain permanent, transient cleanup sharing violations receive only bounded retries, and crash-boundary tests require the path to contain the complete old or complete new generation.

### Private runtime ACLs

Create runtime directories and files with, or immediately apply, a protected DACL containing exactly allow entries for the current user and LocalSystem. Do not grant Builtin Users, Authenticated Users, Interactive Users, or Everyone. Verify the applied descriptor through Windows security APIs rather than inferring privacy from a path under the user profile.

The same DACL construction is used for named-pipe listeners. Production path setup opens each ancestor with `FILE_FLAG_OPEN_REPARSE_POINT` while withholding delete sharing, keeps those handles pinned through the final open, rejects reparse attributes, applies a protected DACL through the final file handle, and then verifies the owner, inheritance protection, exact principals, ACE type, and access mask through that same handle. An existing unsafe descriptor is repaired before use; a path with the wrong owner or a reparse point fails closed.

## Verification

On native x86-64 Windows with PowerShell 7:

```powershell
scripts/windows-phase0-spikes.ps1
```

The script verifies the pinned psmux archive, formatting, Clippy with warnings denied, and all seven runtime spikes. The required root-package and full-stack gates are:

```powershell
scripts/windows-check.ps1
scripts/windows-platform-smoke.ps1
```

The first runs formatting, SQLx-offline compilation, Clippy, normal/native contracts, and archive installation verification. The second runs pinned psmux, real Git/Worktrunk, and no-model Prism/OpenCode/psmux compatibility smoke tests.

Linux can compile-check the mechanisms after installing the MSVC standard library target, but this is only an early type signal:

```sh
rustup target add x86_64-pc-windows-msvc
cargo check --locked --manifest-path spikes/windows/Cargo.toml --target x86_64-pc-windows-msvc
cargo clippy --locked --manifest-path spikes/windows/Cargo.toml --target x86_64-pc-windows-msvc -- -D warnings
```

## Consequences

- Later phases have concrete Windows mechanisms and executable contracts rather than placeholder `cfg(windows)` branches.
- Linux and macOS production behavior and dependencies are unchanged in phase 0.
- Windows support is gated by native root-package tests, the pinned psmux/OpenCode/Worktrunk smoke, archive installation verification, and the focused manual interactive checklist in `docs/windows-interactive-smoke.md`.
- ProcessKit is the single general process supervisor; its Windows Job Objects provide deterministic forced cleanup, while graceful console events are best effort and bounded.
- Native process code is limited to Unix attached-terminal ownership, reuse-safe persisted-identity recovery, and the retained startup handle for the durable Windows OpenCode supervisor.
- Worker IPC and recorder telemetry intentionally use different transports because their delivery contracts differ.
- psmux's terminal-owned sizing is a known contract difference that subsequent adapter and TUI work must preserve explicitly.
- File replacement and lock ownership must use separate permanent paths.
- Power-loss durability and hostile existing-path recovery remain phase 4 work; they are not silently claimed by the phase 0 spike.
