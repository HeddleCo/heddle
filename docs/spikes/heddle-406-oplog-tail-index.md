# heddle#406 - oplog tail index and `transaction_id` index

**Status:** spike decision doc. No production code lands in this issue.

**Decision:** bump the local packed oplog format to **v3** and keep it as one
atomic-replaced file, but add an EOF index footer with:

- an entry-id -> entry-offset table for `last()`, `recent(N)`, and tail scans;
- a newest-first batch directory derived from the entry offsets for
  `recent_batches*` without materializing the whole history;
- a sorted `transaction_id` directory for exact-once dedup and committed-batch
  reconstruction.

The implementation should migrate v2 logs lazily by read-old / rewrite-new on
first open under the existing oplog write lock. `set_undone` and `coalesce`
remain full-log rewrite paths; this spike intentionally does not optimize their
random-access mutation shape.

---

## Current facts

The local packed oplog is currently a single binary file with a fixed 28-byte
header followed by variable-length entries. The fixed prefix is
`MAGIC(8) + VERSION(4) + entry_count(8) + head_id(8)`, and `VERSION` is `2`
(`crates/oplog/src/oplog/packed_oplog.rs:15-22`, `:82-87`). `read_head_id`
already proves the cheap seek pattern works: it opens the file, reads exactly
those 28 bytes, validates magic and version, and returns bytes `20..28` as
`head_id` (`packed_oplog.rs:44-73`).

Everything else is eager. `PackedOpLog::load` does `std::fs::read(path)` and
then `parse`, which allocates `Vec::with_capacity(entry_count)` and pushes every
entry (`packed_oplog.rs:39-41`, `:125-235`). `OpLog::load_cached` and
`refresh_cached` both cache a full `PackedOpLog` (`oplog_core.rs:120-137`), so
`last()`, `recent()`, and every fresh cross-process cache refresh are full-file
materializations even when the caller asks for one entry (`oplog_core.rs:153-162`).

`recent_batches*`, `undo_batches*`, and `redo_batches*` also route through the
same cached full vector (`oplog_core.rs:165-185`, `:188-214`,
`packed_oplog.rs:260-318`). The current batch collector intentionally scans the
whole vector before returning so it can merge non-contiguous coalesced batches;
that correctness constraint must be preserved even if the common path becomes
tail-indexed.

Exact-once dedup is also full-history. `record_batch_exactly_once` takes the
oplog write lock, reloads the full packed log, then performs
`packed.entries.iter().any(...)` over every committed entry to find a matching
`TransactionCommit { transaction_id }` (`oplog_core.rs:406-429`). The #392
conditional commit path does the same full-history `find` first, then scans
`packed.entries.iter().filter(|entry| entry.id > since_head_id)` for CAS
conflicts (`oplog_core.rs:445-504`). `committed_batch_records` refreshes the
full cache, finds the commit marker by scanning, then scans the whole vector
again for the marker's batch (`oplog_core.rs:523-570`).

The current v2 entry encoding is variable-length because scope, msgpack
`OpRecord`, actor name/email, and optional operation id all vary per entry
(`packed_oplog.rs:89-119`, `:146-228`). Any tail lookup therefore needs an
offset table; arithmetic from entry count alone is not enough.

Postgres is not the failing local ceiling. The hosted backend already answers
`last()`/`recent()` with `ORDER BY id DESC LIMIT ...`, fetches recent batch ids
with SQL limits, and performs conditional commits inside one SQL transaction
(`crates/oplog/src/oplog/pg_oplog.rs:422-462`, `:282-418`). This spike is about
the local `PackedOpLog` binary format and file-backed backend.

---

## Chosen format

Keep a single `oplog.bin` file and preserve the first 28 header bytes so the
current `head_id` gate stays a fixed-offset read:

```text
header:
  magic:       8 bytes  "LMOPLOG\0"
  version:     u32      3
  entry_count: u64
  head_id:     u64

body:
  entry bytes, exactly the current v2 per-entry encoding

index sections:
  entry offset table
  batch offset lists
  batch directory
  transaction key bytes
  transaction directory

footer:
  fixed-size `LMOPIDX\0` footer with offsets/counts for each index section
```

