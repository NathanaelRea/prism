#!/usr/bin/env bash
set -euo pipefail

canonical_directory_path() {
	local path="$1"
	local parent base resolved_parent
	if [ -d "$path" ]; then
		(cd "$path" && pwd -P)
		return
	fi
	parent="$(dirname "$path")"
	base="$(basename "$path")"
	if [ "$parent" = "$path" ]; then
		printf '%s\n' "$path"
		return
	fi
	resolved_parent="$(canonical_directory_path "$parent")"
	case "$base" in
	.) printf '%s\n' "$resolved_parent" ;;
	..) dirname "$resolved_parent" ;;
	*) printf '%s/%s\n' "${resolved_parent%/}" "$base" ;;
	esac
}

paths_overlap() {
	local first="${1%/}"
	local second="${2%/}"
	if [ -z "$first" ]; then
		first=/
	fi
	if [ -z "$second" ]; then
		second=/
	fi
	if [ "$first" = / ] || [ "$second" = / ]; then
		return 0
	fi
	case "$first/" in
	"$second/"*) return 0 ;;
	esac
	case "$second/" in
	"$first/"*) return 0 ;;
	esac
	return 1
}

usage() {
	cat <<'EOF'
Usage: ./install.sh [--dev]

  ./install.sh        Install prism from main or a clean release checkout.
  ./install.sh --dev  Snapshot the default state and install isolated prism-dev.
EOF
}

mode=default
case "$#" in
0) ;;
1)
	case "$1" in
	--dev) mode=dev ;;
	-h | --help)
		usage
		exit 0
		;;
	*)
		usage >&2
		exit 2
		;;
	esac
	;;
*)
	usage >&2
	exit 2
	;;
esac

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
install_dir="${PRISM_INSTALL_DIR:-$HOME/.local/bin}"

if [ "$mode" = default ] && git -C "$script_dir" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
	head="$(git -C "$script_dir" rev-parse HEAD)"
	safe_ref=0
	for ref in refs/heads/main refs/remotes/origin/main; do
		if git -C "$script_dir" show-ref --verify --quiet "$ref" &&
			[ "$head" = "$(git -C "$script_dir" rev-parse "$ref")" ]; then
			safe_ref=1
			break
		fi
	done
	if [ "$safe_ref" -ne 1 ]; then
		release_tag=
		while IFS= read -r tag; do
			if [[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
				release_tag="$tag"
				break
			fi
		done < <(git -C "$script_dir" tag --points-at HEAD)
		if [ -n "$release_tag" ]; then
			for ref in refs/heads/main refs/remotes/origin/main; do
				if git -C "$script_dir" show-ref --verify --quiet "$ref" &&
					git -C "$script_dir" merge-base --is-ancestor "$head" "$ref"; then
					safe_ref=1
					break
				fi
			done
		fi
	fi
	if [ "$safe_ref" -ne 1 ]; then
		branch="$(git -C "$script_dir" symbolic-ref --quiet --short HEAD || printf 'detached HEAD')"
		printf 'refusing to install prism from %s; use ./install.sh --dev\n' "$branch" >&2
		exit 1
	fi
	if [ -n "$(git -C "$script_dir" status --porcelain --untracked-files=normal)" ]; then
		printf 'refusing to install prism from a dirty checkout; use ./install.sh --dev\n' >&2
		exit 1
	fi
fi

if [ -n "${PRISM_TEST_INSTALL_BINARY:-}" ]; then
	source_binary="$PRISM_TEST_INSTALL_BINARY"
else
	cargo build \
		--manifest-path "$script_dir/Cargo.toml" \
		--release \
		--locked \
		--package prism
	source_binary="$script_dir/target/release/prism"
fi
if [ ! -x "$source_binary" ]; then
	printf 'install source binary is not executable: %s\n' "$source_binary" >&2
	exit 1
fi

mkdir -p "$install_dir"

if [ "$mode" = default ]; then
	target_path="$install_dir/prism"
	temporary_path="$(mktemp "$install_dir/.prism.XXXXXX")"
	trap 'rm -f "$temporary_path"' EXIT
	install -m 755 "$source_binary" "$temporary_path"
	mv -f "$temporary_path" "$target_path"
	trap - EXIT
	printf 'Installed %s\n' "$target_path"
	printf 'Make sure %s is on your PATH.\n' "$install_dir"
	exit 0
fi

umask 077
default_config_parent="${XDG_CONFIG_HOME:-$HOME/.config}"
default_home="${PRISM_DEFAULT_HOME:-$default_config_parent/prism}"
dev_home="${PRISM_DEV_HOME:-$default_config_parent/prism-dev}"
dev_runtime="${PRISM_DEV_RUNTIME_DIR:-${XDG_RUNTIME_DIR:-/tmp}/prism-dev-$(id -u)}"
case "$(uname -s)" in
Linux)
	default_runtime="${PRISM_DEFAULT_RUNTIME_DIR:-${XDG_RUNTIME_DIR:-$default_home}/prism}"
	if [ -z "${XDG_RUNTIME_DIR:-}" ] && [ -z "${PRISM_DEFAULT_RUNTIME_DIR:-}" ]; then
		default_runtime="$default_home/runtime"
	fi
	;;
Darwin)
	default_runtime="${PRISM_DEFAULT_RUNTIME_DIR:-$HOME/Library/Application Support/Prism/runtime}"
	;;
