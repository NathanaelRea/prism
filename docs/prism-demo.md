# Prism Demo GIF

Refresh the README reel with:

```sh
./scripts/screenshot.sh --keep --frames
```

The harness builds Prism, creates a sandbox-local repository, runs the VHS storyboard, and writes `docs/prism-demo.gif`. `--check` validates the sandbox, pinned tools, local provider, real OpenCode server, and shims without replacing the GIF.

## Reproducibility

The committed path is offline. It uses real Prism, Git, tmux, VHS `0.11.0`, OpenCode `1.17.18`, and LazyGit `0.62.1`; VHS, OpenCode, and LazyGit are version-checked before setup. The harness resolves the cached native `opencode-ai@1.17.18` executable with offline `npx`, then invokes OpenCode and LazyGit through sandbox wrappers with isolated `HOME`, XDG paths, Git config, OpenCode config, and LazyGit config.

`scripts/screenshots/mock-provider.py` implements the OpenAI-compatible local endpoint used by OpenCode. It deterministically writes `plan-ci.md`, applies the checkout fixture change, and prepares the repair edits. It never reads credentials or contacts a provider. The `gh` shim is also local and derives PR reviews, checks, guarded pushes, and merge eligibility from `state/scenario.json`.

The scenario progresses through `new_session`, plan drafting and execution, initial commit, open PR, review and CI repairs, their guarded pushes, and merge. `scripts/screenshots/scenario.py` rejects out-of-order transitions. The Git wrapper delegates Prism-managed operations to real Git and observes successful repair commits and pushes; PR creation confirms the real LazyGit commit boundary.

The tape selects `JetBrainsMono Nerd Font`, required for Prism's `icon_style = "nerd-font"`. Install that font before recording. VHS `0.11.0` uses go-rod Chromium revision `1321438`; the harness reuses that exact managed browser from `~/.cache/rod/browser/` rather than allowing VHS to select an arbitrary system Chromium. Set `PRISM_DEMO_CHROME_BINARY` to override the browser explicitly.

## Live-Model Spike

The fixture was designed for a disposable, manual DeepSeek spike, not for committed capture. Use a temporary credential outside the sandbox, pin the same OpenCode version, record request/response and tool-call traces, and remove the credential before converting the successful transcript into `mock-provider.py`. Never add credentials or a network endpoint to the screenshot configuration.

## Debugging

Use `--keep --frames` to retain `target/screenshots/<run>/`. Useful files are `run.env`, `logs/provider.log`, `logs/gh.log`, `logs/opencode-server.log`, `state/scenario.json`, the rendered tape, and `frames/`.

After a full capture, check the nine visible milestones with:

```sh
scripts/screenshots/verify-frames.py target/screenshots/<run>
```

The checker requires the named opening, worktree, plan, LazyGit, failed-PR, guarded-push, and merged frames plus the full state history. Visually inspect those frames and the generated GIF before accepting a regenerated reel.
