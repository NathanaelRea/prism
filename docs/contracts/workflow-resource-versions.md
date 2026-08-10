# Workflow And Trigger Contract Versions

This document fixes the public contract versions for trigger-driven prompt Workflows.
Breaking wire or stable-JSON changes require an explicit version increase.

| Contract | Current version | Compatibility rule |
| --- | --- | --- |
| Prompt Workflow TOML | unversioned | The filename stem is identity. Unknown fields and unsupported Agent selections are errors. Active runs retain the exact source and compiled snapshot. |
| External Trigger phase protocol | `protocol_version = 1` | Each invocation accepts exactly one request on stdin and returns exactly one response on stdout. Unknown versions, phases, and response shapes fail the phase. |
| CLI stable JSON envelope | `schema_version = 1` | Every stable `--json` response is an object with `schema_version`, `kind`, and `data`. Consumers ignore unknown fields; breaking meaning or type changes require a new version. |

## Workflow Revisions

A Workflow revision is SHA-256 over its source plus resolved Steps, dependencies,
context selections, Agent harness/model/variant selections, and Trigger revisions.
The compiled run snapshot retains that exact content. Repository and user source
paths are provenance only and do not change an active run after launch.

External Trigger executable bytes are retained by content digest. Editing a
Workflow or Trigger affects future runs only. Repository resources require trust
for the exact current resource revision before discovery or execution.

## Trigger Framing

A Trigger is a full-trust shebang executable. Prism starts one bounded process
for each `should_run_step`, `pre_step_run`, or `post_step_run` phase, writes one
UTF-8 JSON request followed by `\n`, closes stdin, and reads one bounded JSON
response. Stdout is protocol-only and stderr is bounded diagnostic output.

The optional source comment
`# prism-trigger: check-only, repeatable-prepare, repeatable-finalize` declares
properties Prism must know before execution. `check-only` permits a promptless
Step; the repeatability directives permit restart reconciliation for the named
hook. Undeclared custom hooks are treated as uncertain after interruption. The
small TypeScript wrapper and executable example live under `trigger-sdk/typescript`;
using them is optional and does not make Node a Prism runtime dependency.

## Stable JSON Kinds

Version 1 includes Workflow list/show/validation/run/history/control,
copy-example, reset, and repository-trust results. Error output uses the same
envelope with a kind ending in `.error` whenever `--json` was accepted.
