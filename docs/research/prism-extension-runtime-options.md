# Extension Runtime Options for Prism

Research performed 2026-08-06 against primary documentation and source from
Rust, Zed, Zellij, Lapce, Nushell, dprint, SWC, Wasmtime, `abi_stable`, Deno,
Boa, QuickJS, and Pi.

## Recommendation

Rust extensions are viable. The hard part is not Rust code; it is choosing a
stable loading and compatibility contract.

Prism should make its external extension seam a **versioned process protocol**,
not a Rust trait ABI and not a language-specific embedded runtime. Ship:

1. a language-neutral extension manifest and protocol;
2. a first-class Rust SDK that builds an ordinary executable;
3. content-addressed package snapshots so a Workflow Run keeps the exact
   extension revision it launched with; and
4. optional TypeScript and WebAssembly Component adapters later if rapid script
   authoring, portable single-file packages, or confinement become priorities.

Following this research, the product decision is Rust-first authoring. Installed
extensions can use prebuilt executables without requiring Cargo; authors and
agents that edit source use the Rust SDK and `prism extension build`. Rust is an
implementation language, not an in-process dynamic-library ABI. Normal Rust types
remain behind the serialized process protocol, leaving room for other language
adapters later.

This design follows the strongest property of Nushell's plugin model: plugins
are executables, the protocol is explicit and versioned, and the Rust SDK is a
convenience rather than the compatibility boundary. Nushell supports JSON and
MessagePack over stdio or local sockets, starts with a version handshake, and
requires semver-compatible protocol versions. [Nushell plugins][nu-plugins]
[Nushell protocol][nu-protocol]

It also avoids the main weakness of native Rust dynamic libraries: the Rust ABI
has no stability guarantee. A direct Rust trait, `String`, `Vec`, future, or
error type cannot safely become Prism's durable third-party interface merely by
placing it in a `.so` or `.dylib`. [Rust ABI][rust-abi] [Rust linkage][rust-linkage]

The process is not a security sandbox under the selected Pi-style trust model.
Extensions have the user's authority. The process seam still earns its keep by
providing crash containment, cancellation, bounded I/O, protocol negotiation,
portable package identities, and support for both TypeScript and Rust.

## The Important Distinction

“Rust extension” can mean four materially different things:

1. **Native Rust dynamic library** — Prism loads a platform `.so`, `.dylib`, or
   `.dll` into the worker process.
2. **Stable-ABI dynamic library** — the same native loading, but all exchanged
   types use an FFI-safe compatibility framework such as `abi_stable`.
3. **Rust compiled to WebAssembly** — Prism embeds a Wasm runtime and calls a WIT
   or serialized interface.
4. **Rust executable** — Prism starts a process and speaks a versioned protocol.

Only the first option is inherently tied to Rust's unstable native ABI. The
other three can support Rust safely, with different authoring and runtime costs.

## Option Comparison

| Option | Authoring | Compatibility | Portability | Reload/update | Fault boundary | Host integration | Prism cost |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Native Rust `dylib` | Excellent Rust ergonomics before the seam | Poor unless host and plugin are rebuilt together | One artifact per OS/architecture | Difficult; unloading is unsafe/platform-sensitive | None; panic, abort, UB, or segfault can kill worker | Direct and fast | High long-term compatibility burden |
| `cdylib` + C ABI | Rust implementation, C-shaped interface | Stable if every type/ownership rule is manually preserved | One artifact per OS/architecture | Versioned function tables possible | None | Direct but FFI-heavy | Medium/high |
| `abi_stable` | Better Rust ergonomics than raw C | Runtime layout checks and controlled semver evolution | One artifact per OS/architecture | No unloading support | None | Direct | Medium; specialized interface discipline |
| Rust → Wasm Component | Rust SDK is pleasant after WIT exists | Explicit versioned WIT interface | One Wasm artifact across supported hosts | Good; immutable modules load side-by-side | Strong runtime isolation | Only imported WASI/host functions | High initial runtime and SDK work |
| Rust executable protocol | Normal Rust application and SDK | Explicit handshake and serialized schema | Source portable; binaries per platform | Excellent; revisions are separate executables | Process crash does not corrupt worker memory | RPC/host calls or full OS access | Medium, with a very deep seam |
| Node/TypeScript subprocess | Fastest edit/reload and npm ecosystem | Explicit protocol; JS code insulated from Rust internals | Requires Node-compatible runtime | Excellent | Process crash contained | Full Node/system access | Low/medium if Node is available |
| Embedded V8/Deno core | TypeScript/JS can be first-class | Host-defined JS interface | Runtime ships with Prism | Good | In-process engine failure can affect worker | Every OS/Node API must be supplied | Very high build and host-runtime cost |
| Embedded QuickJS | Small JS engine and low startup | Host-defined JS interface | Runtime ships with Prism | Good | In process | No Node/system APIs unless Prism builds them | Medium/high |
| Embedded Boa | Pure Rust engine | Host-defined JS interface | Runtime ships with Prism | Good | In process | Runtime APIs must be built | High and currently experimental |

