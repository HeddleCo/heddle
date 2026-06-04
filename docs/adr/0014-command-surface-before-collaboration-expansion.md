# Command surface before collaboration expansion

Heddle should deepen the command surface before broadly expanding collaboration commands such as `inbox`, richer `discuss`, collaboration sync, and capability-aware filtering. The current command metadata, dispatch, schema, runtime-contract, and help surfaces are too scattered; collaboration features should land only after command facts can stay local and schema/docs gates can cover the new surface reliably.

Every agent-facing collaboration JSON response should include an explicit schema identifier and version. This is required before `discuss` evolves in place, because machine consumers need to distinguish the repository collaboration contract from older state-attached discussion semantics.

The command catalog should mark collaboration writes as mutating operations and declare whether they support a collaboration idempotency key. Agents need command metadata that exposes side effects and retry semantics before they can safely automate discussion, inbox, and future collaboration workflows.

Collaboration JSON errors should carry a stable machine-readable code, a human message, the command schema identifier/version, and actionable recovery fields. Stale-head errors return current heads, ambiguous-id errors return candidates, and hosted rejection errors include the hosted rejection code plus retry or hosted-valid-continuation hints when available.

**Status:** proposed

**Considered Options:** Shipping collaboration commands first would move product direction faster, but it would multiply drift across help, JSON schemas, runtime contracts, command catalog entries, and docs. Tightening the command surface first gives the collaboration expansion a stable CLI API foundation.
