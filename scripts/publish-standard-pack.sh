#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
target="${1:-}"
binary="${2:-}"
output_dir="${3:-$repo_root/dist}"

hash_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

case "$target" in
  x86_64-unknown-linux-gnu|aarch64-apple-darwin|x86_64-apple-darwin) ;;
  *)
    printf 'usage: %s <supported-target> [prebuilt-extension] [output-dir]\n' "$0" >&2
    exit 2
    ;;
esac

if [ -z "$binary" ]; then
  cargo build --locked --release --target "$target" --package prism-standard-extension
  binary="$repo_root/target/$target/release/prism-standard-extension"
fi
if [ ! -f "$binary" ]; then
  printf 'Standard Extension executable not found: %s\n' "$binary" >&2
  exit 1
fi

stage="$(mktemp -d "${TMPDIR:-/tmp}/prism-standard-pack.XXXXXX")"
trap 'rm -rf "$stage"' EXIT
package="$stage/prism.standard"
mkdir -p "$package/workflows" "$package/extensions" "$package/schemas" \
  "$package/skills" "$package/templates"

manifest="$package/prism-package.toml"
printf 'schema_version = 1\nid = "prism.standard"\nversion = "%s"\n' \
  "$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$repo_root/Cargo.toml" | head -1)" >"$manifest"

append_resource() {
  local id="$1" kind="$2" relative="$3"
  local digest
  digest="$(hash_file "$package/$relative")"
  printf '\n[[resources]]\nid = "%s"\nkind = "%s"\npath = "%s"\nsha256 = "%s"\n' \
    "$id" "$kind" "$relative" "$digest" >>"$manifest"
}

for name in plan implement auto stabilize-change-request triage-issues; do
  {
    printf '# Standard Pack working copy; execution is enabled by the definition compiler.\n'
    printf 'id = "prism.standard/%s"\n' "$name"
    sed '/^id[[:space:]]*=/d' "$repo_root/assets/workflows/$name.toml"
  } >"$package/workflows/$name.toml"
  append_resource "prism.standard/$name" workflow "workflows/$name.toml"
done

for name in extension-authoring package-authoring workflow-authoring workflow-customization workflow-diagnostics; do
  cp "$repo_root/assets/skills/$name.md" "$package/skills/$name.md"
  append_resource "prism.standard/$name" skill "skills/$name.md"
done
for name in workflow-v2.toml implementation.md review-repair.md; do
  cp "$repo_root/assets/templates/$name" "$package/templates/$name"
  append_resource "prism.standard/${name%.*}-template" template "templates/$name"
done
cp "$repo_root/assets/schemas/workflow-definition-v2.json" "$package/schemas/workflow-definition-v2.json"
append_resource "prism.standard/workflow-definition-schema" artifact_schema \
  "schemas/workflow-definition-v2.json"

cp "$binary" "$package/extensions/prism-standard-extension"
chmod 755 "$package/extensions/prism-standard-extension"
extension_digest="$(hash_file "$package/extensions/prism-standard-extension")"
append_resource "prism.standard/extension" extension "extensions/prism-standard-extension"
printf '\n[[extensions]]\nid = "prism.standard/extension"\ncapabilities = ["agent:run", "workspace:write", "process:run", "provider:read", "provider:write", "git:write", "worktrunk:write"]\n\n[[extensions.targets]]\ntarget = "%s"\npath = "extensions/prism-standard-extension"\nsha256 = "%s"\n' \
  "$target" "$extension_digest" >>"$manifest"

# Fixed metadata plus lexical input order makes repeated publication byte-for-byte reproducible.
find "$package" -exec touch -t 197001010000 {} +
mkdir -p "$output_dir"
archive="$output_dir/prism-standard-pack-$target.tar.gz"
if tar --version 2>/dev/null | head -1 | grep -q GNU; then
  tar --sort=name --mtime=@0 --owner=0 --group=0 --numeric-owner --format=ustar \
    -C "$stage" -cf - prism.standard | gzip -n >"$archive"
else
  COPYFILE_DISABLE=1 tar --format ustar --uid 0 --gid 0 --numeric-owner \
    -C "$stage" -cf - prism.standard | gzip -n >"$archive"
fi
printf '%s  %s\n' "$(hash_file "$archive")" "$(basename "$archive")" >"$archive.sha256"
printf '%s\n' "$archive"
