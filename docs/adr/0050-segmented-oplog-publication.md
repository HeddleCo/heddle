---
status: accepted
---

# Publish the oplog as bounded immutable segments

Heddle stores the current local oplog as a V5 layout: an atomically replaced
`oplog.manifest` names one immutable packed base plus an ordered list of
immutable packed append segments. Each segment descriptor carries a binary
merge level. Each container continues to use the V4
`LMOPLOG\0` encoding and StateId-native record schema 4. The manifest is the
commit point and carries a monotonic generation, total entry count, and head id.

A normal append writes, fsyncs, and validates only its new segment, then durably
swaps the manifest. Reconstructible snapshot-view publication keeps the same
ordering: the snapshot artifact remains authoritative, but a durable manifest
must never select a segment whose directory entry or bytes did not survive.
Unlisted files are crash debris, not committed history. Readers reject unsafe,
duplicate, reordered, overlapping, missing, or metadata-inconsistent container
references instead of falling back to a stale base.

Each append starts as level 0 and repeatedly merges only an equal-level tail,
like carry propagation in a binary counter. Levels are strictly descending in
manifest order, so at most 64 append containers can be selected for a `u64`
append generation. A record participates in O(log N) rewrites across N appends;
normal append never rewrites the full base. Only after the manifest swap may
unselected generations be removed, and cleanup failure does not turn a
committed operation into an append failure. A cold open still fully validates
the selected base and segments and is O(total oplog bytes); segmentation does
not claim to remove that integrity cost.

Existing repository-format-v3 `oplog.bin` files are accepted as the initial V4
base. The first append publishes a V5 manifest; there is no dual write and no
persistent compatibility mirror. V2/V3 containers and record schemas 1–3 remain
refused without mutation.

## Consequences

- `oplog.manifest` is authoritative whenever it exists; the canonical
  `oplog.bin` may be an older immutable base after compaction.

- Explicit recovery never unlinks a pruned, now-non-contiguous suffix as its
  only copy. Before publishing the repaired manifest it preserves every removed
  container under `oplog/oplog.quarantine/`; ordinary generation sweeping does
  not enter that namespace.
- Explicit recovery operates on every manifest-selected container and then
  reconciles manifest count/head metadata with the recovered generation.
- Undo, redo, and batch coalescing publish a new uniquely named compacted base
  rather than mutating a selected generation in place.
- Orphan collection is post-commit maintenance and can be retried independently.

## Considered options

Reflinking and rewriting the V4 tail avoids copying historical entry bytes on
CoW filesystems, but still republishes every cumulative index and falls back to
a full copy elsewhere. Mutating a single file in place would make the fixed
header and EOF footer separate crash commit points. An unbounded level-0 segment
chain would make validation and cold open grow with append count. Fixed-fanout
full compaction still rewrites the entire history periodically and remains
O(N²). Binary tiers keep one atomic authority, bound container fanout, and
reduce cumulative rewrite work to O(N log N).
