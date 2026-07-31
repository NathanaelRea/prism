# Remote Hosting

Prism uses a provider-neutral change-request workflow with GitHub, GitLab, and
Forgejo adapters. Codeberg is a built-in Forgejo host profile, not a separate
protocol. Provider-specific terms such as pull request and merge request are
used only where they clarify the hosting service's UI.

## Supported Hosts And Versions

| Host or server | Transport | Compatibility statement |
| --- | --- | --- |
| GitHub.com | `gh` | Built in and covered by contract fixtures. |
| GitHub Enterprise Server | `gh` | Requires an explicit host mapping. Support is conditional on the configured `/api/v3` API and is not in the pinned local-server matrix. |
| GitLab.com | `glab` and GitLab API v4 | Built in and covered by contract fixtures plus read-only public drift probes. |
| GitLab Self-Managed | `glab` and GitLab API v4 | Requires an explicit host mapping. The scheduled local suite pins GitLab CE `18.2.0-ce.0`; other API v4 server versions are handled from observed API behavior rather than guaranteed by a version range. |
| Forgejo | HTTPS API v1 | Requires an explicit host mapping. Read operations discover server capabilities. Create and guarded merge are qualified only for Forgejo majors 9 through 16 and fail closed outside that range. The local suite pins `11.0.1`; fixtures cover majors 9 and 11. |
| Codeberg | HTTPS API v1 | Built-in Forgejo profile. Version and enabled features are discovered at runtime; the fixture set currently includes Codeberg's observed 16 development version, and a weekly credential-free repository smoke test checks the live read path. |

An unknown hostname is never guessed or probed. Map self-hosted servers under
`[remote_hosts."hostname"]`; see [Configuration](config.md#remote-hosts).

## Authentication

GitHub delegates credentials and requests to `gh`:

```sh
gh auth login --hostname github.com
```

GitLab delegates credentials and requests to `glab`:

```sh
glab auth login --hostname gitlab.com
```

Run the corresponding command with a mapped self-hosted hostname when needed.
Prism never copies either CLI's token into its configuration or database.

Forgejo and Codeberg use direct HTTPS. Configuration names the environment
variable containing the token; it never contains the token value:

```toml
[remote_hosts."codeberg.org"]
provider = "forgejo"
credential_env = "CODEBERG_TOKEN"
```

Export that variable only in the environment that launches Prism. Public
repositories may be readable without a token, but create, merge, private
repository access, and some policy endpoints require authorization.

## Capabilities And Gaps

`Supported` means the adapter implements the operation. `Conditional` means the
server, repository configuration, product tier, or token permissions determine
availability. `Unknown` and incomplete observations never authorize mutation.

| Capability | GitHub | GitLab | Forgejo / Codeberg |
| --- | --- | --- | --- |
| List and inspect change requests | Supported | Supported | Supported |
| Review threads | Supported | Supported | Conditional |
| Resolve review conversations | Supported | Supported | Unsupported |
| Check/status rollup | Supported | Supported | Supported |
| CI logs | Supported | Conditional | Conditional |
| Repository policy | Conditional | Conditional | Conditional |
| Fetch and create | Supported | Supported | Fetch supported; create version-conditional |
| Exact-head guarded merge | Supported | Conditional | Version- and policy-conditional |
| Merge queue or train | Unknown | Conditional | Unsupported |

Important provider-specific gaps:

- GitHub policy evidence can be incomplete when pagination or rulesets are not
  observed. Prism reports unknown evidence instead of assuming no policy.
- GitLab traces, approval policy, external checks, and merge trains depend on
  permissions and product tier. GitLab merge-request rebase is not exposed as a
  Prism merge method.
- Forgejo has no supported review-conversation resolution operation or merge
  queue. Prism does not simulate either behavior.
- Codeberg Actions availability is repository-specific and hosted Actions are
  intentionally limited. External status checks, including Woodpecker status,
  do not grant Prism access to CI logs. Woodpecker log retrieval is not part of
  the hosting adapter.

## Diagnostics

Run diagnostics from the affected repository:

```sh
prism --repo /path/to/repo doctor
prism --repo /path/to/repo debug info
prism --repo /path/to/repo debug logs
prism --repo /path/to/repo debug paths
```

`prism doctor` reports the resolved provider, canonical host and project,
transport, authentication availability, declared capabilities, and a Forgejo
server version when reachable. It reports missing `gh`, `glab`, or a configured
credential environment variable without printing credential values.

Use `debug info` for effective runtime/configuration facts and `debug logs` for
classified provider failures. Start Prism with `--print-logs --log-level debug`
when reproducing a problem. Use `prism debug record` for a bounded TUI flight
recording. Diagnostics redact known credential forms and do not include HTTP
response bodies, but review any artifact before sharing it because repository
and local path metadata may still be private.

Common failures:

- `remote: unavailable` for an unknown self-host means the host needs an
  explicit mapping; Prism has not probed it.
- `authentication` unavailable means the matching CLI login is missing, the
  Forgejo token variable is unset, or the token lacks access.
- `policy`, `ci_logs`, or `queue` shown as conditional/unknown is a capability
  result, not an empty successful observation. Automatic merge remains blocked.
- A Forgejo version outside majors 9 through 16 remains readable where safe,
  but create and merge are disabled.

## Compatibility Automation

`.github/workflows/remote-compatibility.yml` is separate from normal CI. It runs
weekly or by manual dispatch and can select GitLab, Forgejo, public drift probes,
or all suites.

- Local compatibility jobs start fixed GitLab CE and Forgejo image versions on
  runner-local ports, make only disposable local requests, and run the matching
  adapter fixture tests. They never mutate a public service.
- The drift job sends unauthenticated GET requests only to fixed GitHub.com,
  GitLab.com, and Codeberg endpoints. The Codeberg smoke test reads identity for
  a stable public Forgejo repository, one page containing at most one pull
  request, and, when one is available, at most one review plus at most one
  combined status for its exact head commit. Empty pull-request, review, and
  status lists are valid observations. It never sends credentials or a
  mutating HTTP method.
- The drift artifact contains only fixed host/provider identity, API version,
  schema/capability metadata, classified HTTP outcome, aggregate latency,
  bounded byte and item counts, and observation time. Response bodies, headers,
  repository or user content, dynamic identifiers, and URLs are not retained.
- A missing tool, Docker daemon, or pinned image produces an explicit `SKIP`.
  Once a local image starts, readiness or schema failure fails that compatibility
  job. Public network unavailability is recorded as `unavailable`; it also fails
  the scheduled drift job, while a manual probe may retain the classified
  observation without failing. Reachable HTTP errors or schema drift always fail
  the drift job.

Run the same scaffolding locally with:

```sh
scripts/remote-compatibility.sh forgejo
scripts/remote-compatibility.sh gitlab
scripts/remote-drift-probe.sh self-test
scripts/remote-drift-probe.sh all
```
