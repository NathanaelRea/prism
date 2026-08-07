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

case "$(uname -s)" in
  Linux|Darwin)
    ;;
  *)
    printf 'unsupported local check host: %s\n' "$(uname -s)" >&2
    exit 1
    ;;
esac

require_command cargo-nextest
require_command cargo-sqlx
require_command sqlite3
nextest_version="0.9.143"
if [[ "$(cargo nextest --version)" != *" $nextest_version"* ]]; then
  printf 'incompatible cargo-nextest: expected %s, got %s\n' \
    "$nextest_version" "$(cargo nextest --version)" >&2
  printf 'install it with: cargo install cargo-nextest --version %s --locked\n' \
    "$nextest_version" >&2
  exit 1
fi
sqlx_cli_version="0.8.6"
if [[ "$(cargo sqlx --version)" != *" $sqlx_cli_version" ]]; then
  printf 'incompatible SQLx CLI: expected %s, got %s\n' \
    "$sqlx_cli_version" "$(cargo sqlx --version)" >&2
  printf 'install it with: cargo install sqlx-cli --version %s --locked --no-default-features --features sqlite,rustls\n' \
    "$sqlx_cli_version" >&2
  exit 1
fi

# Incremental compilation is valuable for the routine local gate. CI and the
# exhaustive gate override this to zero for reproducible cache artifacts.
export CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-1}"
export CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-always}"

run scripts/check-workflow-cutover.sh
run scripts/check-standard-pack-publication.sh

isolated_tmux_dir="$(mktemp -d "${TMPDIR:-/tmp}/prism-check-tmux.XXXXXX")"
sqlx_database="$(mktemp "${TMPDIR:-/tmp}/prism-check-sqlx.XXXXXX.db")"
workflow_database="$(mktemp "${TMPDIR:-/tmp}/prism-check-workflow.XXXXXX.db")"
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

if [ "${PRISM_CHECK_SQLX_METADATA:-0}" = 1 ]; then
  # Checked queries compile against the compatible union; migration tests
  # validate each real database independently.
  for migration in migrations/workflow/*.sql; do
    run sqlite3 "$sqlx_database" ".read $migration"
  done
  run env CARGO_TARGET_DIR="$repo_root/target/full-check-sqlx" \
    cargo sqlx prepare --check --workspace --database-url "sqlite://$sqlx_database" -- --all-targets
fi

unset DATABASE_URL
export SQLX_OFFLINE=true
run cargo nextest run --locked
# Nextest does not execute doctests.
run cargo test --locked --doc
run cargo clippy --locked --all-targets -- -D warnings
