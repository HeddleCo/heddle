# Formal collaboration convergence before sync

Heddle should have a formal convergence model before collaboration sync ships. The local-only discussion-log slice can begin with Rust tests, but remote CRDT collaboration requires a model covering concurrent append survival, incompatible resolution conflicts, reopen semantics, idempotent replay, conflict resolution, and deterministic materialized views.

The model should cover both semantic materialization and deterministic display linearization. Replicas with the same accepted operation set must produce the same record head sets and the same display order, regardless of operation arrival order.

The model should also cover recursive conflict materialization: concurrent conflict-resolution operations can create a new explicit conflict when they choose incompatible outcomes.

The model should distinguish command idempotency from operation identity. Command/RPC idempotency keys prevent duplicate execution, while content-addressed `CollabOpId`s deduplicate identical operation bytes during sync or import.

Hosted rejection and blocking states should be modeled as a sync-policy layer over local convergence. The local CRDT model converges over locally valid operations; the hosted model converges over accepted subsets, rejected operations, blocked descendants, and hosted-valid continuations.

The first formal convergence model treats signature validity as an operation predicate or policy filter rather than modeling cryptographic details. The first formal risk is convergence and materialization, not cryptographic correctness.

The first model treats capability scope as abstract permissions or predicates over operation kinds and records. Full Biscuit semantics belong in auth-specific tests or specs; collaboration convergence needs policy accept/reject behavior.

The formal model should use Quint if that remains Heddle's formal-spec convention. Collaboration should fit the existing verification practice rather than introduce a second modeling tool.

**Status:** accepted

**Considered Options:** Relying only on implementation tests would move faster, but it would make the CRDT claim less defensible. Requiring a full formal model before any local discussion work would delay learning about command and storage ergonomics unnecessarily.
