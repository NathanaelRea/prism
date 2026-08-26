#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

for command in cargo git ps sqlite3; do
	if ! command -v "$command" >/dev/null 2>&1; then
		printf 'missing required command: %s\n' "$command" >&2
		exit 1
	fi
done

latest_migration_version() {
	local directory="$1"
	local latest
	local files=()
	shopt -s nullglob
	files=("$directory"/*.sql)
	shopt -u nullglob
	if [ "${#files[@]}" -eq 0 ]; then
		printf 'dev install check failed: no migrations in %s\n' "$directory" >&2
		exit 1
	fi
	latest="$(printf '%s\n' "${files[@]}" | sort | tail -n 1)"
	latest="$(basename "$latest")"
	printf '%d\n' "$((10#${latest%%_*}))"
}

hash_file() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$1" | awk '{print $1}'
	else
		shasum -a 256 "$1" | awk '{print $1}'
	fi
}

bundle_count() {
	local bundles=()
	shopt -s nullglob
	bundles=("$dev_home"/bundles/.prism-dev-state.*)
	shopt -u nullglob
	printf '%d\n' "${#bundles[@]}"
}

assert_equal() {
	local actual="$1"
	local expected="$2"
	local message="$3"
	if [ "$actual" != "$expected" ]; then
		printf 'dev install check failed: %s (expected %s, got %s)\n' \
			"$message" "$expected" "$actual" >&2
		exit 1
	fi
}

repository_latest="$(latest_migration_version migrations/repository)"
workflow_latest="$(latest_migration_version migrations/prompt-workflow)"
if [ "$repository_latest" -le 1 ] || [ "$workflow_latest" -le 1 ]; then
	printf 'dev install check requires at least two migrations per database\n' >&2
	exit 1
fi
repository_previous=$((repository_latest - 1))
workflow_previous=$((workflow_latest - 1))

temporary="$(mktemp -d "${TMPDIR:-/tmp}/prism-dev-install.XXXXXX")"
temporary_alias="${temporary}.alias"
runtime_temporary="$(mktemp -d /tmp/prism-dev-runtime.XXXXXX)"
ln -s "$temporary" "$temporary_alias"
cleanup() {
	if [ -x "$temporary/bin/prism-dev" ]; then
		"$temporary/bin/prism-dev" daemon stop >/dev/null 2>&1 || true
	fi
	rm -rf "$temporary" "$runtime_temporary"
	rm -f "$temporary_alias"
}
trap cleanup EXIT
live_home="$temporary/live"
dev_home="$temporary/dev"
live_runtime="$runtime_temporary/live"
dev_runtime="$runtime_temporary/dev"
install_dir="$temporary/bin"
# Exercise the same configured-path versus physical-path split as macOS /var.
test_repo="$temporary_alias/repo"
mkdir -p "$live_home" "$live_runtime" "$dev_runtime" "$install_dir" "$test_repo"
chmod 700 "$live_runtime" "$dev_runtime"

git -C "$test_repo" init --quiet --initial-branch main
git -C "$test_repo" config user.email prism@example.invalid
git -C "$test_repo" config user.name Prism
touch "$test_repo/fixture"
git -C "$test_repo" add fixture
git -C "$test_repo" commit --quiet -m fixture

cargo build --locked --bin prism
test_binary="$repo_root/target/debug/prism"
paths="$(
	PRISM_HOME="$live_home" \
		PRISM_RUNTIME_DIR="$live_runtime" \
		"$test_binary" --repo "$test_repo" debug paths
)"
live_database="$(printf '%s\n' "$paths" | awk -F' = ' '$1 == "db_path" {print $2}')"
if [ -z "$live_database" ]; then
	printf 'dev install check failed: debug paths did not report a database\n' >&2
	exit 1
fi
printf '[[repos]]\npath = "%s"\nkey = "1"\n' "$test_repo" >"$live_home/repos.toml"

rm -f "$live_database" "$live_database-wal" "$live_database-shm"
: >"$live_database"
cargo sqlx migrate run \
	--source migrations/repository \
	--target-version "$repository_previous" \
	--database-url "sqlite://$live_database" >/dev/null
sqlite3 "$live_database" \
	"insert into metadata(key, value) values('dev_install_sentinel', 'from-live');"

live_workflow_database="$live_home/workflow.db"
: >"$live_workflow_database"
cargo sqlx migrate run \
	--source migrations/prompt-workflow \
	--target-version "$workflow_previous" \
	--database-url "sqlite://$live_workflow_database" >/dev/null
sqlite3 "$live_workflow_database" <<SQL
insert into workflow_snapshot(
  digest, workflow_name, source_path, source_revision, source, body_json, created_unix_ms
) values('dev-install-snapshot', 'test', 'test', 'test', '', '{}', 0);
insert into workflow_run(
  id, workflow_digest, workflow_name, repository, worktree, status, cycle,
  max_agent_runs, agent_runs_consumed, cancellation_requested, created_unix_ms,
  updated_unix_ms, revision, state_json
) values
  ('dev-install-queued', 'dev-install-snapshot', 'test', '$test_repo', '$test_repo',
   'queued', 1, 1, 0, 0, 0, 0, 0, '{}'),
  ('dev-install-running', 'dev-install-snapshot', 'test', '$test_repo', '$test_repo',
   'running', 1, 1, 0, 0, 0, 0, 0, '{}'),
  ('dev-install-waiting', 'dev-install-snapshot', 'test', '$test_repo', '$test_repo',
   'waiting', 1, 1, 0, 0, 0, 0, 0, '{}'),
  ('dev-install-input', 'dev-install-snapshot', 'test', '$test_repo', '$test_repo',
   'needs_input', 1, 1, 0, 0, 0, 0, 0, '{}'),
  ('dev-install-paused', 'dev-install-snapshot', 'test', '$test_repo', '$test_repo',
   'paused', 1, 1, 0, 0, 0, 0, 0, '{}'),
  ('dev-install-recovery', 'dev-install-snapshot', 'test', '$test_repo', '$test_repo',
   'recovery_required', 1, 1, 0, 0, 0, 0, 0, '{}');
SQL

live_database_hash="$(hash_file "$live_database")"
live_workflow_hash="$(hash_file "$live_workflow_database")"
printf '#!/bin/sh\nprintf "default-install-sentinel\\n"\n' >"$install_dir/prism"
chmod 755 "$install_dir/prism"
default_binary_hash="$(hash_file "$install_dir/prism")"

run_dev_install() {
	PRISM_INSTALL_DIR="$install_dir" \
		PRISM_DEFAULT_HOME="$live_home" \
		PRISM_DEFAULT_RUNTIME_DIR="$live_runtime" \
		PRISM_DEV_HOME="$dev_home" \
		PRISM_DEV_RUNTIME_DIR="$dev_runtime" \
		PRISM_TEST_INSTALL_BINARY="$test_binary" \
		"$repo_root/install.sh" --dev
}

run_dev_install_failing_after() {
	local point="$1"
	PRISM_INSTALL_DIR="$install_dir" \
		PRISM_DEFAULT_HOME="$live_home" \
		PRISM_DEFAULT_RUNTIME_DIR="$live_runtime" \
		PRISM_DEV_HOME="$dev_home" \
		PRISM_DEV_RUNTIME_DIR="$dev_runtime" \
		PRISM_TEST_INSTALL_BINARY="$test_binary" \
		PRISM_TEST_FAIL_INSTALL_AFTER="$point" \
		"$repo_root/install.sh" --dev
}

if PRISM_INSTALL_DIR="$install_dir" \
	PRISM_DEFAULT_HOME="$live_home" \
	PRISM_DEFAULT_RUNTIME_DIR="$live_runtime" \
	PRISM_DEV_HOME="$live_home" \
	PRISM_DEV_RUNTIME_DIR="$dev_runtime" \
	PRISM_TEST_INSTALL_BINARY="$test_binary" \
	"$repo_root/install.sh" --dev >"$temporary/overlap-failure.log" 2>&1; then
	printf 'dev install check failed: overlapping state roots unexpectedly installed\n' >&2
	exit 1
fi
nested_dev_home="$live_home/nested/dev"
if PRISM_INSTALL_DIR="$install_dir" \
	PRISM_DEFAULT_HOME="$live_home" \
	PRISM_DEFAULT_RUNTIME_DIR="$live_runtime" \
	PRISM_DEV_HOME="$nested_dev_home" \
	PRISM_DEV_RUNTIME_DIR="$dev_runtime" \
	PRISM_TEST_INSTALL_BINARY="$test_binary" \
	"$repo_root/install.sh" --dev >"$temporary/nested-overlap-failure.log" 2>&1; then
	printf 'dev install check failed: nested state root unexpectedly installed\n' >&2
	exit 1
fi
if [ -e "$live_home/nested" ]; then
	printf 'dev install check failed: rejected nested state root changed default state\n' >&2
	exit 1
fi
if PRISM_INSTALL_DIR="$install_dir" \
	PRISM_DEFAULT_HOME="$live_home" \
	PRISM_DEFAULT_RUNTIME_DIR="$live_runtime" \
	PRISM_DEV_HOME="$dev_home" \
	PRISM_DEV_RUNTIME_DIR="$live_runtime" \
	PRISM_TEST_INSTALL_BINARY="$test_binary" \
	"$repo_root/install.sh" --dev >"$temporary/runtime-overlap-failure.log" 2>&1; then
	printf 'dev install check failed: default runtime overlap unexpectedly installed\n' >&2
	exit 1
fi

run_dev_install
assert_equal "$(hash_file "$live_database")" "$live_database_hash" \
	"repository source database changed"
assert_equal "$(hash_file "$live_workflow_database")" "$live_workflow_hash" \
	"Workflow source database changed"
assert_equal "$(hash_file "$install_dir/prism")" "$default_binary_hash" \
	"default prism binary changed"
assert_equal "$("$install_dir/prism")" "default-install-sentinel" \
	"default prism command changed"

output="$(
	XDG_CONFIG_HOME="$temporary/unrelated-xdg" \
		"$install_dir/prism-dev" --repo "$test_repo" debug paths
)"
dev_database="$(printf '%s\n' "$output" | awk -F' = ' '$1 == "db_path" {print $2}')"
dev_user_config="$(printf '%s\n' "$output" | awk -F' = ' '$1 == "user_config" {print $2}')"
dev_state="$(dirname "$dev_user_config")"
case "$dev_state/" in
"$dev_home/bundles/"*) ;;
*)
	printf 'dev install check failed: development state escaped bundle container: %s\n' \
		"$dev_state" >&2
	exit 1
	;;
esac
case "$dev_database/" in
"$dev_state/repos/"*) ;;
*)
	printf 'dev install check failed: development database escaped state bundle: %s\n' \
		"$dev_database" >&2
	exit 1
	;;
esac
printf '%s\n' "$output" | grep -F "worker_runtime_dir = $dev_runtime" >/dev/null
override_output="$(
	PRISM_DEV_HOME="$live_home" \
		PRISM_DEV_RUNTIME_DIR="$live_runtime" \
		"$install_dir/prism-dev" --repo "$test_repo" debug paths
)"
assert_equal "$(printf '%s\n' "$override_output" | awk -F' = ' '$1 == "db_path" {print $2}')" \
	"$dev_database" "runtime environment redirected prism-dev state"
printf '%s\n' "$override_output" | grep -F "worker_runtime_dir = $dev_runtime" >/dev/null
assert_equal "$(sqlite3 "$live_database" 'select max(version) from _sqlx_migrations')" "$repository_previous" \
	"default repository migration version"
assert_equal "$(sqlite3 "$dev_database" 'select max(version) from _sqlx_migrations')" "$repository_latest" \
	"development repository migration version"
assert_equal "$(sqlite3 "$dev_database" "select value from metadata where key='dev_install_sentinel'")" \
	"from-live" "repository sentinel was not copied"
assert_equal "$(sqlite3 "$live_workflow_database" 'select max(version) from _sqlx_migrations')" "$workflow_previous" \
	"default Workflow migration version"
assert_equal "$(sqlite3 "$dev_state/workflow.db" 'select max(version) from _sqlx_migrations')" "$workflow_latest" \
	"development Workflow migration version"
assert_equal "$(sqlite3 "$live_workflow_database" "select count(*) from workflow_run where status not in ('succeeded','failed','cancelled')")" 6 \
	"default nonterminal Workflows were changed"
assert_equal "$(sqlite3 "$dev_state/workflow.db" "select count(*) from workflow_run where status not in ('succeeded','failed','cancelled')")" 0 \
	"nonterminal Workflows were copied into actionable development state"
"$install_dir/prism-dev" worker serve >"$temporary/leased-worker.log" 2>&1 &
leased_worker=$!
reader_path="$install_dir/.prism-dev-readers/$leased_worker"
for _ in 1 2 3 4 5 6 7 8 9 10; do
	if [ -d "$reader_path" ]; then
		break
	fi
	sleep 0.1
done
if [ ! -d "$reader_path" ] || ! kill -0 "$leased_worker" 2>/dev/null; then
	printf 'dev install check failed: wrapper did not transfer its reader lease to Prism\n' >&2
	exit 1
fi
leased_command="$(ps -o comm= -p "$leased_worker" | tr -d '[:space:]')"
case "$leased_command" in
*prism*) ;;
*)
	printf 'dev install check failed: wrapper did not exec Prism (pid %s is %s)\n' \
		"$leased_worker" "$leased_command" >&2
	exit 1
	;;
esac

first_wrapper_hash="$(hash_file "$install_dir/prism-dev")"
first_dev_database_hash="$(hash_file "$dev_database")"
if run_dev_install >"$temporary/active-reader-failure.log" 2>&1; then
	printf 'dev install check failed: active prism-dev reader did not block replacement\n' >&2
	exit 1
fi
assert_equal "$(hash_file "$install_dir/prism-dev")" "$first_wrapper_hash" \
	"blocked install replaced prism-dev wrapper"
assert_equal "$(hash_file "$dev_database")" "$first_dev_database_hash" \
	"blocked install replaced development state"
if ! kill -0 "$leased_worker" 2>/dev/null; then
	printf 'dev install check failed: blocked install stopped the leased Prism process\n' >&2
	exit 1
fi
kill -9 "$leased_worker"
wait "$leased_worker" 2>/dev/null || true
if [ ! -d "$reader_path" ]; then
	printf 'dev install check failed: SIGKILL did not leave a stale reader lease fixture\n' >&2
	exit 1
fi
"$install_dir/prism-dev" daemon start >/dev/null
"$install_dir/prism-dev" daemon status | grep -Fi running >/dev/null

sqlite3 "$live_database" \
	"update metadata set value='refreshed-live' where key='dev_install_sentinel';"
sqlite3 "$dev_database" \
	"insert into metadata(key, value) values('dev_only', 'discard-me');"
run_dev_install
output="$("$install_dir/prism-dev" --repo "$test_repo" debug paths)"
dev_database="$(printf '%s\n' "$output" | awk -F' = ' '$1 == "db_path" {print $2}')"
dev_user_config="$(printf '%s\n' "$output" | awk -F' = ' '$1 == "user_config" {print $2}')"
dev_state="$(dirname "$dev_user_config")"
"$install_dir/prism-dev" daemon status | grep -Fi stopped >/dev/null
assert_equal "$(sqlite3 "$dev_database" "select value from metadata where key='dev_install_sentinel'")" \
	"refreshed-live" "development reinstall did not refresh from default state"
assert_equal "$(sqlite3 "$dev_database" "select count(*) from metadata where key='dev_only'")" 0 \
	"development reinstall retained discarded state"

printf '\n# rollback sentinel retained only by a successful restoration\n' >>"$install_dir/prism-dev"
wrapper_hash="$(hash_file "$install_dir/prism-dev")"
selector_path="$install_dir/.prism-dev-current"
selector_hash="$(hash_file "$selector_path")"
dev_database_hash="$(hash_file "$dev_database")"
current_bundle_count="$(bundle_count)"
for failure_point in bundle wrapper; do
	if run_dev_install_failing_after "$failure_point" \
		>"$temporary/expected-$failure_point-promotion-failure.log" 2>&1; then
		printf 'dev install check failed: %s promotion failure unexpectedly installed\n' \
			"$failure_point" >&2
		exit 1
	fi
	assert_equal "$(hash_file "$install_dir/prism-dev")" "$wrapper_hash" \
		"$failure_point promotion failure replaced prism-dev wrapper"
	assert_equal "$(hash_file "$selector_path")" "$selector_hash" \
		"$failure_point promotion failure replaced prism-dev selector"
	assert_equal "$(hash_file "$dev_database")" "$dev_database_hash" \
		"$failure_point promotion failure replaced development state"
	assert_equal "$(bundle_count)" "$current_bundle_count" \
		"$failure_point promotion failure retained a staged bundle"
done
rollback_output="$("$install_dir/prism-dev" --repo "$test_repo" debug paths)"
assert_equal "$(printf '%s\n' "$rollback_output" | awk -F' = ' '$1 == "db_path" {print $2}')" \
	"$dev_database" "promotion failure changed selected state"
dev_database_hash="$(hash_file "$dev_database")"

printf 'not a sqlite database\n' >"$live_database"
if run_dev_install >"$temporary/expected-failure.log" 2>&1; then
	printf 'dev install check failed: corrupt source unexpectedly installed\n' >&2
	exit 1
fi
assert_equal "$(hash_file "$install_dir/prism-dev")" "$wrapper_hash" \
	"failed install replaced prism-dev wrapper"
assert_equal "$(hash_file "$dev_database")" "$dev_database_hash" \
	"failed install replaced development state"

printf 'development install check: isolated snapshot, migrations, worker cutover, locking, refresh, and rollback passed\n'