## Native Rust Dynamic Libraries

### Why they can work

Rust can emit both `dylib` and `cdylib` outputs. A `dylib` is a dynamic Rust
library intended as a Rust dependency; a `cdylib` is a dynamic system library
intended to be loaded from another language. Linux, macOS, and Windows each use
their platform library format. [Rust linkage][rust-linkage]

`libloading` provides a cross-platform wrapper around platform loaders and ties
loaded symbols to the lifetime of their library handle. Its example still
requires an `unsafe extern "C" fn` type because the host must assert that the
symbol actually has the expected ABI. It intentionally does not hide all
platform behavior differences. [libloading][libloading]

For a tightly controlled deployment where Prism and every plugin are built from
one workspace with one toolchain, native dynamic Rust can be practical. That is
not the proposed user ecosystem: packages can be updated independently, copied,
modified, obtained online, and retained by old Workflow Runs.

### Why a direct Rust trait is not a stable extension interface

The Rust Reference says the native `extern "Rust"` ABI “offers no stability
guarantees.” A native trait object includes compiler-defined representation and
vtable details. Standard Rust data types also do not promise a C-compatible
layout unless their interface explicitly uses an appropriate representation.
[Rust ABI][rust-abi] [Rust Nomicon FFI][rust-ffi]

A safe native interface therefore has to avoid ordinary Rust interface types and
instead define, at minimum:

- a stable calling convention;
- explicit representations for strings, byte arrays, options, results, enums,
  callbacks, and opaque handles;
- allocation and deallocation ownership;
- panic/unwind behavior;
- thread-safety and async completion rules;
- interface and feature-version negotiation; and
- rules for keeping library code loaded while any value or callback exists.

The Rust Nomicon notes that foreign declarations are unsafe because the compiler
cannot verify the declaration, and that unwinding across the wrong ABI can abort
the process or become undefined behavior depending on direction. A plugin panic
must be caught before a non-unwinding FFI boundary if the worker is expected to
survive. [Rust Nomicon FFI][rust-ffi]

At that point the interface is effectively a manually encoded protocol inside
one address space. A process protocol gives Prism nearly the same serialization
work while also containing plugin crashes and allowing TypeScript.

### `abi_stable` improves this, but does not erase the trade-offs

`abi_stable` is specifically designed for Rust-to-Rust FFI and runtime-loaded
libraries built by different Rust versions. It supplies FFI-safe standard-type
wrappers, generated trait-object-like vtables, load-time type-layout checks,
prefix types for extensible interfaces, and non-exhaustive enum support.
[abi_stable][abi-stable]

It demonstrates that native Rust plugins can be engineered responsibly. Its own
interface rules are substantial:

- exchanged types must implement `StableAbi`;
- ordinary structures cannot gain fields compatibly unless represented as
  prefix types;
- exhaustive enums cannot gain variants;
- field names participate in compatibility;
- wrapped external types need care because layout and global-state assumptions
  may change; and
- the crate does not support unloading libraries.

[abi_stable evolution][abi-stable-evolution]

Its README also states that each `0.y.0` and `x.0.0` line defines an incompatible
`abi_stable` ABI. Prism would be adopting both its own compatibility contract and
the framework's major-version contract. [abi_stable README][abi-stable-readme]

