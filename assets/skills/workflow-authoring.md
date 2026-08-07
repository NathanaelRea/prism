# Author and Debug Workflow Definitions

Use Prism's public authoring commands; never edit the Run Ledger, immutable store, or package
lock directly.

1. Start with `prism workflow new <qualified-id>` or copy an existing definition with
   `prism workflow copy <source-id> <new-id>`.
2. Keep every input/output typed, every dependency explicit when branching, every repeat bounded,
   and every implementation or child workflow reference qualified.
3. Run `prism workflow validate <id-or-path>` after each structural edit.
4. Run `prism workflow preview <id>` to inspect the complete definition, package, schema, and
   executable closure without executing it.
5. Use `prism workflow history <run-id> --json`, `prism debug info`, and `prism workflow retry`
   when diagnosing a run. Do not mutate SQLite to make a Step appear complete.

Treat unknown, absent, stale, unsupported, and unavailable evidence as distinct. A Gate observes;
an Action mutates. Approval is evidence-bound and is not a pause/resume mechanism.
