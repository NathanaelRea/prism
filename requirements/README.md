# Prism Requirements

This directory is the concise contract for current Prism product behavior. It
describes what users can observe and what implementations must preserve; it does
not prescribe tickets, migration phases, internal helper structure, or rejected
ideas.

`CONTEXT.md` defines the domain language. Accepted decisions under `docs/adr/`
remain binding technology constraints. If wording conflicts, a more specific
requirement here controls behavior while an ADR controls its stated architecture
boundary.

Requirement bullets use these labels:

- **Behavior**: externally observable behavior.
- **Invariant**: a condition that must remain true across operations or restarts.
- **Quality**: a measurable or verifiable quality attribute.
- **Constraint**: a required technology or architecture boundary.
- **Default**: behavior used until the user chooses otherwise.
- **Customization**: supported user configuration.
