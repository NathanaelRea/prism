#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"

# The exhaustive gate adds online SQLx metadata freshness verification to the
# routine native gate. Platform coverage comes from running this script on each
# supported host in CI rather than cross-compiling from Linux.
export CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-0}"
export PRISM_CHECK_SQLX_METADATA=1
exec "$repo_root/scripts/check.sh"
