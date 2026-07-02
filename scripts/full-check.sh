#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

if [ -d "$HOME/.cargo/bin" ]; then
  export PATH="$HOME/.cargo/bin:$PATH"
fi

run() {
  printf '\n==> %s\n' "$*"
  "$@"
}

require_command() {
  if ! command -v "$1" >/dev/null 2>&1; then
    printf 'missing required command: %s\n' "$1" >&2
    exit 1
  fi
}

require_rust_target() {
  local target="$1"
  if ! rustup target list --installed | grep -Fxq "$target"; then
    printf 'missing Rust target: %s\n' "$target" >&2
    printf 'install it with: rustup target add %s\n' "$target" >&2
    exit 1
  fi
}

export CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-0}"
export CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-always}"

run cargo fmt --check
run cargo test
run cargo clippy --all-targets -- -D warnings

case "$(uname -s)" in
  Linux)
    macos_target="${FULL_CHECK_MACOS_TARGET:-aarch64-apple-darwin}"
    cc_var="CC_${macos_target//-/_}"

    require_command clang
    require_command pkg-config
    require_rust_target "$macos_target"

    if ! pkg-config --exists sqlite3; then
      printf 'missing pkg-config metadata for sqlite3; install sqlite development files\n' >&2
      exit 1
    fi

    printf '\n==> cargo clippy --target %s --all-targets -- -D warnings\n' "$macos_target"
    env \
      PKG_CONFIG_ALLOW_CROSS=1 \
      LIBSQLITE3_SYS_USE_PKG_CONFIG=1 \
      "$cc_var=clang" \
      cargo clippy --target "$macos_target" --all-targets -- -D warnings
    ;;
  Darwin)
    ;;
  *)
    printf 'unsupported local full-check host: %s\n' "$(uname -s)" >&2
    exit 1
    ;;
esac
