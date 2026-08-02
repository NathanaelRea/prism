#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
host="${1:-${PRISM_MAC_HOST:-}}"
remote_dir="${2:-${PRISM_MAC_DIR:-}}"

if [ -z "$host" ] || [ -z "$remote_dir" ]; then
  printf 'usage: %s <ssh-host> [remote-directory]\n' "$0" >&2
  printf 'or set PRISM_MAC_HOST and PRISM_MAC_DIR\n' >&2
  exit 2
fi

if [[ "$remote_dir" == "~/"* ]]; then
  remote_dir="${remote_dir#\~/}"
fi

case "$remote_dir" in
  ""|/*|.|..|../*|*/../*|*/..|*[!A-Za-z0-9_./-]*)
    printf 'remote directory must be a safe path relative to the remote home or start with ~/: %s\n' "$remote_dir" >&2
    exit 2
    ;;
esac

for command in rsync ssh; do
  if ! command -v "$command" >/dev/null 2>&1; then
    printf 'missing remote smoke dependency: %s\n' "$command" >&2
    exit 1
  fi
done

if ! ssh "$host" "test -e $remote_dir/.git"; then
  printf 'remote directory must already be a Git checkout or worktree: %s\n' "$remote_dir" >&2
  exit 1
fi
rsync -az --delete \
  --exclude .git/ \
  --exclude target/ \
  "$repo_root/" "$host:$remote_dir/"
ssh "$host" "cd '$remote_dir' && scripts/platform-smoke.sh"
