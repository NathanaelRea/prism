#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"

keep=0
skip_build=0
output="docs/prism.png"

usage() {
  cat <<'EOF'
Usage: ./scripts/screenshot.sh [options]

Build Prism and capture the deterministic README screenshot.

Options:
  --keep           Keep the temporary sandbox for debugging.
  --skip-build     Reuse an existing target/release/prism binary.
  --output <path>  Output PNG path. Defaults to docs/prism.png.
  -h, --help       Show this help text.

Default output:
  docs/prism.png
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

sandbox_path="${TMPDIR:-/tmp}/prism"
[[ ! -e "$sandbox_path" ]] || die "temporary sandbox already exists: $sandbox_path"
logs_path="$sandbox_path/logs"
prism_binary="$repo_root/target/release/prism"
helper="$repo_root/scripts/screenshots/run.sh"

printf 'Sandbox: %s\n' "$sandbox_path"
printf 'Prism binary: %s\n' "$prism_binary"
printf 'Output PNG: %s\n' "$output_path"
printf 'Shim logs: %s\n' "$logs_path"

if [[ ! -x "$helper" ]]; then
  printf 'Phase 0 entrypoint is ready. Private helper not implemented yet: %s\n' "$helper" >&2
  exit 1
fi

mkdir -p "$(dirname "$output_path")"

args=(
  "--sandbox" "$sandbox_path"
  "--output" "$output_path"
)

[[ "$keep" -eq 1 ]] && args+=("--keep")
[[ "$skip_build" -eq 1 ]] && args+=("--skip-build")
exec "$helper" "${args[@]}"
