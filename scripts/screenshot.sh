#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"

keep=0
skip_build=0
frames=0
check=0
output="docs/prism-demo.gif"

usage() {
  cat <<'EOF'
Usage: ./scripts/screenshot.sh [options]

Build Prism, run the sandboxed terminal demo, and generate the README GIF.

Options:
  --keep           Keep the sandbox under target/screenshots/ for debugging.
  --skip-build     Reuse an existing target/release/prism binary.
  --frames         Keep intermediate frames under the run directory.
  --check          Validate setup without writing docs/prism-demo.gif.
  --output <path>  Output GIF path. Defaults to docs/prism-demo.gif.
  -h, --help       Show this help text.

Default output:
  docs/prism-demo.gif
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  printf 'Run ./scripts/screenshot.sh --help for usage.\n' >&2
  exit 2
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --keep)
      keep=1
      shift
      ;;
    --skip-build)
      skip_build=1
      shift
      ;;
    --frames)
      frames=1
      shift
      ;;
    --check)
      check=1
      shift
      ;;
    --output)
      [[ $# -ge 2 ]] || die "--output requires a path"
      output="$2"
      shift 2
      ;;
    --output=*)
      output="${1#--output=}"
      [[ -n "$output" ]] || die "--output requires a path"
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown option: $1"
      ;;
  esac
done

if [[ "$output" = /* ]]; then
  output_path="$output"
else
  output_path="$repo_root/$output"
fi

run_id="run-$(date -u +%Y%m%dT%H%M%SZ)-$$"
sandbox_path="$repo_root/target/screenshots/$run_id"
logs_path="$sandbox_path/logs"
prism_binary="$repo_root/target/release/prism"
helper="$repo_root/scripts/screenshots/run.sh"

printf 'Sandbox: %s\n' "$sandbox_path"
printf 'Prism binary: %s\n' "$prism_binary"
printf 'Output GIF: %s\n' "$output_path"
printf 'Shim logs: %s\n' "$logs_path"

if [[ ! -x "$helper" ]]; then
  printf 'Phase 0 entrypoint is ready. Private helper not implemented yet: %s\n' "$helper" >&2
  exit 1
fi

if [[ "$check" -eq 0 ]]; then
  mkdir -p "$(dirname "$output_path")"
fi

args=(
  "--sandbox" "$sandbox_path"
  "--output" "$output_path"
)

[[ "$keep" -eq 1 ]] && args+=("--keep")
[[ "$skip_build" -eq 1 ]] && args+=("--skip-build")
[[ "$frames" -eq 1 ]] && args+=("--frames")
[[ "$check" -eq 1 ]] && args+=("--check")

exec "$helper" "${args[@]}"
