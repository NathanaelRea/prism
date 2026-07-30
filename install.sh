#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
install_dir="${PRISM_INSTALL_DIR:-$HOME/.local/bin}"
link_name="${1:-prism}"
target_path="$install_dir/$link_name"

cargo build --manifest-path "$script_dir/Cargo.toml" --release --locked

mkdir -p "$install_dir"
temporary_path="$(mktemp "$install_dir/.${link_name}.XXXXXX")"
trap 'rm -f "$temporary_path"' EXIT
install -m 755 "$script_dir/target/release/prism" "$temporary_path"
mv -f "$temporary_path" "$target_path"
trap - EXIT

echo "Installed $target_path"
echo "Make sure $install_dir is on your PATH."
