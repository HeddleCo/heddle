# Device account API v2

The device endpoint composes the locally admitted account directly. Workspace,
Identity, ownership and Thread-list requests do not select a synthetic Spool and
do not contact Weft. Request PoP and the actual Biscuit operation/resource checks
run before a retained-stream permit is acquired. Registered mint roots, accepted
owner history, publisher revocations and ancestor restrictions remain effective.

The local catalog is `state/device-rpc/catalog.sqlite3`. It contains stable Spool
registrations, settings, child/mount topology, private bookmark tombstones and
exact mutation receipts. One transaction commits the mutation and its response;
reusing an operation ID with different request bytes is rejected. Authorization
of the requested resource precedes receipt lookup. Tombstoned identities preserve
that authorization path for historical retries without reviving their data.
Creation reserves its UUID/name within the catalog transaction, initializes its
private repository, commits, then idempotently seeds the native default Thread.
A retry repairs the seed step; a retry after deletion does not resurrect a Spool.

Every catalog commit publishes an atomic generation marker. Account observers
share an OS watcher and persistent read connection; accepted Thread commits have
their own marker. Idle subscriptions execute local time-caveat checks over the
already verified Biscuit without storage polling. A view binds current physical
registrations before publishing data. Catalog and per-repository generation
fences prevent committing mixed snapshots; lost continuity requires a reset.

Workspace composes Spools, bounded local Thread lists, the current device and
private bookmarks. Exact Spool and Thread views include the bookmark's own CAS
version. Spool and Thread mutation hints use the actual caller's scoped
permissions and the relevant observed version. Hosted-only administration and
billing sections report unavailable on the device.

Thread listing reads summaries maintained once at native admission. Concurrent
name/intent/lifecycle candidates fall back to immutable genesis/default values
in the list, while exact views retain every conflict candidate. Accepted source
counts and heads deduplicate State revisions. Name and updated-order pages seek
through covering indexes, and filter processing advances a bounded candidate
cursor even when a window contains no matching records. A composed Thread query
selects at most 64 local Spools and examines at most 4,096 candidate summaries plus one lookahead per Spool;
individual pages and encoded bytes have separate caps. Readiness remains Unknown
without hosted policy material. Attention/participant filters explicitly require
a future local facet index rather than guessing from incomplete history.
