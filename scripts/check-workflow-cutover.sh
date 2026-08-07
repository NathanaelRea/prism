#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

# Historical plans/prose and SQL migrations whose sole purpose is DROP are
# outside this production-source scan.
roots=(src sql schemas)
patterns=(
  'crate::auto_flow|crate::plan_run|workflow::execution'
  '\bAutoRun\b|\bAutoStep\b|\bAutoFlow\b|\bPlanRun\b|\bPlanMode\b|\bWorkflowKind\b'
  'auto_run|auto_step_run|plan_run|plan_step_run|workflow_execution'
  '\[auto\]|run-plan|prism auto|prism plan'
)

failed=0
for pattern in "${patterns[@]}"; do
  matches="$(rg -n \
    --glob '!migrations/**' \
    --glob '!sql/database/workflow_cutover_drop_assert.sql' \
    --glob '!sql/database/workflow_cutover_drop_preflight.sql' \
    --glob '!sql/database/workflow_cutover_drop_processes.sql' \
    --glob '!sql/database/workflow_cutover_drop_seed_mutation.sql' \
    --glob '!sql/database/workflow_cutover_drop_seed_process.sql' \
    --glob '!sql/database/workflow_cutover_drop_seed_process_pid.sql' \
    --glob '!sql/database/workflow_cutover_drop_seed_protected.sql' \
    "$pattern" "${roots[@]}" || true)"
  if [[ -n "$matches" ]]; then
    printf 'forbidden generalized-workflow cutover symbol: %s\n%s\n' "$pattern" "$matches" >&2
    failed=1
  fi
done

if (( failed )); then
  printf 'workflow cutover static proof failed\n' >&2
  exit 1
fi

printf 'workflow cutover static proof passed\n'
