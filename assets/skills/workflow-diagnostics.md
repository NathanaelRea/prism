# Diagnose Workflow Runs and Effects

Stay on Prism's read/control interfaces rather than repairing persistence by hand.

1. Inspect with `prism workflow history <run-id> --json`, `prism debug info`, and
   `prism debug integrity`.
2. For extension failures, run `prism extension doctor <id> <pinned-executable> --json`; distinguish
   malformed protocol output, stdout contamination, timeout, crash, and cancellation failure.
3. Compare Artifact schema IDs, revisions, digests, provenance, and exact Attempt bindings. A retry
   must use the same input revisions; successor work is a new child iteration.
4. For uncertain brokered effects, inspect the persisted intent, exact preconditions, fencing token,
   dispatch evidence, and reconciliation result before requesting recovery.
5. Use workflow pause/resume/cancel/retry/approve/reject commands. Never mark Steps, Approvals,
   Artifacts, or effects complete with SQL and never rewrite package locks to change a retained run.

Unbrokered full-trust extension effects cannot be assumed fenced or reconcilable. Escalate them with
that limitation stated explicitly.
