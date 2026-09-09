# Local metadata storage

Heddle's v2 mutable Thread and Run state shares `metadata.sqlite3` under the
resolved object-store `.heddle` directory. Separate native checkouts resolve that
same directory while keeping their working trees, HEADs and writer leases local.
Immutable source objects, packs and retained artifact bodies remain files.
Device-wide account registration and replay nonces retain their separate stores.

The core SQLite schema contains original signed Thread operations, causal and
query indexes, sharing policy, Runs, timeline records, command receipts, controls,
permission decisions and immutable artifact catalog records. Initialization is
one transaction, with `user_version` checked before use. The alpha cutover has no
fallback reader for the previous separate Thread and Run database filenames.

WAL and FULL synchronous durability are enabled. Database creation uses owner-only
permissions. Existing-schema opens do not acquire a write transaction. A reader
may inspect committed state while another connection owns the single writer;
network writes, tool execution and filesystem capture work must finish outside
that transaction. Database transactions do not make working-tree edits atomic.

`metadata_changes` is a bounded wake/resume projection. Triggers write changes in
the same transaction as the underlying mutation. Rolled-back work produces no
committed change. Its 4096-entry window reports an expired cursor when a consumer
must reload authoritative state; truncating it never deletes authored records.
OS notification files are wake hints, not durability or authorization evidence.
The existing device observers now watch the common database marker. Their v2
section cursors remain distinct from the local database change cursor.

This is the common persistence foundation, not a claim that every local store has
already moved. Actor-presence file scans and older serialized operation-dedup
maps still require conversion. Typed annotation/reference projections and FTS
indexes belong here. Cross-subsystem operations must explicitly share a
transaction; merely sharing the database filename does not combine separate
method calls into an atomic operation.

Verification covers rollback/commit across Thread state, Run state and a command
receipt; restart continuity; readers during a writer transaction; bounded change
history; expired-cursor rejection; and future-schema rejection. Missing-event,
missing-window-bound and missing-cursor-floor controls fail independently. The
existing Thread, Run and artifact repository suites also run against this schema.
