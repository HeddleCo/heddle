# heddle#382 - same-thread transaction isolation for `AtomicMutation`

**Status:** spike decision doc. No production code lands in this issue.

**Decision:** choose **CAS-on-commit** for same-thread concurrent
`AtomicMutation` transactions. The commit point remains the oplog append; the
append becomes conditional on the transaction's declared per-thread isolation
keys having no newer committed oplog entries since the transaction started.

This is option 1 from the issue, with per-thread conflict granularity. It is not
a per-thread long-held transaction lock, and it does not solve shared-worktree
filesystem races.

---

## Current facts

The code already makes the oplog append the `AtomicMutation` commit point.
`Tx::commit` appends the staged records plus `TransactionCommit` through
`record_batch_exactly_once` (`crates/repo/src/atomic/tx.rs:310-340`). The
executor rewinds staged effects on commit failure (`crates/repo/src/atomic/execute.rs:50-72`,
`crates/repo/src/atomic/tx.rs:344-365`).

The current exact-once commit is dedup plus append, not isolation. The local
backend holds the oplog write lock, reloads the packed log, scans the full log
for the same `TransactionCommit { transaction_id }`, and appends if absent
(`crates/oplog/src/oplog/oplog_core.rs:403-439`). There is no expected-head,
parent, or per-thread version check. A different transaction that raced the same
thread just appends too.

The backend trait's older `record_batch_scoped_if_no_transaction` is also only
dedup plus append. Its default implementation reads recent batches and then
calls `record_batch_scoped` (`crates/oplog/src/oplog/oplog_backend.rs:39-74`);
the local override makes that scan+append atomic, but still does not reject a
different transaction.

The undo/redo serialization lock is repo-global and undo/redo-only
(`crates/cli/src/cli/commands/undo_apply.rs:1608-1628`). It prevents a specific
undo selection/idempotency collision, but it is not the general isolation
primitive for normal thread commands.

Thread-record writes are lock-atomic at the record-set call, not at transaction
scope. `ThreadManager::converge_records` holds the thread-record write lock for
one converge operation (`crates/repo/src/thread_storage.rs:345-365`), while a
transaction can contain several independently locked steps.

The existing rewind ledger is the right abort mechanism. Savepoint children
share the root ledger, and eager children register a compensator through
`Tx::enroll_eager` (`crates/repo/src/atomic/tx.rs:251-300`). A CAS conflict is
therefore just another pre-commit failure: rewind the ledger, then retry from a
fresh read.

---

## Why CAS, not a transaction lock

Same-thread isolation should be enforced at the same point that already defines
commit: the durable `TransactionCommit` oplog batch. A long-held per-thread lock
would also be correct, but it would add a second transaction authority around
the existing append and require lock ordering for multi-thread transactions.
That is the failure mode to avoid: correctness would depend on callers holding
the right lock while also eventually reaching the oplog commit point.

CAS-on-commit keeps the invariant in one place:

- A transaction stages and rewinds exactly as it does today.
- At commit, the oplog writer checks dedup first, then checks isolation, then
  appends under the same write lock.
- If a different transaction advanced any declared same-thread key, this
  transaction has not committed. The executor rewinds it and retries.
- No lock is held across object writes, worktree staging, record convergence,
  hydration, or user-space computation.

This gives serializable per-thread isolation for `AtomicMutation` roots without
serializing unrelated threads.

---

## Isolation contract

Each root `AtomicMutation` declares the logical thread keys whose committed
state it read or may write. The granularity is **per thread**, not per touched
file, ref, object, or record id.

Proposed types:

```rust
pub enum IsolationKey {
    Thread(String),
    LocalHead { scope: String },
}

pub struct IsolationPrecondition {
    pub since_head_id: u64,
    pub keys: BTreeSet<IsolationKey>,
}

pub enum ConditionalCommitOutcome {
    Committed(Vec<u64>),
    AlreadyCommitted(Vec<OpRecord>),
    IsolationConflict {
        key: IsolationKey,
        since_head_id: u64,
        conflicting_entry_id: u64,
    },
}
```