The entry bytes stay byte-for-byte compatible with v2 entry encoding. The
format bump is for the indexed container, not for a changed `OpEntry` encoding.
That keeps migration mechanical: parse v2 entries once, then serialize the same
entries into v3 plus indexes.

The fixed footer is the index root. Readers locate it by seeking to
`file_len - FOOTER_LEN`, validating `LMOPIDX\0`, then using the section offsets
inside it. The footer should contain at least:

```text
index_magic:            8 bytes  "LMOPIDX\0"
index_version:          u32      1
footer_len:             u32
entry_data_end:         u64
entry_offsets_offset:   u64
entry_offsets_count:    u64
batch_offsets_offset:   u64
batch_offsets_count:    u64
batch_dir_offset:       u64
batch_dir_count:        u64
tx_key_bytes_offset:    u64
tx_key_bytes_len:       u64
tx_dir_offset:          u64
tx_dir_count:           u64
entry_count:            u64      redundant with header
head_id:                u64      redundant with header
```

The redundant footer counts are deliberate. On open, v3 validation should check
that header and footer agree on `entry_count` and `head_id`, all section ranges
are inside the file, and `entry_data_end <= entry_offsets_offset`. A mismatch is
corruption, not an opportunity to guess.

No in-place footer update is required. Every writer still creates a complete
temp file and atomically renames it through the same durability class as
`write_file_atomic` (`packed_oplog.rs:76-79`). The new append writer copies the
old header+entry region bytes to the temp file, appends only the new serialized
entries, writes rebuilt index sections, patches the header count/head, and then
renames. This is still O(file bytes) I/O for append, but it is O(new entries +
index metadata) memory instead of O(all entries) memory. That is the right
first implementation because the existing invariant is atomic whole-file
replacement; in-place header/footer mutation would add a crash-consistency
problem this spike does not need.

---

## Tail index

The entry offset table is fixed-width and sorted by `entry_id` ascending:

```text
EntryOffsetRecord:
  entry_id:     u64
  entry_offset: u64  // absolute byte offset from file start
```

The table is an index over entries, not a cache of entries. `read_entry_at`
seeks to `entry_offset` and parses exactly one v2 entry using the existing
entry parser factored out of `PackedOpLog::parse`. `recent(N)` reads the last
`min(N, entry_count)` records from the offset table, seeks to those entries in
reverse order, and returns the same newest-first order the current
`recent_entries` returns (`packed_oplog.rs:256-257`). `last()` is `recent(1)`
plus `pop`.

`head_id()` remains the fixed 28-byte header read. The tail index does not make
that path faster; it keeps every other "tell me about the tip" path from
falling back to `load()`.

CAS tail scans use the same table in ascending order:

```rust
let start = entry_offsets.lower_bound_by_entry_id(precondition.since_head_id + 1);
for offset_record in entry_offsets[start..] {
    let entry = read_entry_at(offset_record.entry_offset)?;
    let touched = isolation_keys_for_record(&entry.operation, entry.scope.as_deref());
    ...
}
```

This preserves the current #392 rule: only entries with
`id > since_head_id` are considered, and they are considered in id order
(`oplog_core.rs:476-490`). The improvement is that the scan parses only the
tail delta, not entries `1..since_head_id`.

### Batch directory

`recent_batches*` cannot be solved completely by "last N entries" because the
current collector supports coalesced batches whose entries may be non-contiguous
after `coalesce_batches` rewrites batch metadata (`packed_oplog.rs:260-318`,
`oplog_core.rs:237-292`). A pure reverse-entry stream would either stop too
early and miss older entries of a coalesced batch, or keep scanning the whole
log.

Therefore v3 should derive a compact batch directory from the entry offset
table whenever indexes are built:

```text
BatchOffsetList:
  repeated u64 entry_offset values, ordered by batch_index ASC

BatchDirRecord:
  batch_id:              u64  // entry.batch_id, or entry.id when batch_id == 0
  newest_entry_id:       u64
  first_offset_index:    u64  // index into BatchOffsetList
  entry_count:           u32
  scope_state:           u8   // none, one common scope, or mixed
  common_scope_key_off:  u64  // valid only for one common scope
  common_scope_key_len:  u32
```

