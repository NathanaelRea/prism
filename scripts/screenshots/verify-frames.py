#!/usr/bin/env python3
"""Verify retained demo frames and the scenario endpoint after a capture."""

from __future__ import annotations

import json
import sys
from pathlib import Path

sys.dont_write_bytecode = True

from scenario import ORDER


def main() -> None:
    sandbox = Path(sys.argv[1])
    with (sandbox / "state" / "scenario.json").open(encoding="utf-8") as handle:
        history = json.load(handle).get("history", [])
    if history != ORDER:
        raise SystemExit(f"scenario history does not cover the complete reel: {history}")

    frame_names = [
        "01-opening.png",
        "02-worktree.png",
        "03-plan-drafted.png",
        "04-plan-complete.png",
        "05-lazygit-commit.png",
        "06-failed-pr.png",
        "07-review-pushed.png",
        "08-ci-pushed.png",
        "09-merged.png",
    ]
    missing = [
        name
        for name in frame_names
        if not (sandbox / "frames" / name).is_file()
        or (sandbox / "frames" / name).stat().st_size == 0
    ]
    if missing:
        raise SystemExit(f"missing reel milestone frames: {', '.join(missing)}")
    print(f"verified {len(frame_names)} frames and all scenario milestones")


if __name__ == "__main__":
    main()