This is a defensible choice for a Rust-only, in-process, performance-critical
plugin system. Workflow Steps usually spend far more time in agents, Git,
provider APIs, CI waits, and commands than in extension dispatch, so Prism does
not receive enough leverage from in-process calls to justify making FFI its
primary ecosystem seam.

## Rust Compiled to WebAssembly

Rust-to-Wasm is the most common native-code answer in established Rust desktop
applications examined here.

### Zed

Zed extension repositories contain an `extension.toml`; procedural extensions
are written in Rust, depend on `zed_extension_api`, and compile as `cdylib`
WebAssembly. Zed warns that normal Rust facilities such as `std::env::var` do not
behave as native code would and directs authors to host-provided extension
functions. [Zed extension development][zed-development]

Zed's current source uses Wasmtime's Component Model host, records a semantic API
version in a custom Wasm section, maintains versioned WIT directories, checks
supported API ranges, and uses epoch interruption so extensions yield. Its WIT
world exposes explicit host functions for process execution, HTTP, downloads,
settings, worktree reads, platform identity, and Node package management.
[Zed WIT][zed-wit] [Zed Wasm host][zed-host]

Zed also has a capability system that can restrict process commands, download
hosts/paths, and npm package installation. Installed extensions retain source in
an `installed` directory and use a separate work directory. [Zed capabilities][zed-capabilities]
[Zed installation][zed-installation]

This is a mature demonstration of the benefit and cost: portable Rust extension
code and a stable explicit host interface, backed by a substantial Wasmtime/WASI
host implementation.

### Zellij

Zellij explicitly describes its plugin system as WebAssembly/WASI, says its own
UI is built from plugins, and presents plugins as portable artifacts. Rust is the
only officially supported authoring language today despite Wasm's theoretical
multi-language support. [Zellij plugins][zellij-plugins]

The Rust `zellij-tile` SDK exports a small lifecycle (`load`, `update`, `render`),
uses generated entrypoints, and serializes events and commands with Protobuf.
Zellij supplies explicit permissions for reading/changing application state,
opening files, running commands, opening terminals/plugins, and writing stdin.
It also provides a build-and-reload development loop targeting
`wasm32-wasi`. [Zellij lifecycle][zellij-lifecycle] [Zellij permissions][zellij-permissions]
[Zellij development][zellij-development] [Zellij SDK source][zellij-sdk]

### Lapce and SWC

Lapce's source loads “Volt” plugin Wasm modules through Wasmtime/WASI, wires
stdin/stdout/stderr, inherits selected environment, preopens the workspace, and
uses a plugin-server RPC layer. This confirms the same pattern in another Rust
editor, though the examined dependency versions and implementation differ from
Zed's Component Model host. [Lapce plugin host][lapce-host]

SWC compiles Rust transformation plugins to Wasm. Its compatibility guide is an
important warning: portability of machine code does not automatically create
schema compatibility. Older SWC plugins used layout-sensitive serialized ASTs
and commonly needed updates with each host release. SWC moved to self-describing
CBOR plus unknown enum variants to improve compatibility, while documenting that
field deletion or type changes can still break it. [SWC plugin development][swc-development]
[SWC compatibility][swc-compatibility]

### Applicability to Prism

Wasmtime describes Wasm as a portable compilation target, WASI as portable
interfaces for OS-like capabilities, and the Component Model as typed
cross-language composition. The Component Model guide identifies WASI 0.2 as a
stable set of WIT definitions that components can pin. [Wasmtime][wasmtime]
[Component Model][component-model]

Wasm would give Prism:

- one extension artifact across Linux/macOS CPU combinations supported by the
  runtime;
- an explicit WIT interface;
- strong memory and control-flow isolation;
- deterministic host capability injection; and
- natural coexistence of pinned extension revisions.

It would also require:

- embedding and maintaining Wasmtime/WASI;
- designing host calls for every useful Prism operation;
- requiring Rust authors to install a Wasm target and use only compatible crates;
- solving TypeScript-to-Component packaging separately; and
- deciding how Pi-style full system access maps into WASI and host imports.

