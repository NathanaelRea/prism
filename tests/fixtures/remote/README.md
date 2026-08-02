# Remote Adapter Fixtures

These fixtures are scrubbed provider contract responses. They contain no live
credentials, authorization headers, private repository URLs, or user-generated
secrets. Adapter tests consume them without network access.

Coverage metadata:

| Directory | Recorded shapes |
| --- | --- |
| `github/` | Truncated summary and policy evidence. |
| `gitlab/` | Same-project and fork merge requests, exact-head and mismatched pipelines, and mixed pipeline provenance. |
| `forgejo/` | Forgejo 9 and 11 versions, a Codeberg 16 development version/profile, forks, stale reviews, statuses/Actions, disabled Actions, and complete/incomplete branch protection. |

When refreshing a fixture, retain only the fields needed by the contract test,
replace account/project/commit identifiers with deterministic examples, and
check that no token, cookie, email address, or private URL remains. Version
fixtures intentionally retain the server version because compatibility behavior
depends on the Forgejo major.

Live compatibility and drift checks are deliberately separate from these files;
see `docs/remote-hosting.md#compatibility-automation`. The Codeberg smoke test
keeps live response bodies only in a temporary directory, accepts empty bounded
list responses, emits counts rather than repository or user content, and removes
the temporary data on exit. Run its deterministic schema checks with
`scripts/remote-drift-probe.sh self-test`; no live response is promoted to a
fixture.