`BatchDirRecord`s are sorted by `newest_entry_id DESC`, matching the current
"first seen while scanning backward" batch order. To answer
`recent_batches_scoped(count, scope)`, the reader walks this directory from the
front, skips batches whose scope metadata proves they cannot match, reads the
candidate batch's entry offsets, parses only those entries, applies the current
predicate (`all`, undo, redo, user-facing), and stops when `count` batches have
qualified.

The directory is a derived index, so `set_undone` and `coalesce` do not need a
new random-access write algorithm. They can keep loading and rewriting the full
log, then rebuild this directory from the rewritten entries before saving v3.
That keeps the existing coalesced-batch semantics exact while removing full-log
materialization from the hot append/read paths.

### Tail read API

The production API should split "indexed metadata handle" from "materialized
log":

```rust
struct PackedOpLogIndex {
    path: PathBuf,
    header: PackedHeader,
    footer: PackedFooter,
}

impl PackedOpLogIndex {
    fn open(path: &Path) -> Result<Self>;
    fn read_head_id(path: &Path) -> Result<u64>;
    fn last_entry(&self) -> Result<Option<OpEntry>>;
    fn recent_entries(&self, count: usize) -> Result<Vec<OpEntry>>;
    fn recent_batches_scoped(
        &self,
        count: usize,
        predicate: impl Fn(&OpBatch) -> bool,
        scope: Option<&str>,
    ) -> Result<Vec<OpBatch>>;
    fn entries_after(&self, since_head_id: u64) -> Result<impl Iterator<Item = Result<OpEntry>>>;
}
```

`PackedOpLog::load` should become an explicit full-materialization helper for
the few rewrite paths that still need it, not the default implementation of
every read.

---

## `transaction_id` index

The transaction directory is a sorted fixed-width table plus a key-byte blob.
It indexes only `OpRecord::TransactionCommit { transaction_id, .. }` entries
(`oplog_types.rs:136-143`).

```text
tx_key_bytes:
  concatenated UTF-8 transaction ids

TxDirRecord, sorted by transaction_id bytes:
  key_offset:       u64  // relative to tx_key_bytes_offset
  key_len:          u32
  commit_entry_id:  u64
  batch_id:         u64
```

A lookup binary-searches `TxDirRecord`s by comparing the searched
`transaction_id` bytes with the key bytes referenced by the candidate record.
The key bytes are stored, not just a hash, so the format has no collision
semantics. An implementation may add a stable hash prefix later for fewer key
reads, but the canonical comparison must remain the full transaction-id bytes.

If migration finds duplicate transaction ids in an old v2 log, the index should
store the first commit marker by `commit_entry_id` and optionally emit a warning.
That matches today's `iter().find(...)` / `iter().any(...)` ascending-history
behavior and avoids inventing a new repair policy during migration. New v3
writes prevent additional duplicates because lookup and append stay serialized
under the existing oplog write lock.

Exact-once append becomes:

1. Acquire the existing oplog write lock.
2. Open and validate the v3 footer/index.
3. Lookup `transaction_id` in `TxDirRecord`.
4. If present, return a dedup hit without scanning history.
5. Otherwise append the new batch and rebuild the index sections atomically.

`committed_batch_records(transaction_id)` uses the same index hit. It reads the
hit's `batch_id`, fetches that batch from the batch directory, filters out
`TransactionCommit`, and returns the original committed records. This preserves
the #354 cross-process dedup behavior currently guarded by
`refresh_cached()` (`oplog_core.rs:535-541`) without refreshing a full
`Vec<OpEntry>`.

The #392 conditional path keeps its load-bearing order:

1. `transaction_id` lookup first.
2. If found, read the hit batch and return
   `ConditionalCommitOutcome::AlreadyCommitted(records)`.
3. Only if absent, compare `head_id` to `precondition.since_head_id`.
4. If the head changed and keys are non-empty, use the entry offset table to
   parse entries with `id > since_head_id` in ascending order and classify them
   with `isolation_keys_for_record`.
5. If no conflict is found, append and atomically publish the updated indexes.

