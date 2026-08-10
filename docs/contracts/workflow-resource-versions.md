# Workflow And Extension Contract Versions

This document fixes the public contract versions targeted by the generalized
workflow cutover. The schemas and runtime support are implemented in later
phases; changing the versions recorded here requires an explicit contract
revision and updated golden fixtures.

| Contract | Initial version | Compatibility rule |
| --- | --- | --- |
| Workflow Definition TOML | `schema_version = 2` | Reject unknown versions. Source migration is explicit, previewable, backed up, and never imports Plan/Auto state. |
| Package manifest TOML | `schema_version = 1` | Reject unknown versions before resolving or activating resources. |
| Scope lockfile TOML | `schema_version = 1` | Reject unknown versions. Every dependency uses an exact source revision and digest; no semver or floating resolution is allowed in a retained lock. |
| Extension JSON Lines protocol | `protocol_major = 1`, `protocol_minor = 1` | Reject a different major. A peer may use a minor-version feature only when both peers negotiated its named feature; unknown optional features and fields are ignored within configured bounds. Unknown required messages fail the affected call. |
| CLI stable JSON envelope | `schema_version = 1` | Every stable `--json` response is an object with `schema_version`, `kind`, and `data`. Consumers must ignore unknown fields, and Prism must not change the meaning or type of an existing field within version 1. Breaking changes require a new schema version. |

## Canonical Identity And Revisions

Workflow and package resource IDs are qualified (`owner.package/resource`). Scope
is metadata and never participates in identity or implicit shadowing. A revision
is SHA-256 over canonical source plus resolved dependency and build metadata. A
Definition Snapshot records the exact Workflow Definition, package closure,
Artifact schemas, prompts/templates, and extension executable digests resolved
once at launch.

Canonicalization is contract-specific and will be frozen with golden byte
fixtures when each parser is implemented. Until then, implementations must not
mint a revision under these versions from an ad hoc serialization.

## Extension Framing

Each protocol frame is one UTF-8 JSON object followed by `\n`. The first exchange
is `hello` / `hello_ack`; `describe` follows successful negotiation. Every
execution and host-operation request has a unique correlation ID. Frame, field,
output, Artifact, render, concurrency, and timeout limits are checked before
unbounded allocation. Stdout is protocol-only; stderr is bounded diagnostic
output.

## Stable JSON Kinds

Version 1 reserves kinds for workflow, extension, package, skill, and template
list/show operations; workflow validation, preview, run, and history; and
extension/package diagnostics. Each command defines its own typed `data` value
without changing the common envelope. Error output uses the same version and a
kind ending in `.error` when `--json` was accepted.
