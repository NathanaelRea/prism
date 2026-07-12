#!/usr/bin/env python3
"""Own the deterministic Prism demo state and its observable transitions."""

from __future__ import annotations

import json
import os
import subprocess
import sys
import time
from pathlib import Path


ORDER = [
    "new_session",
    "plan_drafted",
    "plan_complete",
    "initial_commit",
    "pr_open",
    "review_repaired",
    "review_pushed",
    "ci_repaired",
    "ci_pushed",
    "merged",
]


def state_path() -> Path:
    return Path(os.environ["PRISM_DEMO_ROOT"]) / "state" / "scenario.json"


def load() -> dict[str, object]:
    with state_path().open(encoding="utf-8") as handle:
        return json.load(handle)


def save(state: dict[str, object]) -> None:
    with state_path().open("w", encoding="utf-8") as handle:
        json.dump(state, handle, indent=2)
        handle.write("\n")


def transition(target: str) -> None:
    state = load()
    current = str(state["state"])
    if current == target:
        return
    try:
        valid = ORDER.index(target) == ORDER.index(current) + 1
    except ValueError:
        valid = False
    if not valid:
        raise SystemExit(f"invalid scenario transition: {current} -> {target}")
    state["state"] = target
    state.setdefault("history", []).append(target)
    save(state)


def repo() -> Path:
    root = Path(os.environ["PRISM_DEMO_REPO"])
    raw = subprocess.check_output(
        ["git", "-C", str(root), "worktree", "list", "--porcelain"], text=True
    )
    paths = [Path(line.removeprefix("worktree ")) for line in raw.splitlines() if line.startswith("worktree ")]
    return next((path for path in paths if path != root), root)


def write_plan() -> None:
    path = repo() / "plan-ci.md"
    path.write_text(
        """# CI/CD Investigation Plan

## Phase 1: Inspect Workflows

Read `.github/workflows/ci.yml` and identify the checks that protect checkout changes.

## Phase 2: Make The Fixture Observable

Add a small checkout assertion so the local CI fixture exercises the release path.

## Phase 3: Verify The Change

Run `./scripts/check-ci.sh` and summarize the PR follow-up work.
""",
        encoding="utf-8",
    )
    transition("plan_drafted")


def apply_plan() -> None:
    target = repo() / "src" / "checkout.js"
    text = target.read_text(encoding="utf-8")
    if 'ci: "verified"' not in text:
        target.write_text(
            text.replace('status: "ready"', 'status: "ready", ci: "verified"'),
            encoding="utf-8",
        )
    subprocess.run(["./scripts/check-ci.sh"], cwd=repo(), check=True)
    transition("plan_complete")


def repair(kind: str) -> None:
    current = str(load()["state"])
    expected = "pr_open" if kind == "review" else "review_pushed"
    if current != expected:
        return
    path = repo() / "src" / "checkout.js"
    text = path.read_text(encoding="utf-8")
    marker = "reviewed" if kind == "review" else "ciGreen"
    if marker not in text:
        path.write_text(text.replace('ci: "verified"', f'ci: "verified", {marker}: true'), encoding="utf-8")
    # Keep each blocker visible long enough for the recorder to capture the transition.
    time.sleep(3)


def git_event(event: str, message: str = "") -> None:
    current = str(load()["state"])
    if event == "commit":
        if current == "plan_complete":
            transition("initial_commit")
        elif current == "pr_open" and "review" in message.lower():
            transition("review_repaired")
        elif current == "review_pushed" and "ci" in message.lower():
            transition("ci_repaired")
    elif event == "push":
        if current == "review_repaired":
            transition("review_pushed")
        elif current == "ci_repaired":
            transition("ci_pushed")


def main() -> None:
    command = sys.argv[1:]
    if not command:
        print(json.dumps(load()))
        return
    if command[0] == "transition":
        transition(command[1])
    elif command[0] == "write-plan":
        write_plan()
    elif command[0] == "apply-plan":
        apply_plan()
    elif command[0] == "repair":
        repair(command[1])
    elif command[0] == "git-event":
        git_event(command[1], command[2] if len(command) > 2 else "")
    else:
        raise SystemExit(f"unknown scenario command: {command[0]}")


if __name__ == "__main__":
    main()
