import fcntl
import json
import os
import pathlib
import sys
import time


def root():
        configured = os.environ.get("PRISM_E2E_ROOT")
        if configured:
                return pathlib.Path(configured)
        return pathlib.Path(__file__).resolve().parent.parent


def log_event(tool, argv, stdin=None, unsupported=False, **extra):
        state = root() / "state"
        state.mkdir(parents=True, exist_ok=True)
        events = state / "events.jsonl"
        order = state / "event-order"
        with events.open("a+", encoding="utf-8") as handle:
                fcntl.flock(handle.fileno(), fcntl.LOCK_EX)
                try:
                        try:
                                sequence = int(order.read_text(encoding="utf-8")) + 1
                        except (FileNotFoundError, ValueError):
                                sequence = 1
                        temporary = order.with_suffix(".tmp")
                        temporary.write_text(str(sequence), encoding="utf-8")
                        os.replace(temporary, order)
                        event = {
                                "sequence": sequence,
                                "timestamp_ns": time.time_ns(),
                                "pid": os.getpid(),
                                "tool": tool,
                                "cwd": os.getcwd(),
                                "argv": list(argv),
                                "stdin": stdin,
                                "unsupported": unsupported,
                                "env": {
                                        name: os.environ.get(name)
                                        for name in (
                                                "HOME",
                                                "XDG_CONFIG_HOME",
                                                "XDG_CACHE_HOME",
                                                "XDG_STATE_HOME",
                                                "GIT_CONFIG_GLOBAL",
                                                "PRISM_RUNTIME_DIR",
                                                "TMUX_TMPDIR",
                                                "TMUX",
                                                "TERM",
                                        )
                                },
                        }
                        event.update(extra)
                        handle.write(json.dumps(event, sort_keys=True) + "\n")
                        handle.flush()
                        os.fsync(handle.fileno())
                finally:
                        fcntl.flock(handle.fileno(), fcntl.LOCK_UN)


def reject(tool, argv, message="unsupported invocation"):
        log_event(tool, argv, unsupported=True, error=message)
        print(f"{tool} E2E adapter: {message}: {argv!r}", file=sys.stderr)
        raise SystemExit(64)


def private_env():
        environment = os.environ.copy()
        for name in list(environment):
                upper = name.upper()
                if any(
                        token in upper
                        for token in (
                                "GITHUB_TOKEN",
                                "GH_TOKEN",
                                "GITLAB_TOKEN",
                                "GLAB_TOKEN",
                                "OPENAI_API_KEY",
                                "ANTHROPIC_API_KEY",
                                "CODEBERG_TOKEN",
                                "FORGEJO_TOKEN",
                        )
                ):
                        environment.pop(name, None)
        environment["HOME"] = str(root() / "home")
        environment["XDG_CONFIG_HOME"] = str(root() / "config")
        environment["XDG_CACHE_HOME"] = str(root() / "cache")
        environment["XDG_STATE_HOME"] = str(root() / "xdg-state")
        environment["GIT_CONFIG_GLOBAL"] = str(root() / "gitconfig")
        environment["GIT_CONFIG_NOSYSTEM"] = "1"
        environment["PRISM_RUNTIME_DIR"] = os.environ.get(
                "PRISM_RUNTIME_DIR", str(root() / "runtime")
        )
        environment["TZ"] = "UTC"
        environment["LC_ALL"] = "C"
        environment["LANG"] = "C"
        return environment