`Thread(name)` covers the named thread's logical history: snapshots,
checkpoints, thread ref movement, thread record convergence, undo/redo records,
fast-forward target movement, and other committed records that read or publish
that thread. `LocalHead { scope }` covers detached-HEAD or HEAD-only mutations
whose logical state is scoped to one checkout lane rather than a shared thread
name.

Root mutation API sketch:

```rust
pub trait AtomicMutation {
    type Output;

    fn transaction_id(&self) -> String;

    fn isolation_keys(&self, repo: &Repository) -> Result<BTreeSet<IsolationKey>>;

    fn apply(&mut self, tx: &mut Tx<'_>) -> Result<StagedCommit<Self::Output>>;
}
```

`execute` captures `repo.oplog().head_id()` before `apply` and stores
`IsolationPrecondition { since_head_id, keys }` in the root `Tx`. Capturing the
global `head_id` is already a cheap fixed-header read in the reconciliation
path (`crates/oplog/src/oplog/oplog_core.rs:442-455`,
`crates/refs/src/refs/reconcile.rs:106-123`). The commit check uses it as a
tail boundary, not as global conflict granularity.

The conditional append API replaces `Tx::commit`'s call to
`record_batch_exactly_once`:

```rust
pub fn record_batch_exactly_once_if_unchanged(
    &self,
    operations: Vec<OpRecord>,
    scope: Option<&str>,
    transaction_id: &str,
    precondition: &IsolationPrecondition,
) -> Result<ConditionalCommitOutcome>;
```

Backend semantics under the oplog write lock / SQL transaction:

1. Reload the current log.
2. Search the full committed history for `TransactionCommit { transaction_id }`.
   If found, return `AlreadyCommitted(committed_batch_records)`. This check is
   deliberately first.
3. If `precondition.keys` is empty, append.
4. If current `head_id == precondition.since_head_id`, append; no tail scan.
5. Otherwise scan entries with `id > since_head_id`. If any committed record
   touches one of the declared keys, return `IsolationConflict`.
6. If no declared key advanced, append the batch and return `Committed(ids)`.

Dedup-before-CAS is load-bearing. A crash retry of an already committed logical
transaction must return the committed output even if the same thread has since
advanced. Reversing the order would turn a successful replay into a false
conflict.

---

## Conflict detection

Granularity is per declared thread key. That is intentionally conservative:
two captures of different files on the same thread conflict, because both are
read-modify-write operations against the same thread tip. This prevents
lost-update and write-skew without needing a path-level read/write set.

Different threads do not conflict. A transaction with keys `{Thread("a")}` can
commit after an intervening `{Thread("b")}` append. The commit path scans the
tail only when the global oplog head advanced and classifies entries by their
thread-touch mapping.

The mapper should be explicit and shared by tests. Examples:

- `Snapshot { thread: Some(t), .. }`, `Checkpoint { thread: Some(t), .. }`,
  `ThreadCreate* { name: t, .. }`, `ThreadDelete { name: t, .. }`,
  `ThreadUpdate { name: t, .. }`, `EphemeralThreadCollapse { thread: t, .. }`
  touch `Thread(t)`.
- `FastForward { target_thread, source_thread, .. }` touches both: target is
  written, source was read to define the operation.
- `RemoteThreadUpdate/Delete { thread, .. }` touch `Thread(thread)`.
- `Snapshot { thread: None, .. }`, `Goto`, and local undo-recovery movement
  touch `LocalHead { scope }` unless the root mutation declares a concrete
  attached thread key.
- `TransactionCommit` itself carries no key; the records in the same batch do.

The root mutation's declared key set is authoritative for reads that are not
fully recoverable from the staged records. The staged-record mapper is used to
detect intervening commits. Tests should also assert that every staged record
with a known key is covered by the root declaration, so a mutation cannot
silently write a thread it did not isolate.

Multi-thread transactions declare all involved thread keys up front and commit
only if none advanced. Keys are sorted (`BTreeSet`) for deterministic diagnostic
output; no lock ordering is needed because no per-key lock is held.

