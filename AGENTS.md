# AGENTS.md

- before completing any task that changes code, run `scripts/check.sh`; this is the fast native gate for fmt/build/test/etc
- before pushing, run `scripts/full-check.sh`; CI runs this exhaustive metadata gate natively on Linux and macOS
- never modify, rename, or delete an existing file under `migrations/`; every schema change must be a new migration with the next version number
- for Prism debugging or logs, use `prism debug --help`
- don't modify AGENTS.md, CHANGELOG.md, or README.md unless specifically asked
