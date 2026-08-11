# ADR 0002: Capability-Based Harness Adapters

## Status

Accepted

## Decision

Prism places harness-specific command construction and prompt transport behind a compiled-in harness module. Workflows request interactive or headless invocations and receive explicit unsupported errors; they do not infer behavior from executable names.

OpenCode keeps its structured events, server-backed sessions, observation, submission, and native cancellation. Generic harnesses provide tmux process liveness and optional bounded plain-text headless execution. Prism does not scrape terminal rendering to emulate missing capabilities.

Commands are argument arrays. Generic startup prompts use only explicitly configured argument, stdin (headless only), or temporary-file transport. Prism retains local process ownership for cancellation and output limits.

Workflow Attempts persist the exact implementation, executable revision, and harness inputs
selected when each run is created. Resume and retry resolve those pinned bytes, preventing a
global default change from reinterpreting historical execution identity.

## Consequences

Harnesses degrade explicitly rather than pretending to have OpenCode-like session semantics. New
interactive harness adapters remain compiled into Prism and require deterministic contract
coverage before being advertised. Workflow Step Implementations use the public, versioned
extension process protocol; capability declarations are disclosure and policy inputs, not a
sandbox boundary.