---

## Abort, rewind, retry

`IsolationConflict` is not a commit. `Tx::commit` must return a distinct
`CommitOutcome::IsolationConflict`, and `execute` handles it by:

1. Calling `tx.rewind_all()`.
2. Sleeping with full-jitter exponential backoff.
3. Re-running the same logical mutation with the same `transaction_id`.

Recommended local CLI policy: four attempts total (initial attempt plus three
retries), base backoff 10 ms, cap 250 ms. After the cap is exhausted, return a
structured `HeddleError::Conflict` naming the isolation key and conflicting
entry id. Rewind failure stops the loop immediately and reports both the
original conflict and the rewind failure, matching today's commit-failure path.

Retries must reuse the stable `transaction_id`. An aborted CAS attempt has no
commit marker, so reuse is safe. If an earlier process already committed the
same logical transaction, the dedup-first rule returns `AlreadyCommitted` and
the executor follows today's reconstruction path.

This policy is bounded because same-thread contention can otherwise make an
expensive transaction repeat indefinitely. Callers that want an operator-loop
style retry can catch the structured conflict and invoke a new logical command
attempt after re-reading state.

---

## Composition with existing guarantees

**Exactly-once dedup.** The dedup domain and stable `transaction_id` contract do
not change. The new API folds isolation into the same lock-held append, but
checks for an existing commit first. Existing `AlreadyCommitted` handling still
reconstructs the original output from the committed records and rewinds this
run's staging.

**Nested savepoints.** Enrolled deferred mutations already share the outer
`Tx`; only the outermost transaction reaches `commit`. The isolation keys are
declared by the root and cover the whole nest. A conflict aborts the whole
ledger, so savepoint children unwind in the same reverse order as any other
pre-commit failure.

