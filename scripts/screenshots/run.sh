#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../.." && pwd)"
keep=0
skip_build=0
sandbox_path=""
output_path=""

die() {
  printf 'error: %s\n' "$*" >&2
  exit 2
}

while [[ $# -gt 0 ]]; do
  case "$1" in
  --sandbox)
    sandbox_path="${2:-}"
    shift 2
    ;;
  --output)
    output_path="${2:-}"
    shift 2
    ;;
  --keep)
    keep=1
    shift
    ;;
  --skip-build)
    skip_build=1
    shift
    ;;
  *) die "unknown option: $1" ;;
  esac
done

[[ -n "$sandbox_path" ]] || die "--sandbox is required"
[[ -n "$output_path" ]] || die "--output is required"

prism_binary="$repo_root/target/release/prism"
git_binary="$(command -v git)"
cleanup() {
  local status=$?
  if command -v tmux >/dev/null 2>&1; then
    tmux -S "$sandbox_path/tmux.sock" kill-server >/dev/null 2>&1 || true
  fi
  if [[ "$status" -ne 0 ]]; then
    printf 'Screenshot capture failed. Sandbox: %s\n' "$sandbox_path" >&2
  fi
  if [[ "$keep" -eq 0 ]]; then
    rm -rf "$sandbox_path"
  fi
  exit "$status"
}
trap cleanup EXIT

for tool in cargo git python3 tmux vhs ttyd; do
  command -v "$tool" >/dev/null 2>&1 || die "required tool '$tool' was not found in PATH"
done

if [[ "$skip_build" -eq 1 ]]; then
  [[ -x "$prism_binary" ]] || die "--skip-build requested, but $prism_binary is missing"
else
  cargo build --release --manifest-path "$repo_root/Cargo.toml"
fi

mkdir -p "$sandbox_path/bin" "$sandbox_path/home" "$sandbox_path/xdg-config" \
  "$sandbox_path/xdg-cache" "$sandbox_path/xdg-state" "$sandbox_path/logs" \
  "$sandbox_path/state" "$sandbox_path/work" "$sandbox_path/worktrees" \
  "$sandbox_path/origin.git" "$sandbox_path/frames" "$(dirname "$output_path")"
touch "$sandbox_path/gitconfig"

export HOME="$sandbox_path/home"
export XDG_CONFIG_HOME="$sandbox_path/xdg-config"
export XDG_CACHE_HOME="$sandbox_path/xdg-cache"
export XDG_STATE_HOME="$sandbox_path/xdg-state"
export GIT_CONFIG_GLOBAL="$sandbox_path/gitconfig"
export GIT_CONFIG_NOSYSTEM=1
export PATH="$sandbox_path/bin:$PATH"
export PRISM_DEMO_ROOT="$sandbox_path"
export PRISM_DEMO_REPO="$sandbox_path/work/shop"
export PRISM_DEMO_ORIGIN="$sandbox_path/origin.git"
export PRISM_DEMO_GIT="$git_binary"
export TERM=xterm-256color
export COLORTERM=truecolor
export TZ=UTC
export LC_ALL=C.UTF-8
export LANG=C.UTF-8

"$script_dir/setup-demo.sh"
ln -sf "$prism_binary" "$PRISM_DEMO_REPO/prism"
"$prism_binary" --repo "$PRISM_DEMO_REPO" debug startup >/dev/null

python3 - "$prism_binary" "$PRISM_DEMO_REPO" "$sandbox_path/work/payments" <<'PY'
import sqlite3
import subprocess
import sys
from pathlib import Path

def database_for(repo):
    subprocess.check_call([sys.argv[1], "--repo", repo, "debug", "startup"], stdout=subprocess.DEVNULL)
    paths = subprocess.check_output(
        [sys.argv[1], "--repo", repo, "debug", "paths"], text=True
    )
    return Path(next(
        line.removeprefix("db_path = ")
        for line in paths.splitlines()
        if line.startswith("db_path = ")
    ))

def seed_prs(repo, rows):
    with sqlite3.connect(database_for(repo)) as connection:
        connection.executemany(
            """insert or replace into pr_cache (
                 branch, number, title, body, url, state, review_decision,
                 requested_reviewers, head_ref, base_ref, head_sha, updated_at,
                 check_status, merge_state_status, comment_count, merged, draft,
                 last_refreshed, refreshed_unix_ms
               ) values (?, ?, ?, '', ?, 'OPEN', ?, '', ?, 'main', 'demo',
                  '2026-01-15T12:00:00Z', ?, 'CLEAN', ?, 0, 0,
                  '2026-01-15T12:00:00Z', 4102444800000)""",
            rows,
        )

seed_prs(sys.argv[2], [
            ('feat/agent-session', 14, 'Improve product recommendations',
             'https://github.com/prism-demo/shop/pull/14', 'APPROVED',
             'feat/agent-session', 'pending', 0),
            ('fix/review-comments', 17, 'Tighten review prompt',
             'https://github.com/prism-demo/shop/pull/17', 'CHANGES_REQUESTED',
             'fix/review-comments', 'failed', 2),
            ('feat/shipping-rates', 19, 'Add regional shipping rates',
             'https://github.com/prism-demo/shop/pull/19', 'APPROVED',
             'feat/shipping-rates', 'success', 0),
])
seed_prs(sys.argv[3], [
            ('fix/payment-retries', 23, 'Prevent duplicate payment retries',
             'https://github.com/prism-demo/payments/pull/23', 'REVIEW_REQUIRED',
             'fix/payment-retries', 'running', 1),
])
PY

rendered_tape="$sandbox_path/prism.tape"
capture_name=".prism-capture.png"
capture_path="$PRISM_DEMO_REPO/$capture_name"
sed \
  -e "s|__OUTPUT__|$capture_name|g" \
  -e "s|__FRAMES_DIR__|$sandbox_path/frames|g" \
  "$script_dir/prism.tape" >"$rendered_tape"

(
  cd "$PRISM_DEMO_REPO"
  vhs "$rendered_tape"
)

[[ -s "$capture_path" ]] || die "capture did not produce $capture_path"
mv "$capture_path" "$output_path"
printf 'Captured screenshot: %s\n' "$output_path"
