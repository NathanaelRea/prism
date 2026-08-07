# Generalized Workflow Acceptance Fixtures

These credential-free fixtures are the executable acceptance targets established
in phase 0. Later implementation phases must load them through public catalog,
package, extension-host, run, trigger, provider, and effect interfaces; tests may
add transport setup but must not weaken an expected event.

Each JSON file has `fixture_schema_version: 1`, a stable contract name, inputs,
and ordered or partially ordered expected facts. `contract-index.toml` maps every
accepted product decision to a named fixture or future contract test. A status of
`target` means the fixture intentionally precedes its runner.

`legacy-deletion-targets.txt` is an inventory for removal, not a migration or
compatibility fixture. No acceptance test may assert conversion of Plan Mode or
Auto Flow history.
