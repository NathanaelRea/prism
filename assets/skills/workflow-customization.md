# Customize a Standard Workflow

Copy a Standard Pack Workflow Definition to a new qualified identity before changing it.
Validate and preview the complete dependency closure after every edit. Gates, prompts, Step
Implementations, dependencies, and optional security-review Steps are definition-owned; do not
edit Prism's Run Ledger or package lock by hand.

Useful checks:

- reorder Steps with explicit `depends_on` edges;
- replace `use` or a packaged prompt template;
- remove a Step only when all bindings and terminal outcomes remain reachable;
- add a Gate such as a security review before the exact-head Approval;
- verify brokered versus unbrokered disclosures in preview before launching.