The current Prism dependency graph does not include Wasmtime. A local Cargo
metadata probe with current crate defaults resolved roughly 177 transitive
packages for a minimal Wasmtime probe, versus 44 for `abi_stable`; these counts
are a dependency-shape indicator, not a binary-size or performance benchmark.
Prism itself currently resolves roughly 299 packages. Wasm is feasible, but not
free.

Because the chosen trust model is full-power and the selected initial runtime is
a normal Rust executable, Wasm's strongest differentiator—confinement—is not
currently the highest-value trade. Keep the protocol suitable for a later Wasm
adapter rather than making Wasm the first required runtime.

## Out-of-Process Executable Plugins

### Nushell's model

Nushell plugins are executable files. Nushell starts them on demand and
communicates over stdin/stdout or optional local sockets. Plugins select JSON or
MessagePack, exchange `Hello` messages with protocol/version/features, receive
calls with unique IDs, return responses, and receive interrupt/reset signals.
The protocol specifies that unknown features are ignored and version acceptance
is semver-based. [Nushell protocol][nu-protocol]

The official plugin book presents Rust and Python examples. Its officially
maintained plugins are separate executables, and third-party Rust plugins are
commonly installed with `cargo install`. It explicitly warns that plugin and
Nushell protocol versions must be compatible. [Nushell plugins][nu-plugins]

This is directly applicable to Prism:

- `describe` registers Step Implementations, schemas, renderers, Triggers, and
  notification channels;
- `execute` receives one exact Step Attempt envelope;
- extension-to-host requests invoke Prism operations;
- progress/output messages remain bounded;
- cancellation and heartbeat are protocol messages;
- terminal results include typed outputs and effect/reconciliation facts; and
- the initial handshake negotiates protocol and optional features.

A process can still use full OS permissions, exactly as requested. Prism should
not describe this as confinement. The process boundary instead prevents a Rust
panic, JS exception, or native extension segfault from corrupting the worker's
memory. The worker can record the failed Attempt, terminate the extension
process tree, and apply normal retry/recovery policy.

### dprint's hybrid model

dprint, itself written in Rust, supports both sandboxed Wasm plugins and
unsandboxed process plugins. Its documentation recommends Wasm when a language
can produce it and retains process plugins for ecosystems that cannot. Both use
the same user setup shape; downloaded process plugins require a checksum.
[dprint plugins][dprint-plugins] [dprint development][dprint-development]

That is a useful future path for Prism: define one logical extension interface,
start with process adapters, and permit a Wasm transport later without changing
Workflow Definition references.

## TypeScript Runtime Choices

### Why Pi's model is easy for Pi but not automatically for Prism

Pi is already a Node/TypeScript application. Its extensions are TypeScript
modules loaded through `jiti`, can import Node built-ins and npm dependencies,
and run with full system permissions. Pi packages bundle extensions and skills
and install through npm, Git, or local paths. [Pi extensions][pi-extensions]
[Pi packages][pi-packages]

Prism is a Rust binary. “Support TypeScript like Pi” therefore requires one of:

- require an external Node runtime and start a host process;
- distribute a managed Node/Deno runtime;
- embed a JavaScript engine and implement the missing host environment; or
- compile extensions ahead of execution into another format.

### Node subprocess

A Node subprocess plus an SDK is the shortest route to genuine Pi-like behavior:
TypeScript transpilation, ESM/CommonJS handling, npm dependencies, `node:fs`,
`node:child_process`, network access, and familiar debugging. It naturally uses
the same process protocol recommended for Rust.

The costs are an external or managed Node runtime, package installation and lock
policy, startup overhead, and snapshotting the complete dependency closure—not
just one `.ts` file—when a Workflow Run pins an implementation revision.

If TypeScript support is added later, this is the recommended first adapter.
Prism should make the runtime requirement explicit in package metadata and
`doctor`, rather than pretending arbitrary TypeScript can run inside the Rust
binary for free.

### Embedded V8 through `deno_core`

`deno_core` provides a V8 `JsRuntime`, module loading, Rust “ops,” promises, and
an event loop. Its `extension!` macro primarily defines Rust-compiled Deno
extensions containing ops and included ESM; it is a toolkit for building a JS
runtime, not a ready-made Node-compatible user-extension host. [deno_core][deno-core]
[deno_core extension macro][deno-extension]