*)
	printf 'unsupported install host: %s\n' "$(uname -s)" >&2
	exit 1
	;;
esac

target_path="$install_dir/prism-dev"
selector_path="$install_dir/.prism-dev-current"
install_lock="$install_dir/.prism-dev-install.lock"
reader_root="$install_dir/.prism-dev-readers"
state_parent="$(dirname "$dev_home")"
bundle_parent="$dev_home/bundles"

default_home_resolved="$(canonical_directory_path "$default_home")"
dev_home_resolved="$(canonical_directory_path "$dev_home")"
default_runtime_resolved="$(canonical_directory_path "$default_runtime")"
dev_runtime_resolved="$(canonical_directory_path "$dev_runtime")"
if paths_overlap "$default_home_resolved" "$dev_home_resolved"; then
	printf 'development state must not overlap default state: %s\n' "$dev_home" >&2
	exit 1
fi
if paths_overlap "$dev_home_resolved" "$default_runtime_resolved"; then
	printf 'development state must not overlap default runtime: %s\n' "$dev_home" >&2
	exit 1
fi
if paths_overlap "$dev_runtime_resolved" "$default_home_resolved"; then
	printf 'development runtime must not overlap default state: %s\n' "$dev_runtime" >&2
	exit 1
fi
if paths_overlap "$dev_runtime_resolved" "$default_runtime_resolved"; then
	printf 'development runtime must not overlap default runtime: %s\n' "$dev_runtime" >&2
	exit 1
fi
if paths_overlap "$dev_runtime_resolved" "$dev_home_resolved"; then
	printf 'development runtime must not overlap development state: %s\n' "$dev_runtime" >&2
	exit 1
fi
case "$dev_home$dev_runtime" in
*$'\n'* | *$'\r'*)
	printf 'development state and runtime paths must not contain newlines\n' >&2
	exit 1
	;;
esac

mkdir -p "$state_parent" "$reader_root"
chmod 700 "$reader_root"

state_stage=
wrapper_stage=
selector_stage=
wrapper_backup=
previous_state=
final_bundle=
old_bundle=
lock_acquired=0
bundle_promoted=0
container_created=0
container_displaced=0
wrapper_promoted=0
had_wrapper=0
committed=0

cleanup() {
	status=$?
	set +e
	if [ "$committed" -eq 0 ]; then
		if [ "$wrapper_promoted" -eq 1 ]; then
			if [ "$had_wrapper" -eq 1 ] && [ -n "$wrapper_backup" ] && [ -e "$wrapper_backup" ]; then
				mv -f "$wrapper_backup" "$target_path"
				wrapper_backup=
			else
				rm -f "$target_path"
			fi
		fi
		if [ "$bundle_promoted" -eq 1 ] && [ -n "$final_bundle" ]; then
			rm -rf "$final_bundle"
		fi
		if [ "$container_created" -eq 1 ]; then
			rm -rf "$dev_home"
		fi
		if [ "$container_displaced" -eq 1 ] && [ -n "$previous_state" ] && [ -e "$previous_state" ]; then
			rm -rf "$dev_home"
			mv "$previous_state" "$dev_home"
			previous_state=
		fi
	fi
	if [ -n "$state_stage" ]; then
		rm -rf "$state_stage"
	fi
	if [ -n "$wrapper_stage" ]; then
		rm -f "$wrapper_stage"
	fi
	if [ -n "$selector_stage" ]; then
		rm -f "$selector_stage"
	fi
	if [ -n "$wrapper_backup" ]; then
		rm -f "$wrapper_backup"
	fi
	if [ "$lock_acquired" -eq 1 ]; then
		rm -rf "$install_lock"
	fi
	exit "$status"
}
trap cleanup EXIT

