#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

errors=0
fail() {
	printf 'migration policy: %s\n' "$*" >&2
	errors=$((errors + 1))
}

check_current_directory() {
	local directory="$1"
	local expected=1
	local file name version expected_name
	local files=()
	shopt -s nullglob
	files=("$directory"/*.sql)
	shopt -u nullglob
	if [ "${#files[@]}" -eq 0 ]; then
		fail "$directory has no migrations"
		return
	fi
	for file in "${files[@]}"; do
		name="$(basename "$file")"
		if [ -L "$file" ] || [ ! -f "$file" ] || [ -x "$file" ]; then
			fail "$file must be a non-executable regular file"
			continue
		fi
		if [[ ! "$name" =~ ^[0-9]{4}_[a-z0-9][a-z0-9_]*\.sql$ ]]; then
			fail "$file must be named NNNN_lowercase_description.sql"
			continue
		fi
		version="${name%%_*}"
		printf -v expected_name '%04d' "$expected"
		if [ "$version" != "$expected_name" ]; then
			fail "$directory expected migration $expected_name but found $name"
			expected=$((10#$version + 1))
		else
			expected=$((expected + 1))
		fi
	done
}

check_current_directory migrations/repository
check_current_directory migrations/prompt-workflow

base="${1:-}"
if [ -n "$base" ]; then
	if [[ "$base" =~ ^0+$ ]]; then
		fail "comparison revision is the all-zero Git object"
	elif ! git cat-file -e "$base^{commit}" 2>/dev/null; then
		fail "comparison revision is unavailable: $base"
	else
		while IFS= read -r -d '' path; do
			entry="$(git ls-tree "$base" -- "$path")"
			mode="${entry%% *}"
			if [ "$mode" != 100644 ]; then
				fail "previously committed migration is not a regular non-executable file: $path"
			elif [ ! -e "$path" ] && [ ! -L "$path" ]; then
				fail "previously committed migration was removed: $path"
			elif [ -L "$path" ] || [ ! -f "$path" ]; then
				fail "previously committed migration changed file type: $path"
			elif ! git diff --quiet "$base" -- "$path"; then
				fail "previously committed migration was modified: $path"
			fi
		done < <(git ls-tree -r -z --name-only "$base" -- migrations)
	fi
else
	printf 'migration policy: no base revision supplied; checked current naming and sequence only\n'
fi

if [ "$errors" -ne 0 ]; then
	printf 'migration policy: %d violation(s)\n' "$errors" >&2
	exit 1
fi
printf 'migration policy: append-only history is valid\n'
