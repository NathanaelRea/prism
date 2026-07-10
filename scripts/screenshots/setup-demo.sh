#!/usr/bin/env bash
set -euo pipefail

die() {
  printf 'error: %s\n' "$*" >&2
  exit 2
}

for name in PRISM_DEMO_ROOT PRISM_DEMO_REPO PRISM_DEMO_ORIGIN HOME XDG_CONFIG_HOME XDG_CACHE_HOME XDG_STATE_HOME GIT_CONFIG_GLOBAL PRISM_DEMO_OPENCODE PRISM_DEMO_LAZYGIT; do
  [[ -n "${!name:-}" ]] || die "$name is required"
done

case "$HOME:$XDG_CONFIG_HOME:$XDG_CACHE_HOME:$XDG_STATE_HOME:$GIT_CONFIG_GLOBAL" in
  "$PRISM_DEMO_ROOT"/*:"$PRISM_DEMO_ROOT"/*:"$PRISM_DEMO_ROOT"/*:"$PRISM_DEMO_ROOT"/*:"$PRISM_DEMO_ROOT"/*) ;;
  *) die "refusing to set up demo outside PRISM_DEMO_ROOT" ;;
esac

demo_url="https://github.com/prism-demo/shop.git"
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
bin_dir="$PRISM_DEMO_ROOT/bin"
config_dir="$XDG_CONFIG_HOME/prism"
lazygit_config_dir="$XDG_CONFIG_HOME/lazygit"
lazygit_state_dir="$XDG_STATE_HOME/lazygit"

mkdir -p "$bin_dir" "$config_dir" "$lazygit_config_dir" "$lazygit_state_dir" "$PRISM_DEMO_ROOT/state" "$PRISM_DEMO_ROOT/logs"
for tool in gh tmux wt wl-copy date fzf git; do
  cp "$script_dir/shims/$tool" "$bin_dir/$tool"
  chmod +x "$bin_dir/$tool"
done
for tool in missing-xclip missing-xsel missing-pbcopy; do
  cp "$script_dir/shims/missing-tool" "$bin_dir/$tool"
  chmod +x "$bin_dir/$tool"
done
cat >"$bin_dir/mise" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

if [[ "${1:-}" == "where" && "${2:-}" == "node@latest" ]]; then
  printf '%s\n' "${PRISM_DEMO_NODE_ROOT:?PRISM_DEMO_NODE_ROOT is required}"
  exit 0
fi
printf 'the Prism demo only permits mise where node@latest\n' >&2
exit 64
EOF
chmod +x "$bin_dir/mise"
cp "$script_dir/fixtures/demo-state.json" "$PRISM_DEMO_ROOT/state/scenario.json"
cp "$script_dir/scenario.py" "$bin_dir/prism-demo-scenario"
chmod +x "$bin_dir/prism-demo-scenario"
: >"$PRISM_DEMO_ROOT/state/clipboard.txt"

# Prism invokes the pinned real binaries through these wrappers, never a contributor config.
cat >"$bin_dir/opencode" <<EOF
#!/usr/bin/env bash
set -euo pipefail
exec "$PRISM_DEMO_OPENCODE" "\$@"
EOF
chmod +x "$bin_dir/opencode"
cat >"$bin_dir/lazygit" <<EOF
#!/usr/bin/env bash
set -euo pipefail
exec "$PRISM_DEMO_LAZYGIT" --use-config-dir "$lazygit_config_dir" "\$@"
EOF
chmod +x "$bin_dir/lazygit"

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

mkdir -p "$PRISM_DEMO_REPO/src" "$PRISM_DEMO_REPO/docs" "$PRISM_DEMO_REPO/scripts" "$PRISM_DEMO_REPO/.github/workflows"
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
cat >"$PRISM_DEMO_REPO/scripts/check-ci.sh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
grep -q 'status: "ready"' src/checkout.js
printf 'checkout CI fixture passed\n'
EOF
chmod +x "$PRISM_DEMO_REPO/scripts/check-ci.sh"
cat >"$PRISM_DEMO_REPO/.github/workflows/ci.yml" <<'EOF'
name: CI
on: [push, pull_request]
jobs:
  checkout:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: ./scripts/check-ci.sh
EOF
cat >"$PRISM_DEMO_REPO/docs/runbook.md" <<'EOF'
# Storefront Runbook

- Keep inventory updates small.
- Review checkout changes before release.
EOF
git -C "$PRISM_DEMO_REPO" add README.md src docs scripts .github
git -C "$PRISM_DEMO_REPO" commit -m "Initial storefront skeleton" >/dev/null
git -C "$PRISM_DEMO_REPO" push -u origin main >/dev/null

cat >"$lazygit_config_dir/config.yml" <<'EOF'
gui:
  showCommandLog: false
  showBottomLine: false
EOF
cat >"$lazygit_state_dir/state.yml" <<EOF
lastupdatecheck: 0
recentrepos:
    - $PRISM_DEMO_REPO
startuppopupversion: 5
didshowhunkstaginghint: true
lastversion: 0.62.1
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

[ui]
icon_style = "nerd-font"

[worktrees]
columns = ["url", "vars.localdev", "ci"]

[tools]
gh = "$bin_dir/gh"
tmux = "$bin_dir/tmux"
wt = "$bin_dir/wt"
fzf = "$bin_dir/fzf"
opencode = "$bin_dir/opencode"
lazygit = "$bin_dir/lazygit"
wl-copy = "$bin_dir/wl-copy"
xclip = "$bin_dir/missing-xclip"
xsel = "$bin_dir/missing-xsel"
pbcopy = "$bin_dir/missing-pbcopy"

[prompt_templates]
review_fix = "Fix PR {pr_number} on {branch}:\\n\\n{comments}"
repair_commit_review = "fix: review feedback"
repair_commit_ci = "fix: ci failure"
EOF

printf 'Fake repository: %s\n' "$PRISM_DEMO_REPO"
printf 'Fake origin: %s\n' "$PRISM_DEMO_ORIGIN"
printf 'Prism config: %s\n' "$config_dir/config.toml"
