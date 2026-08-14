#!/usr/bin/env bash
set -euo pipefail

# Ratchet for module-wide dead-code/import suppression. These legacy modules are
# explicit debt: removing an entry is welcome; adding one requires review here.
readonly -a legacy_allowlist=(
	"src/agent_runtime/agent.rs"
	"src/agent_runtime/harness.rs"
	"src/agent_runtime/tmux.rs"
	"src/config.rs"
	"src/persistence/cli_test_support.rs"
	"src/persistence/session.rs"
	"src/remote/mod.rs"
	"src/repository/session.rs"
	"src/telemetry/observability.rs"
	"src/view/mod.rs"
)

is_legacy_path() {
	local candidate="$1"
	local allowed
	for allowed in "${legacy_allowlist[@]}"; do
		if [ "$candidate" = "$allowed" ]; then
			return 0
		fi
	done
	return 1
}

scan_tree() {
	local root="$1"
	local relative_prefix="${2:-}"
	local failed=0
	local file relative

	while IFS= read -r -d '' file; do
		relative="${file#"$root"/}"
		if [ -n "$relative_prefix" ]; then
			relative="$relative_prefix/$relative"
		fi

		# Inner attributes are module-wide. Collect the complete opening attribute
		# so multiline `#![allow(...)]` forms cannot bypass this check.
		if awk '
      BEGIN { in_allow = 0; forbidden = 0 }
      /^[[:space:]]*#!\[allow[[:space:]]*\(/ {
        in_allow = 1
        attribute = $0 "\n"
        if (/\)\][[:space:]]*$/) {
          if (attribute ~ /(^|[^[:alnum:]_])(dead_code|unused_imports)([^[:alnum:]_]|$)/) forbidden = 1
          in_allow = 0
          attribute = ""
        }
        next
      }
      in_allow { attribute = attribute $0 "\n" }
      in_allow && /\)\][[:space:]]*$/ {
        if (attribute ~ /(^|[^[:alnum:]_])(dead_code|unused_imports)([^[:alnum:]_]|$)/) forbidden = 1
        in_allow = 0
        attribute = ""
      }
      END { exit forbidden ? 0 : 1 }
    ' "$file"; then
			if ! is_legacy_path "$relative"; then
				printf 'forbidden module-wide dead-code/import allow: %s\n' "$relative" >&2
				failed=1
			fi
		fi
	done < <(find "$root" -type f -name '*.rs' -print0)

	return "$failed"
}

self_test() {
	local fixture
	fixture="$(mktemp -d "${TMPDIR:-/tmp}/prism-lint-policy.XXXXXX")"
	trap 'rm -rf "$fixture"' RETURN

	mkdir -p "$fixture/src"
	cat >"$fixture/src/forbidden.rs" <<'EOF'
#![allow(
    dead_code,
    reason = "fixture must be rejected"
)]
fn unused() {}
EOF
	if scan_tree "$fixture/src" "fixture/src" >/dev/null 2>&1; then
		printf 'lint policy self-test failed: module-wide fixture was accepted\n' >&2
		return 1
	fi

	cat >"$fixture/src/forbidden.rs" <<'EOF'
#[allow(dead_code, reason = "fixture demonstrates a narrow optional API")]
fn unused() {}
EOF
	if ! scan_tree "$fixture/src" "fixture/src" >/dev/null 2>&1; then
		printf 'lint policy self-test failed: reasoned item-level allow was rejected\n' >&2
		return 1
	fi
}

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

self_test
scan_tree "$repo_root/src" "src"
printf 'lint policy passed (%s explicit legacy module-wide allow entries)\n' "${#legacy_allowlist[@]}"
