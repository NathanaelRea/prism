#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
scratch="$(mktemp -d "${TMPDIR:-/tmp}/prism-publication-check.XXXXXX")"
trap 'rm -rf "$scratch"' EXIT
fake="$scratch/prism-standard-extension"
printf '#!/bin/sh\nexit 0\n' >"$fake"
chmod 755 "$fake"

"$repo_root/scripts/publish-standard-pack.sh" x86_64-unknown-linux-gnu "$fake" "$scratch/one" >/dev/null
"$repo_root/scripts/publish-standard-pack.sh" x86_64-unknown-linux-gnu "$fake" "$scratch/two" >/dev/null
first="$scratch/one/prism-standard-pack-x86_64-unknown-linux-gnu.tar.gz"
second="$scratch/two/prism-standard-pack-x86_64-unknown-linux-gnu.tar.gz"
cmp "$first" "$second"
expected="$(awk '{print $1}' "$first.sha256")"
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "$first" | awk '{print $1}')"
else
  actual="$(shasum -a 256 "$first" | awk '{print $1}')"
fi
test "$actual" = "$expected"
tar -xzf "$first" -C "$scratch"
manifest="$scratch/prism.standard/prism-package.toml"
grep -Fq 'target = "x86_64-unknown-linux-gnu"' "$manifest"
grep -Fq 'id = "prism.standard/auto"' "$manifest"
grep -Fq 'id = "prism.standard/extension"' "$manifest"
printf 'Standard Pack publication contract passed\n'
