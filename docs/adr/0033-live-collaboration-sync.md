# Live collaboration sync

Synced discussions can receive live gRPC-backed updates while a Weft connection is active. Live collaboration sync updates the durable repository collaboration log and materialized views; it does not replace operation-log storage, push/pull catch-up, offline reconciliation, or capability policy.

Live sync starts as explicit foreground/watch command behavior, such as inbox or discussion watch modes. Daemon integration can come later once lifecycle, status, and recovery are clear; Heddle should not start invisible background collaboration sync.

Watch modes should not ship in the first local-only discussion slice. They depend on Weft-backed live sync; local durable async coordination should ship without presenting itself as a live collaboration surface.

Agent communication correctness relies on durable collaboration operations, not live sync. Watch modes reduce latency for already-shared discussions, but offline writes, push/pull catch-up, and operation-log reconciliation remain the source of truth.

Live updates merge through the same operation-log convergence path as push/pull. Remote operations are imported, local unsynced operations remain, materialized views recompute, ordinary concurrent appends coexist, and incompatible resolutions become resolution conflicts.

Presence, typing indicators, and similar live UI affordances are not collaboration records. They may become ephemeral hosted UI data later, but Heddle live sync should deliver durable operations and sync state rather than chat transport state.

**Status:** proposed

**Considered Options:** Requiring manual push/pull for every discussion turn would preserve a simple sync model, but it would make hosted Heddle feel less collaborative than the product intends. Treating live updates as a separate chat transport would fragment the model, so live delivery remains a transport over the same collaboration operations.
