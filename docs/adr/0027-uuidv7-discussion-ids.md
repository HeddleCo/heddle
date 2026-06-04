# UUIDv7 discussion IDs

Discussion IDs are generated UUIDv7 record identifiers, while collaboration operation IDs are content-addressed. UUIDv7 gives discussions stable locally generated identity with useful time-sort behavior without tying the discussion's identity to its opening title, anchor, first turn, or attribution. UUIDv7 order is only an indexing and display convenience; causal ordering remains the semantic ordering model.

Migrated legacy state-attached discussions receive new UUIDv7 discussion IDs. Legacy IDs are preserved as source metadata or lookup aliases when useful, but they do not become canonical repository discussion identity.

**Status:** accepted

**Considered Options:** Content-addressing the opening operation could derive the discussion ID from immutable data, but it would make record identity depend on opening payload details. Random UUIDs would work, but UUIDv7 gives better ordering and locality for listings, indexes, and sync cursors while keeping the ID opaque.
