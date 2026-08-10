#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

fail() {
  printf 'workflow cutover check failed: %s\n' "$1" >&2
  exit 1
}

for path in \
  src/workflow/definition \
  src/workflow/effect.rs \
  src/workflow/engine.rs \
  src/workflow/operations.rs \
  src/workflow/runtime.rs \
  src/workflow/schema.rs \
  src/workflow/trigger.rs \
  src/workflow/worker.rs \
  src/extension \
  src/package \
  crates/prism-extension-protocol \
  crates/prism-extension-sdk \
  standard-pack \
  migrations/workflow \
  migrations/historical \
  sql/workflow_ledger; do
  [[ ! -e "$path" ]] || fail "legacy path remains: $path"
done

if rg -n \
  '\b(WorkflowDefinition|StepClass|WorkflowOperations|WorkflowEffect|WorkflowCommand|LaunchWorkflow|DefinitionCatalog|ExtensionClient|prism-extension-protocol|artifact_binding|workflow_call)\b' \
  src tests assets migrations docs/contracts docs/config.md docs/keybindings.md Cargo.toml --glob '!plan-workflows.md'; then
  fail 'legacy generalized Workflow vocabulary remains in active sources'
fi

if rg -n -i 'Workflow Graph|Child Runs?|Restart Workflow|restart from|Skip Step|skippable|Approve Effect' src/tui src/view src/repository/workspace_state.rs; then
  fail 'legacy graph/effect controls remain in the TUI'
fi

[[ -f assets/templates/workflow.toml ]] || fail 'missing editable Workflow template'
[[ -f assets/workflows/stabilize.toml ]] || fail 'missing stabilize example'
[[ -f assets/workflows/multi-model-review.toml ]] || fail 'missing multi-model-review example'
[[ -f migrations/prompt-workflow/0001_prompt_workflow_kernel.sql ]] || \
  fail 'missing prompt Workflow schema'

printf 'workflow cutover check passed\n'
