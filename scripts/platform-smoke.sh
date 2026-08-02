#!/usr/bin/env bash
set -euo pipefail

script_dir="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(dirname -- "$script_dir")"
cd "$repo_root"

case "$(uname -s)" in
  Linux|Darwin) ;;
  *)
    printf 'unsupported platform smoke host: %s (expected Linux or macOS)\n' "$(uname -s)" >&2
    exit 1
    ;;
esac

for command in cargo git opencode tmux; do
  if ! command -v "$command" >/dev/null 2>&1; then
    printf 'missing platform smoke dependency: %s\n' "$command" >&2
    exit 1
  fi
done

export PRISM_TEST_OPENCODE="${PRISM_TEST_OPENCODE:-$(command -v opencode)}"
export PRISM_TEST_TMUX="${PRISM_TEST_TMUX:-$(command -v tmux)}"

runtime_dir="$(mktemp -d /tmp/ps.XXXXXX)"
tmux_dir="$(mktemp -d /tmp/pt.XXXXXX)"
trap 'rm -rf "$runtime_dir" "$tmux_dir"' EXIT
export PRISM_RUNTIME_DIR="$runtime_dir"
export TMUX_TMPDIR="$tmux_dir"

cargo test platform_smoke_native_ -- --include-ignored --nocapture
cargo test real_opencode_server_round_trips_prism_session_api -- --ignored --nocapture
cargo test real_prism_opencode_tmux_stack_ensures_reusable_agent_session -- --ignored --nocapture
