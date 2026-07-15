# Semantic anchor convergence

Discussion and context anchors should preserve semantic intent, not only a resolved file and line snapshot. Resolved locations are display and lookup aids; durable anchors need enough target information to retarget across source changes and to surface ambiguity when Heddle cannot resolve the target confidently.

A semantic anchor should cite the source state or version where the reference was created and the semantic selector used to follow it forward. Current anchor resolution is derived from those facts and repository history; it is not the durable anchor identity by itself.

OSS CLI anchor resolution should be deterministic from Heddle repository data and should not depend on external hosted services or live language-server state. Richer language-aware resolvers can be added later as optional local extensions or hosted projections without changing durable anchor identity.

Anchor resolver confidence should be deterministic and visible. High-confidence movement can materialize as current or moved, while lower-confidence matches become ambiguous or orphaned attention. Heddle should not silently mutate durable anchors to a guessed target.

Anchor resolver thresholds should not be per-user configuration in v1. Materialized anchor status should be stable for a repository version so humans, agents, tests, and hosted projections agree; user preferences may affect display density, not the semantic status.

Concurrent anchor retarget operations survive when the record type can represent multiple plausible anchors; the materialized view shows anchor ambiguity rather than silently picking a winner. A conflict state appears only when the record requires a single canonical anchor and concurrent retargets are incompatible.

Anchor ambiguity creates an attention item when it affects actionability, such as review comments, blockers, or context annotations tied to changed code. Purely informational ambiguity remains visible on the record without blocking readiness.

**Status:** accepted

**Considered Options:** Last-writer-wins anchor retargeting would keep views simple, but it would hide ambiguity and make concurrent agent work less trustworthy. Treating every concurrent retarget as a conflict would be noisy when multiple anchors can legitimately describe the same discussion.