**Eager commit (#358).** Eager children still run before the root commit and
register compensators through `enroll_eager`. A CAS conflict runs those
compensators during `rewind_all`. The current op-id eager reserve intentionally
uses a no-op compensator because the reservation must survive unrelated outer
aborts (`crates/repo/src/operation_dedup.rs:151-157`, `:195-202`); retrying with
the same operation id/request hash observes the existing reservation rather
than minting a second one.

**Ref commit chokepoint.** `OplogRefCommitter::commit_records` appends
ref-carrying records before publishing refs, but it does not append a
`TransactionCommit` marker (`crates/repo/src/atomic/committer.rs:30-47`). The
same-thread isolation guarantee is for `AtomicMutation` transactions. Commands
that are still plain ref commit-and-publish operations must be migrated into an
`AtomicMutation` root, or explicitly remain outside this guarantee.

---

## Cost and retry amplification

No-conflict cost is small relative to the current commit path:

- One extra transaction-start `head_id()` fixed-header read.
- At commit, the current implementation already takes the oplog write lock,
  reloads the packed log, and scans the whole log for exact-once dedup. The CAS
  check adds an integer compare when `head_id` is unchanged.
- If unrelated transactions committed meanwhile, the CAS check scans only
  entries newer than `since_head_id` and classifies their keys. This is
  `O(delta)`, where `delta` is the number of entries appended during this
  transaction's staging window, not the whole history.

When an indexed `transaction_id` lookup lands, the CAS tail scan becomes the
main added cost only on transactions that observed concurrent commits. The same
`head_id` fast path keeps quiet repositories at one header read plus one integer
comparison.

Retry amplification under same-thread contention is the price of optimistic
isolation. If `N` transactions start from the same thread version and all reach
commit together, one wins and `N - 1` abort. With no backoff and perfectly
synchronized retries, the worst case takes:

```text
attempts = N + (N - 1) + ... + 1 = N * (N + 1) / 2
wasted attempts = N * (N - 1) / 2
```

That is the same serial throughput limit a per-thread lock would impose, but
with repeated staging work instead of queueing. The bounded retry cap and jitter
avoid pathological retry storms. For ordinary human-plus-agent contention, the
expected conflict probability is roughly `1 - exp(-lambda * S)`, where `lambda`
is same-thread transaction arrival rate and `S` is the staging duration. Long
worktree/hydrate transactions have higher retry cost; that is acceptable for
the common path because same-thread concurrent mutation is expected to be rare
and correctness is mandatory.

If production telemetry later shows repeated same-thread conflicts on long
transactions, add a scoped per-thread advisory lock as an optimization for those
commands. That is not the default mechanism.

---

## Call-site migration surface

Implementation should be scoped to these surfaces:

1. `oplog`: add `IsolationKey`, `IsolationPrecondition`, and
   `ConditionalCommitOutcome`; implement
   `record_batch_exactly_once_if_unchanged` for the local packed oplog and the
   Postgres backend. The local path performs dedup, tail conflict scan, and
   append under the existing write lock. The Postgres path performs the same
   checks inside one SQL transaction.
2. `repo::atomic::Tx`: store the precondition captured at root creation, return
   a distinct isolation-conflict outcome, and keep `AlreadyCommitted` behavior
   unchanged.
3. `repo::atomic::execute`: wrap the current single execution in the bounded
   retry loop for isolation conflicts only. Pre-apply errors, ordinary commit
   errors, and rewind failures do not retry.
4. `AtomicMutation` roots:
   - `SnapshotMutation`: declare the attached thread key when HEAD is attached;
     otherwise declare `LocalHead { scope }`.
   - `StartThread`: declare the target thread and any source/base thread read
     to derive the new checkout state.
   - `UndoOp` / `RedoOp`: declare every thread key touched by selected batches,
     plus `LocalHead { scope }` for HEAD/recovery movement.
   - `ReserveOpIdTransaction`: declare an empty key set.
   - Test-only mutations in `crates/repo/src/atomic/tests.rs` and
     `undo_apply.rs` fixtures: declare the minimal key set needed by the test,
     usually empty unless the test asserts isolation.
5. Existing transaction-like paths outside `AtomicMutation`:
   - `rebase_ops` still uses `record_batch_scoped_if_no_transaction`; it remains
     exact-once-windowed, not isolated. Migrate it to `AtomicMutation` or mark it
     outside the #382 guarantee.
   - The daemon transaction service appends `TransactionCommit` directly; it
     must use the conditional API or remain explicitly separate from local
     `AtomicMutation` isolation.
   - `OplogRefCommitter`/`commit_and_publish` should not grow an ad hoc CAS
     layer. Commands needing same-thread isolation should enter through an
     `AtomicMutation` root.
6. Tests:
   - same thread, distinct transaction ids: one commit wins; the loser rewinds
     and retries against the new thread version.
   - different threads: both commit without conflict.
   - dedup first: replay of an already committed transaction returns
     `AlreadyCommitted` even after the thread advanced.
   - savepoint child effects rewind on conflict.
   - eager child compensator path runs on conflict; op-id reserve remains
     replay-safe.
   - retry cap returns a structured conflict after repeated same-thread churn.

The undo/redo repo-global lock can stay initially. It solves an additional
selection/idempotency hazard in undo/redo, and removing it is not part of this
implementation issue.

---

## Worktree-edit race boundary

This design scopes the shared materialized worktree race **out**.

CAS-on-commit isolates committed metadata for same-thread transactions. It does
not prevent two actors from editing the same checkout directory, reading and
writing the same files, or moving one worktree's HEAD concurrently at the
filesystem level. Metadata isolation can reject one commit after staging, but it
cannot make the staged filesystem writes invisible to the other process.

Recommended boundary:

- Parallel agents should use different threads / different materialized
  worktrees.
- If Heddle needs concurrent actors in one checkout, add a separate worktree
  lease held across filesystem editing and HEAD movement. That lease is
  orthogonal to the AtomicMutation commit CAS and should be designed as a
  filesystem/worktree safety primitive, not as an oplog isolation primitive.

---

## Follow-up issue scope

The implementation issue should land the conditional exact-once append,
per-thread isolation declarations, bounded retry loop, and tests above. It
should not implement a per-thread long-held transaction lock, path-level
conflict detection, or a shared-worktree lease.