Dedup-before-CAS remains non-negotiable. A crash retry of an already committed
logical transaction must reconstruct the original committed batch even if
another transaction advanced the same isolation key later. Reversing the order
would turn a successful retry into a false conflict; the current #382 design
already calls this out, and the indexed path must preserve it.

---

## Write path and cache shape

`OpLog::load_cached` currently caches `Option<PackedOpLog>`, which implies a
full entry vector (`oplog_core.rs:120-137`). v3 should cache only the indexed
metadata needed for cheap reads:

```rust
enum OplogCache {
    Indexed(PackedOpLogIndex),
    Empty { path: PathBuf },
}
```

`refresh_cached` should reopen the footer and header, not parse entries. That
keeps the #354 "observe cross-process commits" behavior while removing the
unbounded memory ceiling.

Append should be implemented as an indexed copy-forward writer:

1. Under the write lock, open the current v3 index or create an empty v3 file
   model.
2. Perform dedup and CAS checks from indexes/tail offsets.
3. Serialize the new entries into a temp file after the copied old entry region.
4. Merge old entry-offset records with new offsets.
5. Merge old batch-directory records with the new batch. Normal appends add a
   new newest batch; full-rewrite paths rebuild instead.
6. Merge old transaction-directory records with any new
   `TransactionCommit` marker. The merge is sorted by transaction-id bytes.
7. Write index sections and footer.
8. Patch header `entry_count` and `head_id`.
9. Atomic rename over the old file, then update the in-process cache to the new
   indexed metadata.

This path deliberately does not promise O(1) append I/O. It promises bounded
memory and bounded parse work. That is the ceiling the issue targets.

---

## Migration and versioning

Bump `VERSION` from `2` to `3`. v3 readers accept:

- v3: validate header + footer and proceed;
- v2: migrate under the oplog write lock, then reopen as v3;
- v1 or unknown future versions: fail loudly.

The v2 migration is lazy read-old / rewrite-new. It parses the v2 file once
using the existing full parser, builds all v3 index sections, and atomically
renames the v3 file into place. This is a one-time O(history) memory event for
existing repositories only. New v3 operations should never materialize the full
history except for the scoped-out rewrite paths.

Although Heddle is pre-1.0 and the general compatibility guidance favors the
current model over shims, the oplog is durable user data. Rejecting every v2
repository after this bump would be a poor failure mode when the entry encoding
has not changed and the old file can be rewritten deterministically. The right
stance is: support v2 only as a migration source, not as a long-term dual-read
format. After successful migration, all writes produce v3. Existing v1 files
are already rejected in the current tree because v2 added actor and
operation-id fields with no live migration path (`packed_oplog.rs:16-22`); this
spike does not resurrect v1.

`read_head_id` must not silently treat a v2 file as current just because the
first 28 bytes have the same shape. For logical readers, the implementation
should ensure migration before the reconciler generation gate can return a v2
head. That can be done by an `ensure_current_format()` call during `OpLog`
construction/open and by making `read_head_id` either see v3 or trigger the same
locked migration helper. If migration fails because the file is corrupt or not
writable, fail loudly; do not report generation `0` or skip reconciliation.

Crash behavior is the existing whole-file rule:

- crash before rename: the old v2 or v3 file remains authoritative;
- crash after rename: the new v3 file has matching header, indexes, and footer;
- partial temp files are ignored.

---

## Invariant interactions

### #354 reconcile staleness

#354 depends on two properties visible in current code:

- `generation()` calls `oplog().head_id()` and must fail loudly on corrupt or
  unsupported headers (`crates/repo/src/atomic/reconciler.rs:344-350`);
- when a class watermark lags, reconciliation folds committed entries newer
  than the watermark (`reconciler.rs:353-364`).

v3 preserves the first property by keeping `head_id` in the fixed header and
keeping version validation in the fast path. It improves the second property by
letting reconciliation read entries after the watermark from the entry-offset
table instead of asking `recent_batches_scoped(usize::MAX, scope)` to
materialize all batches first (`reconciler.rs:360-363`).

The reconciler still may need to fold a large tail after a long outage or first
open. That is acceptable. The issue is not that a true lagged catch-up is
proportional to the lag; the issue is that no-lag and tiny-tail reads currently
pay for the entire repository age.

