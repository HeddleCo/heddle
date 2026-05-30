# heddle#330 — atomic-mutation primitive (trait + generic executor, no `dyn`)

**Status:** spike (decision doc). No production code lands in this issue. Trait
sketches below are illustrative. The follow-up impl epic shape is proposed in
§7 — to confirm with the orchestrator/user before filing.

**Scope:** a heddle-native primitive that makes "multi-step mutation with a
forgotten or mis-ordered cleanup" structurally unrepresentable. The primitive
is a `trait` each mutation implements + a generic `execute<M>` that enforces
the commit point and reverse-order rewind exactly once.

**Grounding:** every claim here is checked against the code at the cited
`path:line` (verified 2026-05-30). The value of this spike is the reconciliation
between the issue's proposed shape and what the three durability domains
(object store / refs / oplog / FS) actually do today — not an abstract trait
sketch.

---

## §0 — TL;DR / recommendation

- **Build it.** The bug class (#305 ordering, #302 half-started thread, #251
  cross-process reserve, #198 transaction-id uniqueness) is real and recurring,
  and the executor-enforces-the-contract-once shape genuinely closes it.
- **Trait + generic `execute<M: AtomicMutation>`, static dispatch, no `dyn`.**
  Confirmed: no real call site needs a heterogeneous runtime op queue. The one
  candidate (the transaction sentinel's `buffered_ops`,
  `transaction_sentinel.rs:48`) stores verb *strings*, not trait objects, and
  re-dispatches through the CLI — it does not need `dyn AtomicMutation`. Keep
  it `dyn`-free.
- **The linearization point is NOT simply "the oplog append" — correct that
  framing.** In the real workhorse (`capture`), the **ref write** is the
  externally-visible commit and the oplog append happens *after* it
  (`repository_snapshot.rs:243` then `:252`). The primitive must define the
  commit point as **the last durable write that publishes the new state**, and
  reorder so the oplog append is genuinely last (the "it happened + it's
  undoable" record). See §2 — this is the single most load-bearing correction
  in the spike.
- **Nesting = enroll-into-outermost (savepoint) by default; eager-commit only
  when an effect must be visible to another process before the outer commit**
  (the #251 reserve). The trait encodes this as an associated `const`/type that
  the executor checks: an eager sub-op inside a txn that does not supply a
  compensator is a **compile error**. See §3.
- **Panic-safety: explicit `Result` plumbing for the rewind ledger, `Drop` as a
  backstop that aborts (never half-commits).** See §4.
- **Migrate in priority order:** undo (§5.1), hydrate/thread-start (§5.2),
  capture (§5.3), then op-id reserve (§5.4, the eager-commit exemplar). See §7.

---

## §1 — What already exists (the primitives the executor composes with)

The primitive is **not** built from scratch. Three single-domain atomic
mechanisms already exist; the executor's job is to sequence and unwind across
them, because **there is no cross-domain transaction log** (the issue's "honest
constraint" — confirmed).

### 1.1 — Refs: CAS + an in-domain staged-plan/reverse-rollback batch

`RefManager` already exposes compare-and-swap ref writes keyed on an expectation
enum:

- `RefExpectation<T> { Any, Missing, Value(T) }` — `refs/src/refs/types.rs:9`.
- `set_marker_cas(name, expected, state)` — `refs/src/refs/refs_manager.rs:197`;
  `set_thread_cas` — `:141`; `write_head_cas` — `:122`; `delete_*_cas` — `:166`,
  `:218`. `create_marker` is just `set_marker_cas(.., Missing, ..)` (`:194`) —
  CAS-create.
- `RefUpdate { Thread | Marker | Head }` — `types.rs:16` — and
  `update_refs(&[RefUpdate])` (`refs_manager.rs:319`) applies a **batch** under
  a single refs lock (`lock_refs()`, defined `refs_storage.rs:153`, taken by
  `set_undo_recovery` at `refs_manager.rs:243`).

Crucially, `update_refs_with_lock` (`refs/src/refs/refs_transactions.rs:103`) is
itself a miniature saga, and it is the template the cross-domain executor
generalizes:

1. **Plan** every update, checking each `expected` against the on-disk value via
   `matches_expectation` and rejecting conflicts up front (`:127`, `:167`,
   `:199`).
2. **Stage** new contents into temp files (`write_string_temp`, `:219-224`) —
   nothing canonical is touched yet.
3. **Apply in order** — rename temp→canonical + fsync dir (`:228-256`).
4. On any apply error, **`rollback_updates` in REVERSE order** (`:300-323`):
   restore each applied ref's `previous_content` (or delete if it was created),
   then restore the `packed-refs` snapshot.

That reverse-order rollback over a recorded "previous value" ledger is exactly
the executor's rewind discipline — but scoped to one domain. The gap the
primitive fills is that **refs, oplog, object store, and FS each have their own
lock and their own rollback, with nothing tying them together.**

### 1.2 — Oplog: the append, and an existing idempotent commit

- `OpLog::record_batch_scoped(ops, scope)` — `oplog/src/oplog/oplog_core.rs:236`
  — takes the oplog `write_lock()` (`:66`, `:245`), reloads fresh from disk
  (`:247`, to catch other processes), `packed.append(new_entries)` (`:256`),
  `packed.save()?` (`:257`). **`packed.save()` is the durable append.**
- `OpLog::record_batch_scoped_if_no_transaction(ops, scope, transaction_id,
  recent_window)` — `oplog_core.rs:281` — is an **already-shipped idempotent
  commit**: it scans the recent window for an
  `OpRecord::TransactionCommit { transaction_id, op_count }`
  (`oplog_types.rs:84`) matching `transaction_id` and returns `Ok(None)` without
  writing if found, all under the same held write lock (the heddle#198 r4 fix —
  see the comment at `oplog_core.rs:263-280`). This is the model for "commit
  exactly once even under crash-retry," and the primitive's commit step should
  reuse it rather than invent a new idempotency key.

### 1.3 — Object store: reversible-until-referenced + an abort batch

`snapshot_*` writes the state object first and treats it as disposable until a
ref points at it:

- `store.put_state(&state)` + `store.flush_snapshot_write_batch()` —
  `repo/src/repository_snapshot.rs:224-225`.
- `store.abort_snapshot_write_batch()` on error — `:314-316`.
- The designed crash window is documented inline (`:227-233`): a crash after
  `put_state` but before the ref write leaves "the state object durable on disk
  but no ref pointing at it … captured work is effectively dropped (no
  corruption)." An unreferenced state is a harmless orphan that `gc` collects.

This is the cheapest rewind in the system: **an object-store write needs no
explicit compensator** — leaving the orphan is safe. The primitive should model
this as a "no-op rewind, gc reclaims" leg, not force authors to write a delete.

### 1.4 — FS: atomic write-temp-then-rename-then-fsync

`crates/objects/src/fs_atomic.rs` is the filesystem staging primitive:
`temp_path` (`:133`), the rename (`:346`), `sync_directory` (`:173`/`:178`),
`enrich_rename_error` for cross-mount `EXDEV` (`:289`). Staging into a temp and
renaming into place is the per-leg "stage then commit" the executor relies on
for FS effects (it is what `refs_transactions.rs` already uses internally).

### 1.5 — op_scope: per-checkout identity

`Repository::op_scope()` — `repo/src/repository.rs:1636` — returns
`wt-<blake3(canonical .heddle/HEAD path)[..16]>`. It is **per-worktree** even
when several worktrees share one oplog backend (objectstore-pointer threads),
because the local `HEAD` pointer dir is unique per checkout. `undo`/`redo`/
`--list` filter by exact-match scope (`undo.rs:108`, `:131-132`). The
transaction context the primitive threads (§3) must carry `op_scope` so nested
ops record under the same lane and a sibling checkout's executor never unwinds
this one's.

### 1.6 — There is already a (detection-only) transaction concept

`ActiveTransaction` sentinels live at `<heddle_dir>/state/transactions/<id>.toml`
(`cli/src/cli/transaction_sentinel.rs:33-52`); `active_transactions()` (`:60`)
lists open ones; `daemon/src/transaction_replay.rs` does startup crash recovery
of stuck `active` sentinels. But it is **detection only** — the module's own doc
says recording verbs into `buffered_ops` and replaying them at commit "is the
larger follow-on" (`transaction_sentinel.rs:10-16`, `:43-47`). The primitive in
this spike is the in-process, type-enforced sibling of that on-disk concept. The
two should share the `transaction_id` + `op_scope` keys so the in-process
executor and the on-disk sentinel agree (§3.4).

---

## §2 — Trait API + commit-point / ordering semantics

### 2.1 — The trait (illustrative; not committed to crates)

```rust
/// A single all-or-nothing mutation. Implementors supply the staged
/// forward work and their OWN correct, idempotent rewind. The generic
/// `execute` (below) enforces the commit point + reverse-order rewind.
pub trait AtomicMutation {
    /// The value produced on a committed run (e.g. the new `ChangeId`).
    type Output;

    /// Whether this op, when run as a *sub-op* of a larger transaction,
    /// must commit its externally-observable effect BEFORE the outer
    /// commit (`Eager`) or may defer to the outermost commit
    /// (`Savepoint`, the default). See §3. The executor reads this to
    /// decide enrollment; an `Eager` op MUST override `compensate`
    /// (enforced at compile time via the `EagerMutation` marker, §3.3).
    const COMMIT_KIND: CommitKind = CommitKind::Savepoint;

    /// Forward, staged, fallible side effects: object-store puts, ref
    /// CAS *stages* (recorded as planned inverses), FS temp writes.
    /// MUST NOT perform the oplog append — that is the executor's single
    /// commit step. Every effect performed here MUST be paired with a
    /// rewind recorded into `tx` (see `Tx::on_rewind`).
    fn apply(&mut self, tx: &mut Tx<'_>) -> Result<StagedCommit<Self::Output>>;

    /// Undo whatever THIS op's `apply` did, given the ledger captured at
    /// apply time. Called in reverse order on any pre-commit failure or
    /// panic-unwind. MUST be idempotent (may be invoked after a partial
    /// apply) and MUST undo ONLY what this invocation created — never
    /// pre-existing user state (the #302 r4 lesson, §5.2).
    fn rewind(&mut self, ledger: &RewindLedger) -> Result<()>;
}

pub enum CommitKind { Savepoint, Eager }

/// What `apply` returns: the value to surface plus the oplog record(s)
/// the executor will append AT the commit point. The op never appends
/// to the oplog itself; it hands the record to the executor.
pub struct StagedCommit<T> {
    pub output: T,
    pub oplog: Vec<OpRecord>,
}
```

`Tx` (the transaction context, §3) carries the rewind ledger, the depth, the
`op_scope`, and the held domain locks. The `execute` entry point:

```rust
pub fn execute<M: AtomicMutation>(repo: &Repository, mut m: M) -> Result<M::Output> {
    let mut tx = Tx::root(repo);              // depth 0, fresh ledger, takes locks
    let staged = match m.apply(&mut tx) {     // stage everything reversibly
        Ok(s) => s,
        Err(e) => { tx.rewind_all(); return Err(e); }   // reverse-order unwind
    };
    // THE commit point — last, single, idempotent (§2.2):
    tx.commit(staged.oplog)?;                 // oplog append (idempotent by txn id)
    Ok(staged.output)
}
```

Monomorphized per `M`; zero vtable. The bound `M: AtomicMutation` makes
"register an atomic op without a `rewind`" a **compile error** — exactly the
type-state/witness-gated idiom heddle already uses (e.g. the trust/verification
witnesses).

### 2.2 — The commit point: correct the issue's framing

The issue says "commit at the oplog-append linearization point." The real
workhorse disagrees, and the spike must reconcile it. In
`snapshot_with_attribution_profiled` (`repository_snapshot.rs:52`) the order is:

1. `put_state` + `flush_snapshot_write_batch` — `:224-225` (reversible: orphan).
2. fault-injection checkpoint `snapshot_after_state_before_ref` — `:233`.
3. **ref write** `set_thread` / `write_head` — `:241-250`.
4. **oplog** `record_snapshot` — `:252`.

The externally-visible "it happened" moment is **step 3, the ref write** — that
is what the R7 SIGKILL test asserts (`cli/tests/cli_integration/fault_injection.rs:157-244`:
the invariant is the *ref* didn't advance). The oplog append is step 4, *after*
the publish, and today a crash between 3 and 4 yields a captured+visible state
that is **not undoable** — a real (if benign) partial state the current code
tolerates.

So the primitive defines the commit point precisely:

> **The commit point is the last durable write that publishes the new state to
> any other reader. The executor orders the oplog append to BE that last write,
> and makes it idempotent, so "committed" ⇔ "oplog entry exists."**

Concretely the canonical order the executor enforces is:

| Phase | Domain | Reversibility | Who owns rewind |
|---|---|---|---|
| 1. stage object(s) | object store | orphan, gc reclaims | no-op rewind |
| 2. stage FS | filesystem | temp files, unlinked on rewind | executor (temp ledger) |
| 3. stage refs (CAS) | refs | inverse-CAS recorded | executor (prev-value ledger, cf. §1.1) |
| 4. **commit** | **oplog** | **the linearization point** | n/a — past here it happened |

Steps 1–3 are *staged* and individually reversible; only step 4 makes it real.
This **inverts** capture's current ref-then-oplog order. The migration (§5.3)
therefore changes capture's ordering so the oplog append is genuinely last, and
registers a ref inverse-CAS so a (now near-impossible) post-stage failure rolls
the ref back. This closes the ref-moved-but-not-recorded window that exists
today.

**Idempotency of the commit.** Reuse `record_batch_scoped_if_no_transaction`
(`oplog_core.rs:281`) keyed on the op's `transaction_id`: a crash-retry that
re-runs `execute` re-stages (cheap, reversible) and the commit is a no-op if the
`TransactionCommit` marker already exists. This is the existing #198 mechanism,
not a new one.

### 2.3 — Idempotency requirements on `rewind`

Because the model is a saga over three independently-locked domains (no single
txn log), `rewind` correctness is the load-bearing contract:

- **Idempotent.** `rewind` may run after a *partial* `apply` (the apply failed
  midway) or after a panic. It must tolerate "the effect was never performed"
  (e.g. ref already at the prev value, temp file already gone). The refs
  rollback already models this (`refs_transactions.rs:308-312`: restore prev, or
  delete-if-created, both tolerant).
- **Scoped to this invocation only.** A rewind must undo only what *this*
  `apply` created, never pre-existing user state. This is the #302 r4 precision
  requirement made into a trait contract (§5.2).
- **CAS-guarded.** A ref rewind uses inverse `*_cas` with
  `RefExpectation::Value(what_we_wrote)` so it refuses to clobber a concurrent
  writer that moved the ref after us — it fails loud rather than overwriting.

---

## §3 — Nesting / composition (the must-have)

A composite op (`undo` = capture-recovery + apply-batch; `thread start` =
checkout + manifest + record + registry) is itself an `AtomicMutation` whose
`apply` invokes child mutations, and the whole nest is one all-or-nothing unit.

### 3.1 — Enroll-into-outermost (the default: savepoint)

`Tx` threads through `apply`. An inner op does **not** call the top-level
`execute` (which would commit independently); instead the outer op enrolls the
inner one into the *same* `Tx`:

```rust
fn apply(&mut self, tx: &mut Tx<'_>) -> Result<StagedCommit<()>> {
    // outer op's own staging …
    let inner_out = tx.enroll(CaptureRecovery::new(self.head))?;  // savepoint
    let batch_out = tx.enroll(ApplyUndoBatch::new(self.batch))?;  // savepoint
    Ok(StagedCommit { output: (), oplog: merge(inner_out.oplog, batch_out.oplog) })
}
```

`Tx::enroll` runs the inner op's `apply` against the *same* ledger and pushes its
`rewind` onto the shared stack. **Only the outermost `execute` calls
`tx.commit`** — inner ops never hit the oplog on their own; their staged
`OpRecord`s bubble up and are appended in the single outer commit. Depth and
order are tracked by the ledger's push order: `rewind_all` pops in **LIFO /
reverse order** across the whole nest, at any depth. Composition stays static —
`enroll::<Inner>` is monomorphized; `Tx` is a concrete struct, not a trait
object.

```rust
pub struct Tx<'a> {
    repo: &'a Repository,
    scope: String,                 // repo.op_scope() — §1.5
    transaction_id: String,        // idempotency key for the commit — §2.2
    depth: u32,
    rewind: Vec<RewindEntry>,      // LIFO; popped in reverse on unwind
    // held domain locks (refs lock, oplog write lock) acquired once at root
}

enum RewindEntry {
    /// A deferred (savepoint) inner op's rewind closure + its captured ledger.
    Deferred(Box<dyn FnOnce(&RewindLedger) -> Result<()>>),
    /// An eagerly-committed sub-op's compensator (§3.2). Run on outer rollback.
    Compensator(Box<dyn FnOnce() -> Result<()>>),
}
```

> Note: `RewindEntry` boxes a *closure* internally — this is an implementation
> detail of the ledger, NOT `dyn AtomicMutation`. The public composition surface
> (`enroll::<Inner>`) is fully static/monomorphized; the boxed `FnOnce` is just
> how the executor stores "the work to undo entry N" uniformly. No mutation type
> is ever invoked through a vtable. (If even this boxing is undesirable, the
> ledger can instead be an enum over the concrete `OpRecord` inverses — see §6
> open question O3.)

### 3.2 — Eager-commit exception (the rule, pinned)

Some sub-ops produce an effect that **another process must observe before the
outer transaction commits.** The exemplar is the op-id reserve (#251): the whole
point of `store.reserve(op_id, …)` (`operation_id.rs:115`,
`operation_dedup.rs:216`) is that a *concurrent* `heddle` process sees
`DedupOutcome::InFlight` and backs off. If the reservation deferred to the outer
commit, a second process racing the same `op_id` would not see it and both would
execute — defeating the purpose.

**The rule (decision):**

> A sub-op must be `CommitKind::Eager` **iff its forward effect is durable state
> that a different process or a different repo handle can read, and the
> correctness of *that other reader* depends on seeing the effect before the
> outer transaction commits.** Everything else is `Savepoint`.

Operationally, a sub-op is `Eager` iff **both**:
1. its effect lands in a **cross-process-visible** store (a file other processes
   stat/read under a shared lock — the dedup store
   `operation_dedup.rs:216`/`acquire_file_lock`, a ref another process
   resolves), **and**
2. some external actor's behavior **branches on observing it** mid-transaction
   (the racing process backing off; a child process the op spawns and waits on —
   `operation_id.rs:145-162`).

A `Savepoint` op's effects are only read by *this* transaction until commit
(staged object, staged FS, a ref no other process resolves until we publish), so
deferring is safe.

**How an eager sub-op participates in outer rollback.** It commits eagerly inside
`apply` (e.g. `store.reserve` returns `Reserved`) and **registers a
compensator** with the outer `Tx` (`RewindEntry::Compensator`). If the outer
transaction later fails, `rewind_all` runs that compensator — for the reserve,
the compensator is `store.cancel(op_id, verb)` (`operation_id.rs:152`,
`operation_dedup.rs:288`), which releases the reservation so a retry isn't
wedged on a stale `InFlight`. The compensator is saga semantics for that one
leg: the effect was really visible for a while, and the compensator makes the
*net* outcome correct. Eager legs commit in apply-order; their compensators run
in reverse with everything else.

### 3.3 — The compile-time enforcement

An `Eager` op without a real compensator is a silent disaster (a leaked
reservation). Make it impossible to express:

```rust
/// Marker the executor requires before it will enroll an Eager sub-op.
/// `enroll_eager` is bounded `M: AtomicMutation + EagerMutation`, so an
/// op declaring COMMIT_KIND = Eager that does not implement EagerMutation
/// (whose `compensate` is the only way to satisfy the bound) fails to
/// compile at the enroll site.
pub trait EagerMutation: AtomicMutation {
    /// Runs eagerly inside `apply`; returns the compensator the outer Tx
    /// stores. Separate from `rewind` because an eager leg's undo is a
    /// *forward* compensating action (cancel/release), not a staged-state
    /// rollback.
    fn commit_eager(&mut self, tx: &mut Tx<'_>) -> Result<Compensator>;
}

impl<'a> Tx<'a> {
    pub fn enroll<M: AtomicMutation>(&mut self, m: M) -> Result<StagedCommit<M::Output>> { … }
    pub fn enroll_eager<M: AtomicMutation + EagerMutation>(&mut self, m: M) -> Result<M::Output> { … }
}
```

The discipline: `enroll` (savepoint) is the only path for a `Savepoint` op;
`enroll_eager` is the only path for an `Eager` op and it *requires*
`EagerMutation`. A `debug_assert!(M::COMMIT_KIND == Eager)` inside `enroll_eager`
(and the inverse in `enroll`) catches a mislabeled `const`. The result: you
cannot enroll an eager sub-op without handing the executor a compensator.

### 3.4 — Re-entrancy, locks, and the on-disk sentinel

- **Locks acquired once at the root `Tx`.** The refs lock (`lock_refs()`,
  `refs_storage.rs:153`) and the oplog write lock (`oplog_core.rs:66`) are
  reentrant-by-ownership within one `Tx`: the root holds them, inner ops borrow
  `&mut Tx` and never re-lock. This avoids the self-deadlock an inner op would
  hit if it called a top-level `update_refs` (which takes the lock again). The
  migration must route inner ref writes through `Tx` helpers, not the raw
  `RefManager` methods.
- **`op_scope` flows down the nest** (`Tx.scope`), so every `OpRecord` the nest
  emits records under the same checkout lane (§1.5) and a sibling checkout's
  executor never sees or unwinds this transaction.
- **Bridge to the on-disk sentinel (§1.6).** The root `Tx`'s `transaction_id`
  should be the same id written into the `<heddle_dir>/state/transactions/`
  sentinel, so `daemon::transaction_replay`'s startup recovery and the
  in-process executor agree on "did this commit?" via the single
  `OpRecord::TransactionCommit { transaction_id }` marker. (Wiring this is impl
  work, flagged in §7; the spike only fixes the shared key.)

---

## §4 — Panic-safety

**Decision: explicit `Result` plumbing is the primary unwind path; `Drop` is a
backstop whose only job is to ABORT, never to half-commit.**

- **`Result` path (primary).** `execute` matches `apply`'s `Result`; on `Err` it
  calls `tx.rewind_all()` (reverse-order ledger walk) before returning. This is
  deterministic, testable (the refs rollback is already unit-tested this way,
  `refs_transactions.rs:341-377`), and surfaces rewind failures as errors the
  caller sees.
- **`Drop` backstop (panic only).** `Tx` implements `Drop`. If a `Tx` is dropped
  **without** having reached `commit` (a panic unwound through `apply`, or an
  early `?` the author forgot to route — though the API makes that hard), `Drop`
  runs `rewind_all` and, critically, **never appends to the oplog.** Because the
  commit point is the *last* action and is only reached on the success path, an
  unwinding `Tx` is by construction pre-commit, so the safe action is always
  "rewind the staged effects." A `committed: bool` flag set in `commit` makes
  `Drop` a no-op once the linearization point passed.

  ```rust
  impl Drop for Tx<'_> {
      fn drop(&mut self) {
          if !self.committed {
              // best-effort reverse-order unwind; log (don't panic) on
              // a rewind error to avoid a double-panic abort.
              if let Err(e) = self.rewind_all() {
                  tracing::error!(error = %e, "Tx Drop rewind failed; \
                      staged effects may persist as orphans (gc-collectable) \
                      — see transaction sentinel for recovery");
              }
          }
      }
  }
  ```

- **Why not `Drop`-only (the rejected alternative).** A `Drop`-only design can't
  return a rewind error to the caller and risks double-panic if a rewind itself
  panics. And it muddies "did this commit?" — the explicit path keeps the commit
  a visible, single statement.
- **Interaction with `op_scope` / per-checkout scoping.** Because `Tx` holds the
  oplog + refs locks for its whole lifetime and `op_scope` keys every record, a
  panic in checkout A's `Tx` cannot strand checkout B: B's executor is a
  different `Tx` with a different scope and re-acquires the locks A's `Drop`
  releases. Crash-across-process (SIGKILL, not unwind) is the on-disk sentinel +
  `daemon::transaction_replay`'s job (§3.4) — the in-process `Drop` covers only
  in-process panics; the spike does not claim otherwise.

---

## §5 — Retrofit inventory (each call site as an `AtomicMutation`)

Sketches are illustrative. Each lists: the steps, the commit point, the rewind,
and whether it nests / needs eager-commit.

### 5.1 — `undo` / `redo`  (#305) — **highest priority**

**Today** (`cli/src/cli/commands/undo.rs:93` `cmd_undo`):
1. preflights (`:142-144`) — refusals, no mutation.
2. record pre-undo recovery ref BEFORE apply (`:196-199`,
   `refs_manager.rs:242` `set_undo_recovery`) — the #305 ordering fix.
3. loop over batches: `apply_undo_batch(&repo, &batch)` then
   `oplog.mark_batch_undone(&batch)` (`:202-205`).

**The hazard:** if `apply_undo_batch` fails on batch *N* after batches `0..N`
were applied **and marked undone**, there is no rollback — the repo is left
half-rewound (some batches undone, worktree partially rewritten). The preflights
reduce the odds but cannot eliminate a mid-apply failure.

**As an `AtomicMutation` (composite, nests, no eager leg):**
```rust
struct Undo { batches: Vec<OpBatch>, head: Option<ChangeId> }
impl AtomicMutation for Undo {
    type Output = UndoSummary;
    fn apply(&mut self, tx: &mut Tx) -> Result<StagedCommit<UndoSummary>> {
        // savepoint sub-op: stage the recovery ref (inverse-CAS recorded)
        tx.enroll(SetUndoRecovery::new(self.head))?;
        for batch in &self.batches {
            // savepoint sub-op per batch: stage worktree rewrite + the
            // mark-undone, recording the inverse (re-apply / mark-redone)
            tx.enroll(ApplyUndoBatch::new(batch.clone()))?;
        }
        Ok(StagedCommit { output: …, oplog: vec![/* the undo records */] })
    }
    fn rewind(&mut self, _l: &RewindLedger) -> Result<()> { Ok(()) } // children own it
}
```
Now a failure on batch *N* triggers `rewind_all`: batches `0..N` re-apply +
mark-redone in reverse, the recovery ref restores its prev value — **atomic
undo**. Nests: yes (recovery + per-batch). Eager: no.

### 5.2 — `thread start` / hydrate  (#302) — **second priority**

**Today** (`cli/src/cli/commands/thread.rs`, `cmd_start`):
1. `prepare_worktree_target` (`:1709` → `worktree_cmd/helpers.rs:11`) — validates
   + `std::fs::create_dir_all` (`helpers.rs:20`).
2. `write_isolated_checkout` (`thread.rs:1761`) — materializes files on disk.
3. `record_thread_manifest` (`:1769`).
4. `thread_manager.save(&thread_state)` (`:1865`) — persists the record.
5. `registry.create_generated_entry_for_thread` (`:1866`) — agent registry.

**The hazard (#302):** a failure at step 4 or 5 (or the mount path,
`:1795`) after step 2 created the checkout leaves a **half-started thread** — a
directory full of files with no thread record, or a record with no registry
entry.

**The #302 r4 precision requirement, encoded as a rewind contract:**
`prepare_worktree_target`'s `create_dir_all` (`helpers.rs:20`) is a **no-op when
the user passed `--path` to a pre-existing empty directory** (`validate_worktree_target`
explicitly *allows* an existing empty dir, `helpers.rs:68-80`). Therefore the
rewind for the "create worktree dir" leg **must record whether it actually
created the leaf directory**, and on rewind remove **only what it created** —
never `rm -rf` the user's pre-existing directory. This is exactly "undo only
what THIS invocation created" (§2.3) made concrete.

**As an `AtomicMutation` (composite, nests, no eager leg):**
```rust
struct StartThread { … }
impl AtomicMutation for StartThread {
    fn apply(&mut self, tx: &mut Tx) -> Result<StagedCommit<…>> {
        // leg 1: create dir, recording created-vs-preexisting for a precise rewind
        let dir = tx.enroll(CreateWorktreeDir::new(target))?; // rewind: rmdir IFF we created it
        tx.enroll(WriteIsolatedCheckout::new(dir, base_state))?; // rewind: remove written files
        tx.enroll(RecordThreadManifest::new(…))?;
        tx.enroll(SaveThreadRecord::new(record))?;               // rewind: delete the record
        tx.enroll(CreateAgentEntry::new(entry))?;                // rewind: strip the entry
        Ok(StagedCommit { output: …, oplog: vec![/* ThreadCreateV2 */] })
    }
}
```
A failure at any leg unwinds the prior legs in reverse — no half-started thread,
and the user's pre-existing `--path` directory survives. Nests: yes. Eager: no.

### 5.3 — `capture` / `snapshot`  (#198-adjacent) — **third priority**

**Today** (`repo/src/repository_snapshot.rs:52`): object → ref → oplog (§2.2),
with `abort_snapshot_write_batch` (`:314`) covering only the object batch and a
fault checkpoint at `:233`.

**As an `AtomicMutation` (leaf, no nest, no eager leg):** the migration's
*behavioral* change is **reordering the oplog append to be last** and registering
a ref inverse-CAS so a post-stage failure rolls the ref back (closing the
ref-moved-but-not-recorded window). The object-store leg uses the cheap no-op
rewind (orphan + gc, §1.3); the ref leg records inverse-CAS
(`set_thread`/`write_head` ←→ prior value via `RefExpectation::Value`); the
commit is the idempotent oplog append (`record_batch_scoped_if_no_transaction`,
§2.2). This is the cleanest demonstration that the primitive *strengthens* an
existing contract rather than just refactoring it. Nests: no. Eager: no.

### 5.4 — op-id reserve  (#251) — **the eager-commit exemplar, fourth priority**

**Today** (`cli/src/operation_id.rs:62` `run_local_idempotency_if_requested`):
`store.reserve(op_id, command_name, request_hash)` (`:115`,
`operation_dedup.rs:216`) returns `Reserved` / `Replay` / `InFlight` / `Conflict`
(`operation_dedup.rs:104`); on `Reserved` it spawns the child, then `store.record`
(`:162`) or `store.cancel` on spawn failure (`:152`).

**As an `EagerMutation` sub-op:** when an op-id-bearing command is itself wrapped
in a transaction, the reserve is the canonical `Eager` leg:
```rust
impl EagerMutation for ReserveOpId {
    fn commit_eager(&mut self, tx: &mut Tx) -> Result<Compensator> {
        match self.store.reserve(self.op_id, self.verb, self.hash)? {
            DedupOutcome::Reserved => {
                let (store, op, verb) = (self.store.clone(), self.op_id, self.verb.clone());
                Ok(Compensator::new(move || store.cancel(op, &verb)))   // outer rollback releases it
            }
            DedupOutcome::Replay { .. } | InFlight | Conflict => /* surface as the existing typed error */,
        }
    }
}
```
The reservation is visible to other processes the instant `reserve` returns
(cross-process file lock, `operation_dedup.rs:216`) — it **cannot** defer to the
outer commit (§3.2 rule, both conditions met: cross-process store + racing
process branches on it). On outer rollback the compensator runs `store.cancel`
(`operation_dedup.rs:288`) so a retry isn't wedged on a stale `InFlight`.
Eager: **yes** — this is the whole reason `CommitKind::Eager` exists.

### 5.5 — ref-write paths (already in-domain-atomic)

`update_refs(&[RefUpdate])` (`refs_manager.rs:319`) is already atomic + reverse-
rollback **within the refs domain** (§1.1). These do not need migration on their
own; they become the executor's "stage refs" leg (§2.2 phase 3). The win is only
realized when a ref write is *combined* with an oplog append or an FS effect in
one mutation (capture, undo) — which §5.1–5.3 cover.

### 5.6 — Inventory summary

| Site | File:line | Nests? | Eager leg? | Priority | What the primitive fixes |
|---|---|---|---|---|---|
| undo/redo | `undo.rs:93` | yes | no | 1 | mid-apply leaves repo half-rewound |
| thread start | `thread.rs` `cmd_start` (`:1709`+) | yes | no | 2 | half-started thread; precise dir rewind (#302 r4) |
| capture | `repository_snapshot.rs:52` | no | no | 3 | ref-moved-but-not-recorded window; oplog-last ordering |
| op-id reserve | `operation_id.rs:115` | as sub-op | **yes** | 4 | eager-commit exemplar; stale `InFlight` on rollback |
| ref writes | `refs_manager.rs:319` | n/a | no | — | already in-domain atomic; becomes the "stage refs" leg |

---

## §6 — Open questions / risks (carry into the impl epic)

- **O1 — Reordering capture is a behavior change.** Moving the oplog append after
  the ref write to *being the commit* changes the crash window the R7 test
  (`fault_injection.rs:157`) pins. The impl must add a new fault checkpoint
  (`*_after_ref_before_oplog`) and a test that the ref rewinds when the oplog
  append fails. Low risk (strictly strengthens the contract) but must be done
  deliberately.
- **O2 — Lock ordering / deadlock.** The root `Tx` holds the refs lock and the
  oplog write lock simultaneously. Any *other* path that takes both must take
  them in the same order. The impl must audit for the reverse order (a grep for
  `lock_refs` + `write_lock` co-occurrence) and add a documented lock hierarchy.
- **O3 — Ledger representation: boxed `FnOnce` vs `OpRecord`-inverse enum.** §3.1
  boxes closures in the ledger for uniformity. If "no heap allocation in the hot
  path" matters, the ledger can instead be an `enum` over concrete inverse
  records (ref prev-value, FS temp path, eager compensator id). Decide in impl;
  does not affect the public `dyn`-free surface either way.
- **O4 — Sentinel bridge scope.** Fully unifying the in-process `Tx` with the
  on-disk `ActiveTransaction` sentinel (so SIGKILL recovery and in-process
  rollback share one source of truth) is a meaningful chunk. The spike fixes the
  shared `transaction_id`/`op_scope` keys; the wiring is its own impl issue.
- **O5 — Async.** Several backends are `async` (`CoreRefBackend::get_thread` is
  `async`, `refs_manager.rs:395`; oplog backend methods too). `execute` is shown
  sync; the CLI mutation paths are sync today, but the trait may need an
  `async fn apply` variant if a migrated op touches an async backend. Decide per
  migrated op; start with the sync paths (undo/thread/capture are sync).

---

## §7 — Recommendation + follow-up impl epic

**Recommendation: build the primitive, `dyn`-free, and migrate in the priority
order above.** The bug class is real, recurring, and structurally closable; the
executor-enforces-once shape fits heddle's existing type-state idioms; and the
primitives it composes (CAS batch + reverse rollback, idempotent oplog commit,
orphan-tolerant object store, atomic FS rename) already exist — the work is
sequencing them under one ledger, not inventing durability.

### Proposed impl epic shape (blocked by this spike — confirm before filing)

> **Epic: atomic-mutation primitive — land `AtomicMutation` + migrate the
> recurring multi-step mutations.** Blocked by #330.

1. **#330-impl-a — land the primitive (no migrations).** `AtomicMutation`,
   `EagerMutation`, `Tx`, `execute`, the rewind ledger, `Drop` backstop, and the
   reuse of `record_batch_scoped_if_no_transaction` for the idempotent commit.
   Unit tests mirroring `refs_transactions.rs:341-377` (reverse-order rewind) +
   a panic-unwind test. Effort: **xhigh** (intricate state machine + locks +
   panic-safety). No call site changes yet.
2. **#330-impl-b — migrate `undo`/`redo` (#305).** First real user; proves the
   nesting path. Effort: xhigh. Blocked by a.
3. **#330-impl-c — migrate `thread start` (#302), with the precise dir rewind.**
   Effort: high. Blocked by a.
4. **#330-impl-d — migrate `capture` (reorder to oplog-last), with the new fault
   checkpoint + test (O1).** Effort: high. Blocked by a.
5. **#330-impl-e — migrate op-id reserve as the `EagerMutation` exemplar
   (#251).** Effort: high. Blocked by a.
6. **#330-impl-f (optional) — unify the in-process `Tx` with the on-disk
   transaction sentinel (O4).** Effort: xhigh. Blocked by a + the daemon
   transaction-replay owner's review.

Land `a` first and pause: migrating one real op (`b`) validates the design
before committing to the full sweep. If `b` reveals the nesting/lock model needs
revision, only one migration is in flight, not five.

---

## §8 — Deliverable checklist (maps to the issue's 5 + addendum)

- [x] **(1) Trait API** — §2.1, `dyn`-free justified (§0, §3.1 note), trait +
  generic `execute<M>` chosen, fits type-state idiom.
- [x] **(2) Commit-point + ordering** — §2.2 (corrects the oplog-as-commit
  framing against `repository_snapshot.rs:243`/`:252`), §2.3 idempotency.
- [x] **(3) Nesting** — §3: enroll-into-outermost (savepoint) default, eager-
  commit exception **rule pinned** (§3.2), compile-time compensator requirement
  (§3.3), `Tx` context + depth/reverse-order tracking (§3.1), op_scope tie-in
  + sentinel bridge (§3.4).
- [x] **(4) Panic-safety** — §4: explicit `Result` primary, `Drop` abort-only
  backstop, op_scope interaction.
- [x] **(5) Retrofit inventory** — §5: undo, thread/hydrate (with #302 r4
  precision), capture, op-id reserve (eager exemplar), ref-write; each sketched.
- [x] **Recommendation + follow-up impl epic** — §7.
- [x] Real primitives cited by `path:line` throughout; no production code
  changed (this doc only).
