# UUIDv7 discussion IDs

Discussion IDs are generated UUIDv7 record identifiers, while collaboration operation IDs are content-addressed. UUIDv7 gives discussions stable locally generated identity with useful time-sort behavior without tying the discussion's identity to its opening title, anchor, first turn, or attribution. UUIDv7 order is only an indexing and display convenience; causal ordering remains the semantic ordering model.

Migrated legacy state-attached discussions receive new UUIDv7 discussion IDs. Legacy IDs are preserved as source metadata or lookup aliases when useful, but they do not become canonical repository discussion identity.

The client mints this UUIDv7 identity and carries it to the hosted service on open; the hosted service records the client-minted id, it does not mint its own. A discussion therefore has one identity that is stable across every clone — the id the originator printed is the id every other machine can address. (Before this, the hosted read path re-minted a fresh UUIDv7 per clone, so no clone could address the originator's id.) Legacy hosted discussions opened before clients carried their id have no `disc-<UUIDv7>` on the wire; those get a deterministic id derived from the hosted id, so clones still agree with each other.

**Status:** accepted

**Considered Options:** Content-addressing the opening operation could derive the discussion ID from immutable data, but it would make record identity depend on opening payload details. Random UUIDs would work, but UUIDv7 gives better ordering and locality for listings, indexes, and sync cursors while keeping the ID opaque.
