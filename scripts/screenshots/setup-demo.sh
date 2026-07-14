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
create_worktree "fix/review-comments" "$root/worktrees/review-comments" \
  "Review the comments on PR 17 and fix if applicable" "review.js" \
  'export const unresolved = comments => comments.filter(comment => !comment.resolved);'
create_worktree "fix/ci-green" "$root/worktrees/ci-green" \
  "Prepare the storefront health check" "health.js" \
  'export const health = () => ({ checkout: "ready" });'
create_worktree "feat/shipping-rates" "$root/worktrees/shipping-rates" \
  "Add regional shipping rates" "shipping.js" \
  'export const shippingRate = region => region === "local" ? 0 : 5;'
create_worktree "refactor/wishlist" "$root/worktrees/wishlist" \
  "Build the customer wishlist" "wishlist.js" \
  'export const wishlist = items => [...new Set(items)];'

create_repo() {
  local name="$1"
  local path="$root/work/$name"
  git init "$path" >/dev/null
  printf '# %s\n' "$name" >"$path/README.md"
  git -C "$path" add README.md
  git -C "$path" commit -m "Initial ${name} service" >/dev/null
}

create_repo "catalog"
create_repo "payments"
create_repo "fulfillment"

create_repo_worktree() {
  local repo_name="$1"
  local branch="$2"
  local path="$3"
  local summary="$4"
  local file="$5"
  git -C "$root/work/$repo_name" worktree add -b "$branch" "$path" main >/dev/null
  mkdir -p "$path/.agent/tasks"
  printf '{ "prompt_summary": "%s" }\n' "$summary" >"$path/.agent/tasks/task.json"
  printf '// %s\n' "$summary" >"$path/$file"
}

create_repo_worktree "catalog" "refactor/catalog-index" "$root/worktrees/catalog-index" \
  "Rebuild the catalog search index" "catalog-index.js"
create_repo_worktree "payments" "fix/payment-retries" "$root/worktrees/payment-retries" \
  "Fix duplicate payment retries" "payment-retries.js"
printf '\nCatalog sync pending.\n' >>"$root/work/catalog/README.md"

cat >"$config_dir/repos.toml" <<EOF
[[repos]]
path = "$repo"
key = "1"

[[repos]]
path = "$root/work/catalog"
key = "2"

[[repos]]
path = "$root/work/payments"
key = "3"

[[repos]]
path = "$root/work/fulfillment"
key = "4"
EOF

cat >"$config_dir/config.toml" <<EOF
default_agent = "opencode"
default_base = "main"
worktree_command = "wt"

[ui]
icon_style = "nerd-font"

[layout]
sidebar_width = 50

[auto]
require_review_approval = true

[worktrees]
columns = ["url"]

[tools]
gh = "$bin_dir/gh"
tmux = "$bin_dir/tmux"
wt = "$bin_dir/wt"
opencode = "$bin_dir/opencode"
EOF