acquire_install_lock() {
	local attempt owner
	for attempt in 1 2; do
		if mkdir "$install_lock" 2>/dev/null; then
			lock_acquired=1
			printf '%s\n' "$$" >"$install_lock/pid"
			return 0
		fi
		owner="$(cat "$install_lock/pid" 2>/dev/null || true)"
		if [ "$attempt" -eq 1 ] && [[ "$owner" =~ ^[0-9]+$ ]] && ! kill -0 "$owner" 2>/dev/null; then
			rm -rf "$install_lock"
			continue
		fi
		printf 'another prism-dev installation is in progress (%s)\n' "$install_lock" >&2
		return 1
	done
	return 1
}

check_active_readers() {
	local reader pid active
	for _ in 1 2 3 4 5; do
		active=
		shopt -s nullglob
		for reader in "$reader_root"/*; do
			[ -d "$reader" ] || continue
			pid="$(basename "$reader")"
			if [[ "$pid" =~ ^[0-9]+$ ]] && kill -0 "$pid" 2>/dev/null; then
				active="${active:+$active, }$pid"
			else
				rm -rf "$reader"
			fi
		done
		shopt -u nullglob
		if [ -z "$active" ]; then
			return 0
		fi
		sleep 0.1
	done
	printf 'refusing to replace prism-dev while it is running (pid %s)\n' "$active" >&2
	return 1
}

selected_bundle=
selected_runtime=
fail_install_for_test() {
	local point="$1"
	if [ -n "${PRISM_TEST_INSTALL_BINARY:-}" ] &&
		[ "${PRISM_TEST_FAIL_INSTALL_AFTER:-}" = "$point" ]; then
		printf 'injected prism-dev install failure after %s promotion\n' "$point" >&2
		exit 1
	fi
}

read_selector() {
	local extra
	selected_bundle=
	selected_runtime=
	if [ ! -f "$selector_path" ] || [ -L "$selector_path" ]; then
		return 1
	fi
	{
		IFS= read -r selected_bundle || return 1
		IFS= read -r selected_runtime || return 1
		if IFS= read -r extra && [ -n "$extra" ]; then
			return 1
		fi
	} <"$selector_path"
	[ -n "$selected_bundle" ] && [ -n "$selected_runtime" ]
}

state_stage="$(mktemp -d "$state_parent/.prism-dev-state.XXXXXX")"
wrapper_stage="$(mktemp "$install_dir/.prism-dev-wrapper.XXXXXX")"
selector_stage="$(mktemp "$install_dir/.prism-dev-selector.XXXXXX")"
final_bundle="$bundle_parent/$(basename "$state_stage")"

"$source_binary" __prepare-dev-state "$default_home" "$state_stage"
install -m 755 "$source_binary" "$state_stage/.prism-bin"
printf 'managed by install.sh --dev\n' >"$state_stage/.prism-dev-bundle"
chmod 600 "$state_stage/.prism-dev-bundle"

{
	printf '%s\n' '#!/usr/bin/env bash' 'set -euo pipefail' 'umask 077'
	printf 'selector=%q\n' "$selector_path"
	printf 'install_lock=%q\n' "$install_lock"
	printf 'reader_root=%q\n' "$reader_root"
	cat <<'EOF'
if [ -d "$install_lock" ]; then
	printf 'prism-dev is being installed; try again\n' >&2
	exit 75
fi
mkdir -p "$reader_root"
chmod 700 "$reader_root"
reader="$reader_root/$$"
rmdir "$reader" 2>/dev/null || true
mkdir "$reader"
if [ -d "$install_lock" ]; then
	rmdir "$reader"
	printf 'prism-dev is being installed; try again\n' >&2
	exit 75
fi
bundle=
runtime=
{
	IFS= read -r bundle || exit 1
	IFS= read -r runtime || exit 1
} <"$selector"
if [ -z "$bundle" ] || [ -z "$runtime" ] || [ ! -x "$bundle/.prism-bin" ]; then
	rmdir "$reader"
	printf 'prism-dev installation selector is invalid: %s\n' "$selector" >&2
	exit 1
fi
export PRISM_HOME="$bundle"
export PRISM_RUNTIME_DIR="$runtime"
export PRISM_DEV=1
export PRISM_DEV_READER_PATH="$reader"
exec "$bundle/.prism-bin" "$@"
EOF
} >"$wrapper_stage"
chmod 755 "$wrapper_stage"
printf '%s\n%s\n' "$final_bundle" "$dev_runtime" >"$selector_stage"
chmod 600 "$selector_stage"

if [ -x "$target_path" ] && [ ! -e "$selector_path" ] && [ ! -L "$selector_path" ]; then
	PRISM_DEV_HOME= PRISM_DEV_RUNTIME_DIR= "$target_path" worker shutdown >/dev/null
fi

acquire_install_lock
check_active_readers

if [ -e "$selector_path" ] || [ -L "$selector_path" ]; then
	if ! read_selector; then
		printf 'existing prism-dev selector is invalid: %s\n' "$selector_path" >&2
		exit 1
	fi
	old_bundle="$selected_bundle"
	old_runtime="$selected_runtime"
	if [ ! -f "$old_bundle/.prism-dev-bundle" ] || [ ! -x "$old_bundle/.prism-bin" ]; then
		printf 'existing prism-dev bundle is invalid: %s\n' "$old_bundle" >&2
		exit 1
	fi
	old_bundle_resolved="$(canonical_directory_path "$old_bundle")"
	old_runtime_resolved="$(canonical_directory_path "$old_runtime")"
	if paths_overlap "$old_bundle_resolved" "$default_home_resolved" ||
		paths_overlap "$old_runtime_resolved" "$default_home_resolved" ||
		paths_overlap "$old_runtime_resolved" "$default_runtime_resolved"; then
		printf 'refusing to contact an existing prism-dev installation that overlaps default Prism\n' >&2
		exit 1
	fi
	PRISM_HOME="$old_bundle" \
		PRISM_RUNTIME_DIR="$old_runtime" \
		PRISM_DEV=1 \
		"$old_bundle/.prism-bin" worker shutdown >/dev/null
fi

if [ -e "$dev_home" ] || [ -L "$dev_home" ]; then
	if [ -d "$dev_home" ] && [ ! -L "$dev_home" ] && [ -f "$dev_home/.prism-dev-container" ]; then
		:
	else
		previous_state="$(mktemp -d "$state_parent/.prism-dev-previous.XXXXXX")"
		rmdir "$previous_state"
		mv "$dev_home" "$previous_state"
		container_displaced=1
	fi
fi
if [ ! -d "$dev_home" ]; then
	mkdir -p "$bundle_parent"
	container_created=1
	printf 'managed by install.sh --dev\n' >"$dev_home/.prism-dev-container"
	chmod 600 "$dev_home/.prism-dev-container"
else
	mkdir -p "$bundle_parent"
fi
chmod 700 "$dev_home" "$bundle_parent"
if [ -e "$final_bundle" ] || [ -L "$final_bundle" ]; then
	printf 'development bundle destination already exists: %s\n' "$final_bundle" >&2
	exit 1
fi
mv "$state_stage" "$final_bundle"
state_stage=
bundle_promoted=1
fail_install_for_test bundle

if [ -L "$target_path" ] || { [ -e "$target_path" ] && [ ! -f "$target_path" ]; }; then
	printf 'existing prism-dev command is not a regular file: %s\n' "$target_path" >&2
	exit 1
fi
if [ -e "$target_path" ]; then
	wrapper_backup="$(mktemp "$install_dir/.prism-dev-wrapper-backup.XXXXXX")"
	cp -p "$target_path" "$wrapper_backup"
	had_wrapper=1
fi
mv -f "$wrapper_stage" "$target_path"
wrapper_stage=
wrapper_promoted=1
fail_install_for_test wrapper
mv -f "$selector_stage" "$selector_path"
selector_stage=
committed=1

if [ -n "$old_bundle" ] && [ "$old_bundle" != "$final_bundle" ] && [ -f "$old_bundle/.prism-dev-bundle" ]; then
	rm -rf "$old_bundle" || printf 'warning: could not remove previous development bundle %s\n' "$old_bundle" >&2
fi
if [ "$container_displaced" -eq 1 ] && [ -n "$previous_state" ]; then
	rm -rf "$previous_state" || printf 'warning: could not remove previous development state %s\n' "$previous_state" >&2
	previous_state=
fi
if [ -n "$wrapper_backup" ]; then
	rm -f "$wrapper_backup"
	wrapper_backup=
fi
rm -rf "$install_lock"
lock_acquired=0
trap - EXIT

printf 'Installed %s\n' "$target_path"
printf 'Development state: %s\n' "$final_bundle"
printf 'Development state container: %s\n' "$dev_home"
printf 'Development runtime: %s\n' "$dev_runtime"
printf 'The default Prism state was not modified.\n'