The underlying `rusty_v8` project says V8 is very large and takes a long time to
compile; normal builds download a prebuilt static library, while source builds
require Chromium's GN/Ninja toolchain and are described as nontrivial. [rusty_v8][rusty-v8]

A local Cargo metadata probe resolved roughly 192 transitive packages for a
minimal current `deno_core` dependency. More importantly, Prism would still need
to design module resolution, TypeScript transpilation, filesystem/process/fetch
APIs, npm installation, permissions, cancellation, and lifecycle behavior.
Embedding V8 is justified when the JS runtime itself is a core product. It is
not the minimum path to workflow extensions.

### Embedded QuickJS or Boa

QuickJS is much smaller. `rquickjs` advertises ES2020 modules, async Rust
integration, custom module loaders, and low startup time. It also explicitly
states that it does not provide system or web APIs, and that it compiles a C
library with platform-binding limitations. [rquickjs][rquickjs]

Boa is a pure-Rust JavaScript engine, but its project describes it as
experimental and reports partial current ECMAScript conformance. [Boa][boa]

Either engine could support a deliberately small Prism scripting language. To
support the requested Pi-like ecosystem, Prism would still have to implement or
emulate the Node facilities users and npm packages expect, as well as TypeScript
transpilation. That is a larger and shallower interface than a Node subprocess
adapter.

## Package and Snapshot Implications

A package may contain Workflow Definitions, extensions, skills, templates, and
schemas. The package installer should resolve a source into an immutable package
revision before activation:

- canonical package identity and source;
- exact Git ref, npm version, local digest, or URL digest;
- manifest and protocol version;
- lockfile and resolved dependency digest;
- extension entrypoints and runtime kind;
- registered Step Implementation descriptors;
- declared full-trust status and capabilities;
- platform/runtime requirements; and
- all files required to execute that revision.

Editing installed source creates a new local package revision. A new Workflow
Run snapshots references to that revision. Existing runs retain the prior
content-addressed package. Removing editable source disables it for future
resolution but does not erase a revision retained by run history.

For TypeScript packages, snapshotting only source while reinstalling floating npm
dependencies later would violate the agreed immutable-run model. Prism needs a
lockfile/reproducible install policy or an archived resolved package tree. For
Rust executable packages, Prism needs either a retained exact binary digest or a
reproducible source/toolchain build record; executing the retained binary is the
stronger run-time authority.

## Full-Trust Consequence

The selected Pi-style model conflicts with one promise in the existing workflow
requirements: a full-trust extension can call `git`, `gh`, `curl`, or Worktrunk
directly and bypass Prism's Effect Broker. No in-process or subprocess interface
can both grant unrestricted user authority and technically require brokered
mutations.

Prism can still:

- make the Standard Pack use brokered host operations;
- provide high-level host methods that persist effect intent and reconcile;
- declare and preview expected capabilities;
- record whether an implementation is brokered or arbitrary full-trust code;
- require stronger confirmation before unattended or externally triggered runs;
- warn that direct extension effects are not crash-reconciled by Prism; and
- reject false claims that a full-trust implementation is confined.

But capability declarations are disclosure and policy for a full-trust
extension, not an OS security boundary. This must be made explicit in the new
requirements and plan.

## Proposed Prism Extension Interface

The external seam should remain small:

```text
extension handshake
  -> protocol version, package revision, supported features

describe
  -> Step Implementations, schemas, Triggers, notifications, render metadata

execute(AttemptEnvelope)
  -> progress/output messages
  -> host-operation requests
  -> typed terminal result

cancel(attempt_id, generation)
  -> acknowledgement; Prism still owns process-tree termination deadline
```

The implementation behind that seam owns transport, subprocess supervision,
TypeScript/Rust SDK conversion, bounded output, package resolution, digesting,
version negotiation, and host-operation routing.

Do not expose Run Ledger, SQLite, worker internals, or Rust domain structs to an
extension. The Attempt envelope should use stable, versioned wire types and
opaque identities. This keeps the module deep: callers and extension authors
learn one execution contract while Prism retains locality for scheduling,
persistence, and migration.

## Suggested Delivery

