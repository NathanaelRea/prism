# Scaffold, Test, and Build Rust Extensions

Use the public versioned JSON Lines protocol through the Rust SDK.

1. Scaffold with `prism extension new <qualified-id>`.
2. Add deterministic protocol fixtures and unit tests before implementation.
3. Run `prism extension check <path>` frequently and `prism extension build <id> <path>` before
   publishing.
4. Run `prism extension doctor <id> <executable> --json` to verify handshake, descriptor,
   heartbeat, and bounded diagnostics.
5. Use `prism extension reload` only for future runs. Existing runs remain pinned to their exact
   executable revision.

Declare capabilities honestly. Full-trust extensions have the user's OS authority; capability
declarations are disclosure and policy, not a sandbox. Only protected effects sent through host
operations receive intent-first fencing and reconciliation guarantees.
