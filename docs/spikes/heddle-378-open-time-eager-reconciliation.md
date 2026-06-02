# heddle#378 - open-time eager reconciliation

**Status:** spike decision doc. No production code lands in this issue.

**Decision:** do not add an open-time eager reconciliation pass now. The current
per-read and pre-write reconciliation paths are the correctness mechanisms, and
the persisted reconcile watermark already gives fresh handles cross-process
crash recovery without making every `Repository::open` fold the oplog tail.

---

## Current facts

`Repository::open_raw` wires an `OplogRefReconciler` into `RefManager` and then
calls `init_reconcile_watermark` (`crates/repo/src/repository.rs`). `Repository::init`
does the same, so normal local handles use the same read and write chokepoints
from creation onward.

`RefManager::with_reconciler` seeds the in-memory local and shared generation
watermarks to the current oplog `head_id`. `init_reconcile_watermark` then
replaces those seeds with the persisted last-clean local/shared watermarks when
they exist, or persists the current-generation seed for new repositories. This
means a fresh handle does not blindly re-fold ancient history, but it can still
fold the crash tail newer than the last-clean point on its next read.

Every public ref read funnels through `reconciled_load`. The hot path is one
`generation()` header read plus a class-watermark compare. When a class lags,
the read takes the refs lock, refreshes the persisted watermark, folds the
committed tail for that class, materializes every ref touched by the lagged
batches, advances the class watermark, and persists it. The materialization set
is class-wide, not request-only, so a read of one thread can catch up sibling
thread/marker/remote refs in the same class.

Every ref write enters `write_chokepoint`, which calls
`materialize_committed_tail` for both local and shared classes before validating
or publishing the caller's update. That gives writes the same last-clean floor as
reads and prevents a stale canonical ref from being used as a CAS/expectation
base.

`OplogRefReconciler` is path-backed, not tied to a cached `OpLog`, so a long-held
handle sees commits made through another handle. The regression tests cover the
load-bearing cases: a long-held handle reconciles an oplog-only fork on its next
read, and a handle opened before a concurrent committed-but-unpublished ref also
reconciles on its next read (`crates/repo/src/atomic/tests.rs`).

## What eager open would add

An eager pass at open would pre-run the same class materialization that the first
lagging read or write already runs. Its only real benefits are:

- lower latency for the first ref read after opening a handle with a lagging
  persisted watermark;
- canonical refs may be caught up even if the handle later reads no ref in that
  class.

The second point is not a correctness requirement in the current model. A handle
that never reads or writes a ref exposes no stale ref value. A handle that writes
already materializes both classes before the write. A handle that reads any ref
uses the read chokepoint before trusting canonical storage.

## Why not now

Open-time eager reconciliation would not replace read-time reconciliation. A
commit can land after open, and long-lived daemon or mount handles must still
observe it on the next read. The existing per-read tests are therefore the
correctness floor; eager open can only be a prefetch.

The cost is paid at the wrong time. `Repository::open` is used by many commands
and setup paths, including paths that may not need ref data. The hot no-lag case
would be cheap if implemented through the existing class gates, but any lagging
open would take the refs lock and run the same tail fold/materialization work
that is currently paid only when a read or write actually needs it. The current
reconciler also asks the oplog for scoped recent batches and filters by
watermark inside the fold, so doing this speculatively at open is not a free
optimization.

The persisted watermark already solves the important fresh-open robustness case
without resurrecting old records. The tests around `persisted_watermark_recovers_cross_process_crash_tail`,
`persisted_watermark_does_not_resurrect_unrecorded_delete`, and
`shared_watermark_is_cross_worktree_no_resurrect` show why the last-clean floor
is the important state. An eager fold that ignores or weakens that floor would
re-open the old resurrection class; an eager fold that honors it is just the
first read's work moved earlier.

## Revisit when

Revisit eager open if profiling shows first-read reconciliation latency is a
material user-visible cost and most opens immediately read refs anyway, or if a
new API needs canonical refs to be materialized before any logical ref read or
write occurs. The safer future optimization is probably a cheaper committed-ref
index or dirty-class signal, not an unconditional open-time fold.
