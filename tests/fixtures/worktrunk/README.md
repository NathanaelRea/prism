# Worktrunk JSON Fixtures

These redacted compatibility fixtures exercise Prism's typed projection of
`wt list --format=json` output. Paths, URLs, branch names, variables, and remote
facts are synthetic; no user state or credentials are retained.

- `schema1-*.json` covers the bare-array schema available at the supported
  Worktrunk floor, 0.58.0, and retained by tested-current Worktrunk 0.71.0.
- `schema2-*.json` covers the schema-2 envelope documented by Worktrunk 0.71.0.
- Full fixtures include development URL/listening state, typed variables, and
  rendered custom columns. Minimal fixtures cover missing paths and absent or
  null optional observations.

The fixtures intentionally contain future/irrelevant fields to verify that
Prism exposes only its canonical environment facts. They are parser fixtures,
not byte-for-byte snapshots of every field emitted by either release. Unknown
schema discriminators are covered separately and must fail closed.
