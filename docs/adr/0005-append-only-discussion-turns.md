# Append-only discussion turns

Discussion turns are append-only in the OSS collaboration log; corrections are represented as later turns. This preserves auditability for human and agent coordination, keeps local-first merge semantics simple, and avoids hiding what an agent or person said behind mutable message edits.

The v1 turn-kind set should be small and closed for machine behavior: `comment`, `question`, `answer`, `blocker`, `decision`, `handoff`, and `status`. New behavior-bearing kinds should arrive through schema evolution rather than arbitrary user-defined strings. Free-form labels can be added later without weakening turn kind as reliable agent signal.

Discussion turns may include optional structured references to earlier collaboration operations they answer, supersede, hand off from, or otherwise respond to. Inbox and readiness behavior should prefer those references over body-text heuristics; the turn body explains the response but should not be the durable workflow edge.

Accidentally sensitive content should not introduce general edit or delete semantics for discussion turns. Heddle should model suppression as a distinct collaboration redaction operation or hosted policy action, preserving audit metadata while hiding redacted content from normal views or sync according to policy.

Redaction suppresses selected content; it does not delete causal graph structure. Descendant turns remain part of the discussion when policy allows them, and references to redacted operations render as redacted references rather than disappearing.

Discussion turn materialization uses causal parent operation edges as the primary ordering signal. Timestamp and UUIDv7 order are deterministic display tie-breakers for concurrent turns, not substitutes for causality.

Only the operation that opens a discussion record may omit causal parents by default. Imported or orphaned history may also omit parents when it records an import reason. Normal append operations cite the current observed record head set so convergence can distinguish concurrent branches.

Concurrent discussion turns do not create a conflict state. They survive as sibling turns and are deterministically ordered for display. Conflict states are reserved for operations whose claims are mutually incompatible, such as competing resolutions or incompatible visibility or anchor changes.

**Status:** proposed

**Considered Options:** Editable message bodies would match common chat tools, but they require additional CRDT text or last-write-wins edit semantics and weaken provenance. Hosted products may later add narrow presentation affordances, but the durable OSS record remains append-only.
