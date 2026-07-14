#!/usr/bin/env bash
set -euo pipefail

root="${PRISM_DEMO_ROOT:?PRISM_DEMO_ROOT is required}"
repo="${PRISM_DEMO_REPO:?PRISM_DEMO_REPO is required}"
origin="${PRISM_DEMO_ORIGIN:?PRISM_DEMO_ORIGIN is required}"
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
bin_dir="$root/bin"
config_dir="$XDG_CONFIG_HOME/prism"
demo_url="https://github.com/prism-demo/shop.git"

mkdir -p "$bin_dir" "$config_dir" "$root/state" "$root/logs"
for tool in gh git opencode tmux wt; do
  cp "$script_dir/shims/$tool" "$bin_dir/$tool"
  chmod +x "$bin_dir/$tool"
done
git config --global init.defaultBranch main
git config --global user.name "Prism Demo"
git config --global user.email "demo@prism.local"
git config --global advice.detachedHead false
git config --global "url.file://$origin.insteadOf" "$demo_url"

git init --bare "$origin" >/dev/null
git --git-dir="$origin" symbolic-ref HEAD refs/heads/main
git init "$repo" >/dev/null
git -C "$repo" remote add origin "$demo_url"
printf '.agent/\n/prism\n' >>"$repo/.git/info/exclude"
mkdir -p "$repo/src"
printf '# Prism Shop\n' >"$repo/README.md"
printf 'export const checkout = () => ({ status: "ready" });\n' >"$repo/src/checkout.js"
git -C "$repo" add README.md src/checkout.js
git -C "$repo" commit -m "Initial storefront" >/dev/null
git -C "$repo" push -u origin main >/dev/null

create_worktree() {
  local branch="$1"
  local path="$2"
  local summary="$3"
  local file="$4"
  local content="$5"
  git -C "$repo" worktree add -b "$branch" "$path" main >/dev/null
  printf '%s\n' "$content" >"$path/src/$file"
  git -C "$path" add "src/$file"
  git -C "$path" commit -m "$summary" >/dev/null
  git -C "$path" push -u origin "$branch" >/dev/null
  mkdir -p "$path/.agent/tasks"
  printf '{ "prompt_summary": "%s" }\n' "$summary" >"$path/.agent/tasks/task.json"
}

create_worktree "feat/agent-session" "$root/worktrees/agent-session" \
  "Improve recommendations while checks run" "recommendations.js" \
  'export const recommendations = () => ["Jasmine Tea"];'
create_worktree "feat/review-fix" "$root/worktrees/review-fix" \
  "Resolve review feedback on checkout" "review.js" \
  'export const unresolved = comments => comments.filter(comment => !comment.resolved);'
create_worktree "feat/ci-green" "$root/worktrees/ci-green" \
  "Prepare the storefront health check" "health.js" \
  'export const health = () => ({ checkout: "ready" });'

cat >"$config_dir/repos.toml" <<EOF
[[repos]]
path = "$repo"
key = "1"
EOF

cat >"$config_dir/config.toml" <<EOF
default_agent = "opencode"
default_base = "main"
worktree_command = "wt"

[ui]
icon_style = "nerd-font"

[layout]
sidebar_width = 46

[auto]
require_review_approval = true

[worktrees]
columns = ["url", "ci"]

[tools]
gh = "$bin_dir/gh"
tmux = "$bin_dir/tmux"
wt = "$bin_dir/wt"
opencode = "$bin_dir/opencode"
EOF
