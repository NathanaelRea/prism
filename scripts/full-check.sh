#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"

# The exhaustive gate adds online SQLx metadata freshness verification to the
# routine native gate. Platform coverage comes from running this script on each
# supported host in CI rather than cross-compiling from Linux.
export CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-0}"
export PRISM_CHECK_SQLX_METADATA=1
"$repo_root/scripts/check.sh"

# Controller-tmux black-box tests are intentionally excluded from the routine
# gate and run only in the exhaustive platform gate.
cd "$repo_root"
cargo nextest run --locked --test tui_e2e --run-ignored ignored-only
