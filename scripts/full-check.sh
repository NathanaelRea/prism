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

require_command cargo-sqlx
require_command sqlite3
sqlx_cli_version="0.8.6"
if [[ "$(cargo sqlx --version)" != *" $sqlx_cli_version" ]]; then
  printf 'incompatible SQLx CLI: expected %s, got %s\n' \
    "$sqlx_cli_version" "$(cargo sqlx --version)" >&2
  printf 'install it with: cargo install sqlx-cli --version %s --locked --no-default-features --features sqlite,rustls\n' \
    "$sqlx_cli_version" >&2
  exit 1
fi

export CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-0}"
export CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-always}"

run scripts/check-workflow-cutover.sh
run scripts/check-standard-pack-publication.sh

isolated_tmux_dir="$(mktemp -d "${TMPDIR:-/tmp}/prism-full-check-tmux.XXXXXX")"
sqlx_database="$(mktemp "${TMPDIR:-/tmp}/prism-full-check-sqlx.XXXXXX.db")"
workflow_database="$(mktemp "${TMPDIR:-/tmp}/prism-full-check-workflow.XXXXXX.db")"
trap 'rm -rf "$isolated_tmux_dir"; rm -f "$sqlx_database" "$sqlx_database-wal" "$sqlx_database-shm" "$workflow_database" "$workflow_database-wal" "$workflow_database-shm"' EXIT
export TMUX_TMPDIR="$isolated_tmux_dir"

run cargo fmt --check
run cargo sqlx migrate run --source migrations/repository --database-url "sqlite://$sqlx_database"
run cargo sqlx migrate run --source migrations/workflow --database-url "sqlite://$workflow_database"
repository_legacy_count="$(sqlite3 "$sqlx_database" "select count(*) from sqlite_master where name in ('auto_run','auto_step_run','plan_run','plan_step_run','workflow_execution')")"
workflow_import_count="$(sqlite3 "$workflow_database" "select count(*) from sqlite_master where name = 'import_journal'")"
if [ "$repository_legacy_count" -ne 0 ] || [ "$workflow_import_count" -ne 0 ]; then
  printf 'fresh databases retain legacy workflow schema objects\n' >&2
  exit 1
fi
# Checked queries compile against the compatible union; migration tests validate each real target.
for migration in migrations/workflow/*.sql; do
  run sqlite3 "$sqlx_database" ".read $migration"
done
run cargo sqlx prepare --check --workspace --database-url "sqlite://$sqlx_database" -- --all-targets
if [ "$(uname -s)" = Linux ]; then
  macos_targets="${FULL_CHECK_MACOS_TARGETS:-aarch64-apple-darwin}"
  for macos_target in $macos_targets; do
    require_rust_target "$macos_target"
    cc_var="CC_${macos_target//-/_}"
    env \
      DOCS_RS=1 \
      PKG_CONFIG_ALLOW_CROSS=1 \
      LIBSQLITE3_SYS_USE_PKG_CONFIG=1 \
      "$cc_var=clang" \
      cargo sqlx prepare --check --workspace --database-url "sqlite://$sqlx_database" \
        -- --all-targets --target "$macos_target"
  done
fi
unset DATABASE_URL
export SQLX_OFFLINE=true
run cargo test --locked
run cargo clippy --locked --all-targets -- -D warnings
run cargo test --locked platform_contract_

case "$(uname -s)" in
  Linux)
    require_command clang
    require_command pkg-config

    if ! pkg-config --exists sqlite3; then
      printf 'missing pkg-config metadata for sqlite3; install sqlite development files\n' >&2
      exit 1
    fi

    macos_targets="${FULL_CHECK_MACOS_TARGETS:-aarch64-apple-darwin}"
    for macos_target in $macos_targets; do
      require_rust_target "$macos_target"
      cc_var="CC_${macos_target//-/_}"
      printf '\n==> cargo clippy --target %s --all-targets -- -D warnings\n' "$macos_target"
      env \
        DOCS_RS=1 \
        PKG_CONFIG_ALLOW_CROSS=1 \
        LIBSQLITE3_SYS_USE_PKG_CONFIG=1 \
        "$cc_var=clang" \
        cargo clippy --locked --target "$macos_target" --all-targets -- -D warnings
    done
    ;;
  Darwin)
    ;;
  *)
    printf 'unsupported local full-check host: %s\n' "$(uname -s)" >&2
    exit 1
    ;;
esac
