#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../.." && pwd)"

keep=0
skip_build=0
frames=0
check=0
sandbox_path=""
output_path=""

usage() {
  cat <<'EOF'
Usage: scripts/screenshots/run.sh --sandbox <path> --output <path> [options]

Private implementation helper for ./scripts/screenshot.sh.
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 2
}

warn() {
  printf 'warning: %s\n' "$*" >&2
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --sandbox)
      [[ $# -ge 2 ]] || die "--sandbox requires a path"
      sandbox_path="$2"
      shift 2
      ;;
    --output)
      [[ $# -ge 2 ]] || die "--output requires a path"
      output_path="$2"
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
    --frames)
      frames=1
      shift
      ;;
    --check)
      check=1
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

[[ -n "$sandbox_path" ]] || die "--sandbox is required"
[[ -n "$output_path" ]] || die "--output is required"

prism_binary="$repo_root/target/release/prism"
logs_path="$sandbox_path/logs"
setup_helper="$script_dir/setup-demo.sh"
tape_template="$script_dir/tapes/prism-demo.tape"
host_mise_config="${XDG_CONFIG_HOME:-$HOME/.config}/mise/config.toml"

cleanup() {
  local status=$?

  if command -v tmux >/dev/null 2>&1 && [[ -n "${PRISM_DEMO_ROOT:-}" ]]; then
    tmux -S "${PRISM_DEMO_TMUX_SOCKET:-$PRISM_DEMO_ROOT/tmux.sock}" kill-server >/dev/null 2>&1 || true
  fi

  if [[ "$status" -ne 0 && -n "$sandbox_path" ]]; then
    printf 'Screenshot recording failed.\n' >&2
    printf 'Sandbox: %s\n' "$sandbox_path" >&2
    printf 'Shim logs: %s\n' "$logs_path" >&2
    printf 'Debug environment: %s\n' "$sandbox_path/run.env" >&2
    printf 'Rerun with: ./scripts/screenshot.sh --keep --skip-build --output %q\n' "$output_path" >&2
    if [[ "$keep" -eq 0 && "$frames" -eq 0 ]]; then
      printf 'Sandbox will be removed because --keep was not passed.\n' >&2
    fi
  fi

  if [[ "$keep" -eq 0 && "$frames" -eq 0 && -n "$sandbox_path" && -d "$sandbox_path" ]]; then
    rm -rf "$sandbox_path"
  fi

  exit "$status"
}
trap cleanup EXIT

require_tool() {
  local tool="$1"
  local guidance="${2:-}"

  if ! command -v "$tool" >/dev/null 2>&1; then
    if [[ -n "$guidance" ]]; then
      die "required tool '$tool' was not found in PATH. $guidance"
    fi
    die "required tool '$tool' was not found in PATH"
  fi
}

require_recording_tool() {
  local tool="$1"
  local guidance="$2"

  require_tool "$tool" "$guidance"
}

optional_tool() {
  local tool="$1"

  if ! command -v "$tool" >/dev/null 2>&1; then
    warn "optional tool '$tool' was not found; later GIF optimization steps will skip it"
  fi
}

file_size_bytes() {
  if stat -c '%s' "$1" >/dev/null 2>&1; then
    stat -c '%s' "$1"
  else
    stat -f '%z' "$1"
  fi
}

escape_sed_replacement() {
  printf '%s' "$1" | sed 's/[&|]/\\&/g'
}

validate_output_gif() {
  local gif="$1"
  local max_bytes="${PRISM_DEMO_MAX_GIF_BYTES:-5242880}"
  local size

  if [[ ! -s "$gif" ]]; then
    die "recording did not produce a non-empty GIF: $gif"
  fi

  size="$(file_size_bytes "$gif")"
  if [[ "$size" -gt "$max_bytes" ]]; then
    die "generated GIF is ${size} bytes, above limit ${max_bytes}: $gif"
  fi

  printf 'Validated GIF: %s (%s bytes)\n' "$gif" "$size"
}

assert_no_unsupported_shim_calls() {
  if grep -R "UNSUPPORTED" "$logs_path" >/dev/null 2>&1; then
    printf 'Unsupported shim command recorded under %s:\n' "$logs_path" >&2
    grep -R "UNSUPPORTED" "$logs_path" >&2 || true
    exit 1
  fi
}

smoke_opencode_server() {
  local port=49381
  local pid
  "$sandbox_path/bin/opencode" serve --hostname 127.0.0.1 --port "$port" >/dev/null 2>&1 &
  pid=$!
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    if python3 - "$port" <<'PY' >/dev/null 2>&1
import sys
import urllib.request

urllib.request.urlopen(f"http://127.0.0.1:{sys.argv[1]}/global/health", timeout=0.25).read()
PY
    then
      break
    fi
    sleep 0.1
  done
  python3 - "$port" "$PRISM_DEMO_REPO" <<'PY'
import json
import sys
import urllib.request

port, repo = sys.argv[1], sys.argv[2]
base = f"http://127.0.0.1:{port}"
request = urllib.request.Request(
    f"{base}/session",
    data=json.dumps({"directory": repo}).encode(),
    headers={"Content-Type": "application/json"},
)
session = json.loads(urllib.request.urlopen(request, timeout=1).read())
session_id = session["id"]
for path in [
    "/session",
    f"/session/{session_id}",
    "/session/status",
    f"/session/{session_id}/message?limit=5",
    f"/session/{session_id}/todo",
    "/event",
]:
    urllib.request.urlopen(f"{base}{path}", timeout=1).read()
PY
  kill "$pid" >/dev/null 2>&1 || true
  wait "$pid" >/dev/null 2>&1 || true
}

printf 'Checking screenshot dependencies...\n'
require_tool cargo "Install Rust from https://rustup.rs/."
require_tool git "Install Git with your system package manager."
require_tool python3 "Install Python 3 with your system package manager."
require_recording_tool vhs "Install VHS from https://github.com/charmbracelet/vhs before refreshing the demo GIF."
require_recording_tool ttyd "VHS requires ttyd. Install it with your system package manager or from https://github.com/tsl0922/ttyd."
require_recording_tool ffmpeg "VHS requires ffmpeg. Install it with your system package manager."
optional_tool gifsicle

if [[ "$skip_build" -eq 1 ]]; then
  [[ -x "$prism_binary" ]] || die "--skip-build requested, but Prism binary is missing or not executable: $prism_binary"
  printf 'Reusing Prism binary: %s\n' "$prism_binary"
else
  printf 'Building Prism: cargo build --release\n'
  (
    cd "$repo_root"
    cargo build --release
  )
fi

mkdir -p \
  "$sandbox_path/bin" \
  "$sandbox_path/home" \
  "$sandbox_path/xdg-config" \
  "$sandbox_path/xdg-cache" \
  "$sandbox_path/xdg-state" \
  "$sandbox_path/logs" \
  "$sandbox_path/state" \
  "$sandbox_path/work" \
  "$sandbox_path/worktrees" \
  "$sandbox_path/origin.git"

if [[ "$frames" -eq 1 ]]; then
  mkdir -p "$sandbox_path/frames"
fi

touch "$sandbox_path/gitconfig"

export HOME="$sandbox_path/home"
export XDG_CONFIG_HOME="$sandbox_path/xdg-config"
export XDG_CACHE_HOME="$sandbox_path/xdg-cache"
export XDG_STATE_HOME="$sandbox_path/xdg-state"
export GIT_CONFIG_GLOBAL="$sandbox_path/gitconfig"
export GIT_CONFIG_NOSYSTEM=1
export npm_config_update_notifier=false
export PATH="$sandbox_path/bin:$PATH"
export PRISM_DEMO_ROOT="$sandbox_path"
export PRISM_DEMO_REPO="$sandbox_path/work/prism-shop"
export PRISM_DEMO_ORIGIN="$sandbox_path/origin.git"
export PRISM_DEMO_TMUX_SOCKET="$sandbox_path/tmux.sock"
export TERM=xterm-256color
export COLORTERM=truecolor
export COLUMNS=140
export LINES=46
export TZ=UTC
export LC_ALL=C.UTF-8
export LANG=C.UTF-8

unset GH_TOKEN GITHUB_TOKEN OPENCODE_API_KEY OPENAI_API_KEY ANTHROPIC_API_KEY

if [[ -f "$host_mise_config" ]]; then
  export MISE_TRUSTED_CONFIG_PATHS="${MISE_TRUSTED_CONFIG_PATHS:+$MISE_TRUSTED_CONFIG_PATHS:}$host_mise_config"
fi

case "$HOME:$XDG_CONFIG_HOME:$XDG_CACHE_HOME:$XDG_STATE_HOME:$GIT_CONFIG_GLOBAL" in
  "$sandbox_path"/*:"$sandbox_path"/*:"$sandbox_path"/*:"$sandbox_path"/*:"$sandbox_path"/*)
    ;;
  *)
    die "sandbox environment escaped PRISM_DEMO_ROOT"
    ;;
esac

printf 'Sandbox created: %s\n' "$sandbox_path"
printf 'Prism binary: %s\n' "$prism_binary"
printf 'Output GIF: %s\n' "$output_path"
printf 'Shim logs: %s\n' "$logs_path"
printf 'Fake HOME: %s\n' "$HOME"
printf 'Fake XDG_CONFIG_HOME: %s\n' "$XDG_CONFIG_HOME"

cat >"$sandbox_path/run.env" <<EOF
PATH=$PATH
HOME=$HOME
XDG_CONFIG_HOME=$XDG_CONFIG_HOME
XDG_CACHE_HOME=$XDG_CACHE_HOME
XDG_STATE_HOME=$XDG_STATE_HOME
GIT_CONFIG_GLOBAL=$GIT_CONFIG_GLOBAL
PRISM_BINARY=$prism_binary
RECORDER=$(command -v vhs)
TERM=$TERM
COLORTERM=$COLORTERM
COLUMNS=$COLUMNS
LINES=$LINES
PRISM_DEMO_TMUX_SOCKET=$PRISM_DEMO_TMUX_SOCKET
OUTPUT=$output_path
EOF
printf 'Debug environment: %s\n' "$sandbox_path/run.env"

if [[ ! -x "$setup_helper" ]]; then
  die "demo setup helper is missing or not executable: $setup_helper"
fi

printf 'Setting up fake repository and Prism config...\n'
"$setup_helper"
ln -sf "$prism_binary" "$PRISM_DEMO_REPO/prism"

printf 'Running Prism startup smoke check...\n'
"$prism_binary" --repo "$PRISM_DEMO_REPO" debug startup

printf 'Running screenshot shim smoke checks...\n'
"$sandbox_path/bin/gh" auth status >/dev/null
"$sandbox_path/bin/opencode" run --format json --dir "$PRISM_DEMO_REPO" "Smoke test demo plan" >/dev/null
smoke_opencode_server
"$sandbox_path/bin/wt" -C "$PRISM_DEMO_REPO" list --format=json >/dev/null
"$sandbox_path/bin/tmux" -V >/dev/null
printf 'README.md\nplan-demo.md\n' | "$sandbox_path/bin/fzf" >/dev/null
printf 'shim clipboard smoke\n' | "$sandbox_path/bin/wl-copy"
"$sandbox_path/bin/date" +%H:%M:%S >/dev/null

assert_no_unsupported_shim_calls

for log in gh opencode wt tmux wl-copy date fzf; do
  if [[ ! -s "$logs_path/$log.log" ]]; then
    die "expected shim log was not written: $logs_path/$log.log"
  fi
done

if [[ "$check" -eq 1 ]]; then
  printf 'Check mode complete; screenshot shims logged calls without unsupported commands.\n'
  exit 0
fi

if [[ ! -f "$tape_template" ]]; then
  die "VHS tape is missing: $tape_template"
fi

printf 'Recording Prism demo with VHS...\n'
mkdir -p "$(dirname "$output_path")"
rendered_tape="$sandbox_path/prism-demo.tape"
raw_gif="$sandbox_path/prism-demo.raw.gif"
optimized_gif="$sandbox_path/prism-demo.optimized.gif"
frames_output=""

if [[ "$frames" -eq 1 ]]; then
  frames_output="Output \"$sandbox_path/frames/\""
fi

sed \
  -e "s|__OUTPUT__|$(escape_sed_replacement "$raw_gif")|g" \
  -e "s|__FRAMES_OUTPUT__|$(escape_sed_replacement "$frames_output")|g" \
  -e "s|__REPO__|$(escape_sed_replacement "$PRISM_DEMO_REPO")|g" \
  "$tape_template" >"$rendered_tape"

if [[ "$frames" -eq 1 ]]; then
  printf 'Frame directory for this run: %s\n' "$sandbox_path/frames"
fi

(
  cd "$PRISM_DEMO_REPO"
  vhs "$rendered_tape"
)

if [[ ! -s "$raw_gif" ]]; then
  die "recording did not produce a non-empty GIF: $raw_gif"
fi

if command -v gifsicle >/dev/null 2>&1; then
  printf 'Optimizing GIF with gifsicle...\n'
  gifsicle -O3 "$raw_gif" -o "$optimized_gif"
  cp "$optimized_gif" "$output_path"
else
  warn "gifsicle was not found; writing unoptimized GIF"
  cp "$raw_gif" "$output_path"
fi

validate_output_gif "$output_path"
assert_no_unsupported_shim_calls

printf 'Recorded GIF: %s\n' "$output_path"
printf 'Sandbox: %s\n' "$sandbox_path"
printf 'Shim logs: %s\n' "$logs_path"
printf 'Intended generated artifact: %s\n' "$output_path"
printf 'Git status after generation:\n'
(
  cd "$repo_root"
  git status --short
)
