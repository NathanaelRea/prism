# AGENTS.md

- before completing any task that changes code, run the full CI/tests with `scripts/full-check.sh`; if you just need a basic check use `scripts/check.sh` instead
- never modify, rename, or delete an existing file under `migrations/`; every schema change must be a new migration with the next version number
- for Prism debugging or logs, use `prism debug --help`
- don't modify AGENTS.md, CHANGELOG.md, or README.md unless specifically asked
