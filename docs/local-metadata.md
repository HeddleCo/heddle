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
already moved. Actor-presence file scans still require conversion. Command receipts now use
indexed SQLite storage, with an explicit separate bootstrap database for commands
that run before a repository exists. Completed receipts have bounded seven-day
cleanup; pending reservations require explicit cancellation. Rebuildable shared
reference roots and typed-property equality indexes also live here; capture
publication, richer predicates and FTS integration remain pending. Cross-subsystem operations must explicitly share a
transaction; merely sharing the database filename does not combine separate
method calls into an atomic operation.

Verification covers rollback/commit across Thread state, Run state and a command
receipt; restart continuity; readers during a writer transaction; bounded change
history; expired-cursor rejection; and future-schema rejection. Missing-event,
missing-window-bound and missing-cursor-floor controls fail independently. The
existing Thread, Run and artifact repository suites also run against this schema.

The daemon owns artifact expiration independently of open browser streams. It
watches only metadata directory entries, sleeps until the indexed next expiry,
and deletes bounded batches. Removal from discovery does not cancel a retained
artifact's TTL. Shutdown cancels the workers; startup resumes overdue cleanup.
Watcher continuity failures and failed/contended cleanup retry with a delay.

Run/Checkout observations share a committed-change gate. Filesystem changes still
wake them independently. Exact permission/artifact deadlines trigger projection
updates; a device-owned CPU-only clock rechecks arbitrary Biscuit time caveats
without SQL or projection reads on valid ticks. That fallback is necessary
because an ancestor check can expire before the root's explicit expiry fact.

Source anchors can now sign an explicit shared target binding: follow the viewed
Thread, follow a named Thread, or stay pinned to an exact revision. The original
coordinates remain signed evidence. An absent target means exact-location only.
Rust and browser encoders share independent canonical/signature vectors. Binding
these roots to signed captures and including their object closure in transfer
remain required before the capture-driven automatic update path is complete.
