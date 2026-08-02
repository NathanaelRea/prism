# ADR 0004: Supported Platform Contract

## Status

Accepted

## Context

Prism has Unix-oriented implementation details, but isolated fallback branches
previously made it appear to support Windows or any Unix host. That is not the
product contract. Prism depends on native process groups, Unix-domain sockets,
tmux, and supported durability primitives. Cross-compiling can find Rust type
and lint failures, but it cannot execute Darwin syscalls or establish their
error behavior.

Docker, `act`, Darling, and non-Apple macOS virtualization are therefore not
native macOS correctness signals. Docker shares the Linux host kernel, and a
Darwin cross-check does not run the produced executable.

## Decision

Linux and macOS are Prism's only supported operating systems. `build.rs` rejects
other target operating systems with an intentional diagnostic before Prism's
Unix-specific modules are compiled. `Cargo.toml` records the same support set as
package metadata.

Platform policy uses the closed `SupportedOs::{Linux, MacOs}` facts type. Prism
will not add a generic `Platform` trait simply to hide `cfg` attributes. Pure
policy accepts an explicit `SupportedOs`, while native mechanisms remain in the
module that owns the capability.

Browser launch policy is Linux (`xdg-open`, `gio open`, then `wslview`) or macOS
(`open`) and always uses direct argv. Prism does not invoke `cmd` or a shell to
open URLs. Shell selection and POSIX quoting are shared by direct terminal
handoff, tmux, Plan mode, and Worktrunk command hints.

## `cfg` Inventory

The inventory is organized by capability owner rather than treating every
conditional as a platform abstraction:

- `build.rs`, `platform`: support-boundary enforcement and pure OS facts.
- `process`: process groups, signals, identity, liveness, cancellation, and
  terminal foreground ownership. Domain modules consume its platform-neutral
  observations and termination outcomes.
- `durability`: Linux/macOS synchronization primitives and policy.
  `file_persistence` and `run_marker` request durability intents.
- `worker`: private runtime-directory and Unix-socket path policy.
- `flight_recorder`: recorder Unix-datagram lifecycle.
- `storage`: SQLite file identity and supported-platform filesystem behavior.
- `terminal`, `tui_signal`: terminal and signal mechanics shared by both
  supported Unix operating systems.
- `session`, `util`: supported-Unix filesystem identity and time formatting.
- Test-only `cfg` sites belong to the module under test and may select native
  smoke coverage without changing product support.

Refresh this inventory from the repository root with:

```sh
rg -n '#\[cfg\([^]]*(target_os|unix|target_family|target_arch)|cfg!\([^)]*(target_os|unix|target_family|target_arch)' src --glob '*.rs'
```

## Verification Contract

Run the Linux gate, including both explicit platform policies and the Darwin
cross-check:

```sh
scripts/full-check.sh
```

Run the focused native gate on macOS before the complete suite:

```sh
scripts/platform-smoke.sh
scripts/full-check.sh
```

The focused gate selects `platform_smoke_native_` syscall and lifecycle
contracts plus the real OpenCode/tmux integration filters. Deterministic policy,
errno classification, and fault-injection tests remain in the full suite. The
native persistence-staging test is retained because it also executes the host's
strong sync primitive. The real integrations are ignored by ordinary
`cargo test` and are enabled only by this prepared-host command.

Run the focused gate on a Mac over SSH without publishing a branch:

```sh
scripts/remote-macos-smoke.sh mac-builder prism-platform-smoke
```

The policy test group can be run on either supported host and exercises both OS
inputs:

```sh
cargo test platform_contract_
```

## Consequences

- Unsupported targets fail intentionally rather than reaching incomplete
  launch, process, durability, or socket fallbacks.
- Linux development can verify Linux and macOS policy decisions.
- Darwin cross-clippy remains useful for compilation, while native macOS smoke
  owns syscall and real integration evidence.
- The full Linux/macOS CI suite remains the merge gate after the early macOS
  smoke signal.
