#!/usr/bin/env bash
set -euo pipefail

die() {
  printf 'error: %s\n' "$*" >&2
  exit 2
}

require_env() {
  local name="$1"
  [[ -n "${!name:-}" ]] || die "$name is required"
}

require_env PRISM_DEMO_ROOT
require_env PRISM_DEMO_REPO
require_env PRISM_DEMO_ORIGIN
require_env HOME
require_env XDG_CONFIG_HOME
require_env XDG_CACHE_HOME
require_env XDG_STATE_HOME
require_env GIT_CONFIG_GLOBAL

case "$HOME:$XDG_CONFIG_HOME:$XDG_CACHE_HOME:$XDG_STATE_HOME:$GIT_CONFIG_GLOBAL" in
  "$PRISM_DEMO_ROOT"/*:"$PRISM_DEMO_ROOT"/*:"$PRISM_DEMO_ROOT"/*:"$PRISM_DEMO_ROOT"/*:"$PRISM_DEMO_ROOT"/*)
    ;;
  *)
    die "refusing to set up demo outside PRISM_DEMO_ROOT"
    ;;
esac

demo_url="https://github.com/prism-demo/shop.git"
bin_dir="$PRISM_DEMO_ROOT/bin"
config_dir="$XDG_CONFIG_HOME/prism"
lazygit_config_dir="$XDG_CONFIG_HOME/lazygit"
lazygit_state_dir="$XDG_STATE_HOME/lazygit"
fixtures_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/fixtures" && pwd)"
shims_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/shims" && pwd)"

mkdir -p "$bin_dir" "$config_dir" "$lazygit_config_dir" "$lazygit_state_dir" "$PRISM_DEMO_ROOT/state" "$PRISM_DEMO_ROOT/logs"

for tool in gh tmux wt wl-copy date; do
  cp "$shims_dir/$tool" "$bin_dir/$tool"
  chmod +x "$bin_dir/$tool"
done
for tool in missing-xclip missing-xsel missing-pbcopy; do
  cp "$shims_dir/missing-tool" "$bin_dir/$tool"
  chmod +x "$bin_dir/$tool"
done
cp "$fixtures_dir/prs.json" "$PRISM_DEMO_ROOT/state/prs.json"
: >"$PRISM_DEMO_ROOT/state/clipboard.txt"

git config --global init.defaultBranch main
git config --global user.name "Prism Demo"
git config --global user.email "demo@prism.local"
git config --global advice.detachedHead false
git config --global "url.file://$PRISM_DEMO_ORIGIN.insteadOf" "$demo_url"

git init --bare "$PRISM_DEMO_ORIGIN" >/dev/null
git --git-dir="$PRISM_DEMO_ORIGIN" symbolic-ref HEAD refs/heads/main

git init "$PRISM_DEMO_REPO" >/dev/null
git -C "$PRISM_DEMO_REPO" remote add origin "$demo_url"
printf '.agent/\n/prism\n' >>"$PRISM_DEMO_REPO/.git/info/exclude"

mkdir -p "$PRISM_DEMO_REPO/src" "$PRISM_DEMO_REPO/docs"
cat >"$PRISM_DEMO_REPO/README.md" <<'EOF'
# Prism Shop

A demo storefront used by the Prism screenshot pipeline.
EOF
cat >"$PRISM_DEMO_REPO/src/catalog.js" <<'EOF'
export const products = [
  { sku: "tea-001", name: "Jasmine Tea", inventory: 18 },
  { sku: "mug-002", name: "Travel Mug", inventory: 7 },
];
EOF
cat >"$PRISM_DEMO_REPO/src/checkout.js" <<'EOF'
export function checkout(cart) {
  return { totalItems: cart.length, status: "ready" };
}
EOF
cat >"$PRISM_DEMO_REPO/docs/runbook.md" <<'EOF'
# Storefront Runbook

- Keep inventory updates small.
- Review checkout changes before release.
EOF
git -C "$PRISM_DEMO_REPO" add README.md src docs
git -C "$PRISM_DEMO_REPO" commit -m "Initial storefront skeleton" >/dev/null
git -C "$PRISM_DEMO_REPO" push -u origin main >/dev/null

touch "$lazygit_config_dir/config.yml"
cat >"$lazygit_state_dir/state.yml" <<EOF
lastupdatecheck: 0
recentrepos:
    - $PRISM_DEMO_REPO
startuppopupversion: 5
didshowhunkstaginghint: true
lastversion: 0.61.1
customcommandshistory: []
hidecommandlog: false
githubPullRequests: {}
EOF

cat >"$config_dir/repos.toml" <<EOF
[[repos]]
path = "$PRISM_DEMO_REPO"
key = "1"
EOF

cat >"$config_dir/config.toml" <<EOF
default_agent = "opencode"
default_base = "main"
worktree_command = "wt"

[worktrees]
columns = ["url", "vars.localdev", "ci"]

[tools]
gh = "$bin_dir/gh"
tmux = "$bin_dir/tmux"
wt = "$bin_dir/wt"
wl-copy = "$bin_dir/wl-copy"
xclip = "$bin_dir/missing-xclip"
xsel = "$bin_dir/missing-xsel"
pbcopy = "$bin_dir/missing-pbcopy"

[prompt_templates]
review_fix = "Fix PR {pr_number} on {branch}:\\n\\n{comments}"
EOF

printf 'Fake repository: %s\n' "$PRISM_DEMO_REPO"
printf 'Fake origin: %s\n' "$PRISM_DEMO_ORIGIN"
printf 'Prism config: %s\n' "$config_dir/config.toml"