The class-watermark rule remains unchanged. Advancing a watermark still happens
only after the relevant class has materialized the committed tail. The index is
an access path over committed entries; it is not a second source of truth and
must not advance watermarks by itself.

### #392 CAS commit point

#392 makes the oplog append the conditional commit point. The current local
path holds the oplog write lock, reloads the log, dedups by `transaction_id`,
then scans `id > since_head_id` for isolation conflicts before appending
(`oplog_core.rs:456-504`).

The indexed design keeps the same lock and the same order. The only change is
which access paths are used under that lock:

- dedup: `TxDirRecord` lookup instead of full `entries.iter().find`;
- conflict scan: entry-offset lower_bound at `since_head_id + 1` instead of
  filtering the whole vector;
- append: atomic v3 rewrite that publishes entries and indexes together.

The index must be updated in the same atomic file replacement as the appended
entries. A file with a new entry but stale transaction directory would break
exact-once; a file with a transaction directory pointing past `entry_data_end`
would be corrupt. Footer validation should reject both, and the writer should
make neither observable by using temp-file replacement.

Because the write lock covers lookup, CAS tail scan, and replacement, two
concurrent retriers still serialize. The first writer to append also publishes
the new transaction-id directory entry; the second writer opens the new footer
under the lock and returns a dedup hit.

---

## Load-bearing boundary kept

Do not optimize `set_undone` or `coalesce` in the implementation issue.

`set_undone` mutates every entry in a target batch and currently walks the full
entry vector (`packed_oplog.rs:244-250`, `oplog_core.rs:608-612`). `coalesce`
finds entries from two existing batches, rewrites their `batch_id` and
`batch_index`, and saves the whole packed log (`oplog_core.rs:237-292`). Those
are genuine random-access rewrite operations, not tail append/read operations.

For v3 they should call an explicit `load_all_for_rewrite`, perform the current
mutation, and save through `save_v3_rebuild_indexes`. That keeps correctness
simple and makes their cost visible. If undo/redo rewrite cost becomes a
separate ceiling, design a targeted batch-id index or mutation log in a later
issue.

---

## Implementation notes and tests

The implementation issue should land tests that prove the access path, not just
the returned shape:

- `recent(1)` and `last()` on a large log do not call the full parser.
- `committed_batch_records` finds an old transaction id through the index after
  a fresh cross-process handle opens.
- `record_batch_exactly_once` dedups a transaction far older than any recent
  window without scanning entries.
- `record_batch_exactly_once_if_unchanged` returns `AlreadyCommitted` before
  considering CAS conflicts when both are possible.
- CAS conflict detection scans only entries with `id > since_head_id` and still
  reports the earliest conflicting entry id.
- `recent_batches_scoped` preserves coalesced non-contiguous batch semantics
  using the batch directory.
- corrupt footer/header disagreement fails loudly.
- v2 migration rewrites to v3 and preserves entries, head id, scopes, actors,
  operation ids, batch ordering, and transaction lookup behavior.
- `set_undone` and `coalesce` still work after v3 migration and rebuild indexes
  that subsequent tail reads use.

Instrumentation is worth adding while this lands: count full materializations,
entry parses, and index lookups behind `debug` logs or test-only counters. The
main regression risk is accidentally reintroducing a full `PackedOpLog::load`
through `load_cached`.

---

## Proposed impl issue

**Title:** Implement packed oplog v3 tail and `transaction_id` indexes

**Scope:** Implement the v3 packed oplog container, lazy v2 -> v3 migration,
entry-offset tail reads, batch directory reads for `recent_batches*`, sorted
`transaction_id` lookup, indexed exact-once/conditional commit paths, indexed
`committed_batch_records`, cache reshaping away from `Vec<OpEntry>` on hot
reads, and the tests listed above. Keep `set_undone` and `coalesce` as explicit
full-log rewrite paths that rebuild indexes.

**Blocked by #406.**

**Estimate:** xhigh. This touches binary format stability, migration of durable
user data, exact-once transaction semantics, and the #354/#392 atomic
invariants.
