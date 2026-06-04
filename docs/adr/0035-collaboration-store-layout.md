# Collaboration store layout

The local collaboration store is rooted at `.heddle/collaboration/`. Durable collaboration operations live under `ops/`, while `indexes/`, `views/`, `sync/`, and `tmp/` hold rebuildable query structures, materialized views, Weft sync metadata, and temporary files.

V1 stores each collaboration operation as an individual content-addressed file under prefix-sharded `ops/` directories. Operation files contain immutable versioned operation envelopes; indexes and views may be rebuilt or normalized, but durable operation bytes are not rewritten in place. Append segments or pack compaction can be introduced later if scale requires it without changing the logical operation model or operation IDs.

V1 should not introduce append segments or pack compaction beyond the individual operation-file layout. Individual files are easier to audit while collaboration convergence, `fsck`, and repair invariants settle. Packing can follow after the formal convergence model and store invariants are stable.

Collaboration operation writes use a temp file, atomic rename, and directory sync before updating indexes or views. Index rebuild must tolerate durable operation files that exist without matching index entries.

Index and view updates are not transactionally coupled to operation writes. The operation file is the durable commit point; indexes and views are derived, may lag, and can be rebuilt after crashes.

Concurrent local writers use a repository collaboration lock around idempotency checks, operation-file creation, and minimal sync/idempotency metadata updates. Derived index and view rebuild work should stay outside the critical path when safe.

Routine collaboration `fsck` should recompute operation IDs from canonical bytes, decode supported schema versions, verify primary-record and parent references, verify idempotency index consistency, distinguish unresolved parents from malformed bytes, and prove indexes and views are rebuildable. It should not require Weft access or historical token material.

Collaboration `doctor` may rebuild derived indexes and views, recreate missing idempotency metadata from durable operations when unambiguous, and quarantine malformed artifacts for diagnostics. It must not invent replacement operation bytes, silently delete locally valid operations, or rewrite content-addressed operation envelopes.

**Status:** proposed

**Considered Options:** Storing collaboration operations under `.heddle/objects/` would align them with content addressing, but it would blur source-history objects with collaboration metadata. Storing them under `.heddle/oplog/` would reuse an append log, but it would confuse collaboration history with undo/redo history. A dedicated root keeps the boundary clear while staying Heddle-native.
