#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
install_dir="${PRISM_INSTALL_DIR:-$HOME/.local/bin}"
link_name="${1:-prism}"
target_path="$install_dir/$link_name"

cargo build --manifest-path "$script_dir/Cargo.toml" --release

mkdir -p "$install_dir"
ln -sfn "$script_dir/target/release/prism" "$target_path"

echo "Installed $target_path -> $script_dir/target/release/prism"
echo "Make sure $install_dir is on your PATH."