1. Specify protocol fixtures and a fake extension before finalizing the SDK.
2. Implement process supervision and the JSON protocol handshake.
3. Implement the Rust SDK and a tiny example executable.
4. Port the Standard Pack through that same public protocol.
5. Add package install/update/remove and immutable revision storage.
6. Add Rust-focused agent skills and examples.
7. Consider MessagePack/local sockets only if measured payload pressure warrants
   them.
8. Consider a Node/TypeScript adapter only when its runtime and dependency
   snapshot policy are justified.
9. Consider a Wasm Component adapter only after the WIT interface can be derived
   from the stable process protocol and there is a concrete portability or
   confinement need.

## Conclusion

Rust extensions do work. Prism should reject only **unversioned in-process Rust
trait plugins**, not Rust as an extension language.

The best fit for the agreed product is:

- Rust-first authoring and a first-class Rust SDK;
- prebuilt executables for users who do not customize source;
- one language-neutral executable protocol;
- Pi-style full trust disclosed honestly;
- standard implementations using brokered Prism host operations; and
- immutable package revisions pinned by Workflow Runs.

TypeScript and Wasm can be added later without changing Workflow Definition
identity or the extension compatibility boundary.

This preserves the editability and agent-generated customization of Pi without
making Prism's Rust compiler internals or a JavaScript engine its public
extension interface.

## Sources

[rust-abi]: https://doc.rust-lang.org/reference/items/external-blocks.html#abi
[rust-linkage]: https://doc.rust-lang.org/reference/linkage.html
[rust-ffi]: https://doc.rust-lang.org/nomicon/ffi.html
[libloading]: https://docs.rs/libloading/latest/libloading/
[abi-stable]: https://docs.rs/abi_stable/latest/abi_stable/
[abi-stable-evolution]: https://docs.rs/abi_stable/latest/abi_stable/docs/library_evolution/
[abi-stable-readme]: https://github.com/rodrimati1992/abi_stable_crates/blob/master/readme.md
[zed-development]: https://zed.dev/docs/extensions/developing-extensions
[zed-capabilities]: https://zed.dev/docs/extensions/capabilities
[zed-installation]: https://zed.dev/docs/extensions/installing-extensions
[zed-wit]: https://github.com/zed-industries/zed/tree/main/crates/extension_api/wit
[zed-host]: https://github.com/zed-industries/zed/blob/main/crates/extension_host/src/wasm_host.rs
[zellij-plugins]: https://zellij.dev/documentation/plugins
[zellij-lifecycle]: https://zellij.dev/documentation/plugin-lifecycle
[zellij-permissions]: https://zellij.dev/documentation/plugin-api-permissions
[zellij-development]: https://zellij.dev/documentation/plugin-dev-env
[zellij-sdk]: https://github.com/zellij-org/zellij/blob/main/zellij-tile/src/lib.rs
[lapce-host]: https://github.com/lapce/lapce/blob/master/lapce-proxy/src/plugin/wasi.rs
[swc-development]: https://swc.rs/docs/plugin/ecmascript/getting-started
[swc-compatibility]: https://swc.rs/docs/plugin/ecmascript/compatibility
[wasmtime]: https://docs.wasmtime.dev/introduction.html
[component-model]: https://component-model.bytecodealliance.org/
[nu-plugins]: https://www.nushell.sh/book/plugins.html
[nu-protocol]: https://www.nushell.sh/contributor-book/plugin_protocol_reference.html
[dprint-plugins]: https://dprint.dev/plugins/
[dprint-development]: https://dprint.dev/plugin-dev/
[pi-extensions]: https://github.com/earendil-works/pi-mono/blob/main/packages/coding-agent/docs/extensions.md
[pi-packages]: https://github.com/earendil-works/pi-mono/blob/main/packages/coding-agent/docs/packages.md
[deno-core]: https://docs.rs/deno_core/latest/deno_core/struct.JsRuntime.html
[deno-extension]: https://docs.rs/deno_core/latest/deno_core/macro.extension.html
[rusty-v8]: https://github.com/denoland/rusty_v8/blob/main/README.md
[rquickjs]: https://github.com/DelSkayn/rquickjs/blob/master/README.md
[boa]: https://github.com/boa-dev/boa/blob/main/README.md
