# AGENTS.md

- before fully completing a task that modifies code, run the full CI/tests with `scripts/full-check.sh`; if you just need to run format/tests use `scripts/check.sh` instead
- never modify, rename, or delete an existing file under `migrations/`; every schema change must be a new migration with the next version number
- for Prism debugging or logs, use `prism debug --help`
- don't modify AGENTS.md, CHANGELOG.md, or README.md unless specifically asked
