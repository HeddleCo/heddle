# Versioned collaboration operation encoding

Collaboration operations use a typed Heddle in-memory model and a canonical MessagePack local encoding. Each durable operation envelope carries an explicit collaboration operation schema version from v1. JSON, protobuf, or gRPC message forms are adapters for user interfaces and transport; they are not the durable local source of truth.

The collaboration codec should reuse the packed oplog's schema-versioning pattern: one latest encoder, explicit version dispatch, frozen decode-only historical schema snapshots, and no blind unversioned decode path for newly written data. The canonical operation envelope should also carry an explicit operation kind rather than relying on Rust enum declaration order or rmp-serde enum discriminant indexes for long-lived identity.

Unlike packed oplog migration, collaboration operation migration must not rewrite old durable operation bytes in place. `CollabOpId` is content-addressed over the canonical envelope, so rewriting bytes would change identity. Historical operation schemas are decoded into the current in-memory model for materialized views, indexes, display, and sync validation. Future compaction must either preserve each operation's original envelope bytes or explicitly create new operation IDs with a recorded migration relationship.

A v1 operation and a v2 operation that decode to the same current in-memory operation still have distinct operation IDs when their canonical envelope bytes differ. Semantic or retry deduplication belongs in explicit idempotency fields and indexes, not in cross-version hash normalization. The idempotency key is part of the operation envelope so local retry handling and Weft validation can reason about the same command-attempt identity.

Schema tests should pin version numbers, round-trip the latest schema, map each historical schema into the current model, reject unsupported versions, prove legacy shapes do not decode as current by accident, and maintain golden canonical byte/hash fixtures for representative operation kinds.

**Status:** proposed

**Considered Options:** Reusing the oplog implementation directly would bring a proven pattern, but its rewrite-to-latest migration behavior conflicts with content-addressed collaboration operation identity. Storing only current Rust serde enum bytes would be simpler, but it would make enum reordering, field reshaping, and accidental defaulting part of the durable compatibility contract. JSON would be easier to inspect manually, but canonical MessagePack keeps the local store compact and closer to existing Heddle binary storage while still allowing JSON as an adapter.
