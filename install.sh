#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
install_dir="${PRISM_INSTALL_DIR:-$HOME/.local/bin}"
link_name="${1:-prism}"
target_path="$install_dir/$link_name"
extension_name="prism-standard-extension"
extension_path="$install_dir/$extension_name"

if [[ "$target_path" == "$extension_path" ]]; then
  echo "Prism executable name cannot be $extension_name" >&2
  exit 1
fi

cargo build \
  --manifest-path "$script_dir/Cargo.toml" \
  --release \
  --locked \
  --package prism \
  --package prism-standard-extension

mkdir -p "$install_dir"
temporary_path="$(mktemp "$install_dir/.${link_name}.XXXXXX")"
temporary_extension_path="$(mktemp "$install_dir/.${extension_name}.XXXXXX")"
trap 'rm -f "$temporary_path" "$temporary_extension_path"' EXIT
install -m 755 "$script_dir/target/release/prism" "$temporary_path"
install -m 755 "$script_dir/target/release/$extension_name" "$temporary_extension_path"
mv -f "$temporary_extension_path" "$extension_path"
mv -f "$temporary_path" "$target_path"
trap - EXIT

echo "Installed $target_path"
echo "Installed $extension_path"
echo "Make sure $install_dir is on your PATH."
