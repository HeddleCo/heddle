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

> **EXISTING vs PROPOSED — read this first.** This is a **design doc**; nothing
> in it has been built. The round tags **r6–r15** below are *revisions of this
> spike* (Codex review iterations on the document), **not shipped code**. Every
> mechanism this spike introduces — the new/retrofitted `OpRecord` variants
> (`RemoteThreadUpdate`/`RemoteThreadDelete`/`UndoRecoveryUpdate`, and the
> `Fork`/`Collapse` published-ref-identity fields), the `reconciled_load` read
> chokepoint, the `RefReconciler` trait, the `RefManager` write chokepoint, the
> class-split materialization watermark, and the unbounded `transaction_id`
> commit index — is **PROPOSED impl-epic work (§6/§7)**, not present in the
> current tree. When a sentence reads "r9 closed …" / "r11 makes …", parse it as
> "the spike's r9/r11 design closes/makes …", a property that holds **after the
> proposed work lands**, not a description of today's code. Where the universal
> `committed ⇔ ref-carrying oplog record` proof (§2.2/§2.4) says the invariant
> holds for *all* ref classes and *all* writers, it holds **conditionally on the
> proposed format + writer changes being implemented** — today's tree still has
> the direct-write exceptions called out below.
>
> **What exists in code today (verified 2026-05-30):**
>
> | Surface | Today's reality | Cite |
> |---|---|---|
> | `OpRecord` enum tail | ends at `GitCheckpoint`; **no** `RemoteThreadUpdate`/`RemoteThreadDelete`/`UndoRecoveryUpdate` variants (grep finds these names only in *this doc*) | `oplog/src/oplog/oplog_types.rs:223` |
> | `OpRecord::Fork` | `{ from, new_state }` — **ref-blind** (no published thread/HEAD identity) | `oplog_types.rs:39` |
> | `OpRecord::Collapse` | `{ sources, result }` — **ref-blind** (carries target, not which ref it published) | `oplog_types.rs:41-44` |
> | remote-thread / undo-recovery writes | `set_remote_thread`/`delete_remote_thread`/`set_undo_recovery` write the ref **directly** (`lock_refs()` + `write_string`/`remove_file`), **no `OpRecord` appended** | `refs_manager.rs:261`,`:284`,`:242` |
> | second ref backend (`PgRefBackend`) | the hosted/server `RefBackend` impl publishes thread/marker/HEAD via its **own** SQL `pool.begin()…tx.commit()` over the `refs`/`heads` tables, writing **no** oplog record in that tx — a second raw publish entirely separate from the file path's `update_refs_with_lock` temp→rename | `pg_refs.rs:35`,`:324`,`:328` |
> | `cmd_fork` / `cmd_collapse` ordering | **publish ref before** appending the oplog record (phase-5-before-phase-4); `cmd_fork` *also* passes `record_fork`'s args **reversed** — `record_fork(from, new_state)` (`oplog_records.rs:113`) vs the call `record_fork(&new_state.change_id, &source_state.change_id)` (`fork.rs:94-95`), so the source is persisted as the fork *result* | `fork.rs:74-95`, `collapse.rs:99-113`, `oplog_records.rs:113` |
> | read path | the ten `RefManager` read methods touch raw storage directly; **no** `reconciled_load` primitive, **no** `RefReconciler` trait | `refs_manager.rs:114`–`:327` |
> | commit dedup | only the **window-bounded** `record_batch_scoped_if_no_transaction`; **no** unbounded `transaction_id` index | `oplog_core.rs:281` |
>
> Everything else this spike describes as a "chokepoint," "watermark," "primitive,"
> or "invariant that holds universally" is **proposed design**, to be built per
> §6/§7 — not an account of shipped behavior.

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
- **The oplog append is the SOLE commit point; the canonical ref is a
  materialized view, not the commit.** Today's `capture` publishes the ref
  *before* the oplog append (`repository_snapshot.rs:241-250` then `:252`), and
  ref readers are **lock-free** (`refs_head.rs:22-41`, `refs_manager.rs:129-135`)
  — so a crash between the two leaves a reader-visible ref with no undo record.
  The fix: a mutation is committed iff its `TransactionCommit` oplog entry is
  durable; ref publication (temp→rename, `refs_transactions.rs:230`) moves
  **after** the commit as a deterministic, idempotent materialization — the
  canonical ref is a *cache* of the committed oplog. **Correctness rests on
  per-read reconciliation, the universal rule: every ref read reconciles against
  the oplog at read time** — funnelled through **one internal `reconciled_load`
  primitive that is the sole path for LOGICAL READS (reads that serve a ref value
  to a caller) to touch raw ref storage** (the maintenance path `pack_refs` touches
  the raw loaders directly to compact storage but serves no logical value and is an
  explicit, reasoned exemption, cid 3328894984), so all **ten** public read methods
  (`read_head` `:114`, `get_thread` `:129`, `get_marker` `:185`, `get_undo_recovery`
  `:252`, `get_remote_thread` `:256`, `list_threads` `:178`, `list_markers` `:230`,
  `list_remotes` `:305`, `list_remote_threads` `:312`, `resolve` `:327`, all in
  `refs_manager.rs`) obtain ref data **only** by calling it, and the reconciliation
  lives *inside* it — reached via a `RefReconciler` trait defined in `refs` and
  injected from `repo`, so `refs` keeps no `oplog` dep. **Reconciliation scope is a
  property of the ref class (r10, cid 3328894983): local refs — `HEAD`,
  undo-recovery, beside the per-worktree HEAD (`refs_storage.rs:69-93`) — reconcile
  within this `op_scope`; shared refs — thread, marker, remote-thread, under the
  shared ref root (`refs_storage.rs:50-67`) — reconcile GLOBALLY across all lanes,
  since one file serves every worktree** (applying the `op_scope` filter to a shared
  ref is itself a correctness bug — it would miss a co-tenant's committed-but-
  unpublished shared write; r5's filter is scoped down to the local classes only).
  Because the raw-storage loaders are reachable from a logical read **only** from
  inside the primitive, every present reader **and any future `RefManager` read
  method** reconciles automatically — there is no reader enumeration to keep
  complete. (Placing the hook at the bypassable `Repository` accessors, or
  enumerating a *subset* of read methods as r6 did — it covered only
  `read_head`/`get_thread`/`get_marker` and left the four `list_*`, both remote,
  undo-recovery, and `resolve` readers observing stale state, cid 3328832780 —
  leaves the invariant with holes.) This
  holds for **every reader path, every handle age, every crash timing** —
  crucially the daemon's **long-held `Arc<Repository>`**
  (`local_daemon.rs:330`) that **never re-passes `Repository::open`**
  (`repository.rs:594`), the case an open-time pass structurally cannot reach
  (cid 3328112197). "Recover at open" is kept only as an **eager optimization**,
  not the guarantee; the hot path stays cheap via an O(1) oplog-generation
  (`head_id`, `packed_oplog.rs:26`,`:55`) check — a **watermark of *fully-
  materialized* committed batches, split by ref class** (a per-`op_scope` cell for
  local refs, a global cell for shared refs, r10), advanced only after a lagged read
  materializes a crashed batch's *every* ref of that class (batch-atomic, r8 cid
  3328853451), never on a partial single-ref reconcile, so the gate cannot
  short-circuit a batch sibling for any read shape (point, list, remote) — full
  reconcile only on the rare lag. **And the commit is deduplicated by an *unbounded, indexed*
  `transaction_id` lookup, not the window-bounded
  `record_batch_scoped_if_no_transaction` (which only scans a caller-supplied
  window — the rebase caller passes `64` and documents that aging past it
  duplicates the batch, `rebase_ops.rs:192-202`)** — so a crash-retry at *any*
  later time is exactly-once. Per-read reconciliation (read side) + the unbounded
  index (write side) make "committed" ⇔ "oplog entry exists" hold universally —
  across reader path, handle age, and retry timing. See §2.2 + the §2.4
  crash/retry-coverage proof — the single most load-bearing correction in the
  spike.
- **(PROPOSED, r9) Every ref *class* a reader resolves would have committed oplog
  records once the r9 design lands — closing the last two direct-write exceptions
  (cid 3328869364). This is spike design, not shipped: today's tree still writes
  these two classes directly (see the EXISTING-vs-PROPOSED note above).** Today,
  `set_remote_thread` (`refs_manager.rs:261`), its removal path
  `delete_remote_thread` (`:284`), and `set_undo_recovery` (`:242`) wrote their
  refs **directly** (`lock_refs()` + `write_string`/`remove_file`) with **no**
  `OpRecord` appended — the `OpRecord` enum (`oplog_types.rs:16`) had variants for
  thread/marker/HEAD but none for remote-thread or undo-recovery — so reconciling
  `get_remote_thread`/`list_remote_threads`/`list_remotes`/`get_undo_recovery` was
  *vacuous*: nothing in the tail to reconcile against, and after an oplog-only
  commit + crash those refs could not be re-derived. The r9 design **proposes**
  three net-new committed `OpRecord` variants
  (`RemoteThreadUpdate`/`RemoteThreadDelete`/`UndoRecoveryUpdate`, to be appended
  at the enum tail — today the tail ends at `GitCheckpoint`, `oplog_types.rs:223`)
  and routing those setters through the oplog-as-sole-commit path, so that **once
  built** the all-ten reconciliation guarantee holds **literally** — there would be
  no ref class whose read reconciles against an empty tail. **Until then, the tree
  has no such variants and the two setters still write directly.**
  See §2.2 "Remote-thread and undo-recovery writes are oplog-committed too."
- **(PROPOSED, r11) The write side becomes a structural chokepoint too — every ref
  publication preceded by a ref-carrying committed record (cid 3328926767). This is
  spike design, not shipped.** The r9 design closes
  the last writers that recorded *nothing*; the r11 design closes the last writers
  that record the *wrong shape* or in the *wrong order* — the write-side mirror of
  r7's read chokepoint. Two residual holes exist in today's tree, verified in real
  code (2026-05-30): **(a)
  ordering** — `cmd_fork` (`fork.rs:74-92`) and `cmd_collapse` (`collapse.rs:99-108`)
  **publish** the thread/HEAD ref *before* appending the oplog record (`record_fork`
  `fork.rs:94`, `record_collapse` `collapse.rs:112` run AFTER the
  `update_refs`/`write_head`/`set_thread` publish) — the phase-5-before-phase-4
  ordering §2.2 forbids; **and `cmd_fork` passes the `record_fork` args reversed**
  (r15) — `record_fork(from, new_state)` (`oplog_records.rs:113`) names the source as
  `from`, but the call passes `(&new_state.change_id, &source_state.change_id)`
  (`fork.rs:94-95`), persisting the source as the fork *result*; **(b) completeness** — `OpRecord::Fork { from, new_state }`
  (`oplog_types.rs:38`) and `Collapse { sources, result }` (`:40`) do **not** carry
  the *published ref identity* (which thread name, or detached HEAD), so an
  oplog-first fork/collapse that crashes pre-publish cannot know **which** ref to
  materialize on replay — the record is unreplayable for ref purposes. The fix is the
  write analog of r7: a single **write chokepoint** — one commit-then-publish
  primitive `commit_and_publish(op_records: &[OpRecord], ref_updates: &[RefUpdate])` (the
  canonical publish, temp→rename `refs_transactions.rs:230`, reachable **only** through
  it) that appends the **caller-supplied** record **batch** `op_records` (phase 4, the
  commit point) *before* publishing the atomic `ref_updates` batch (phase 5) — and the
  ref batch is **published whole, never split**. It takes a **batch** rather than a
  single record (r16) because some atomic ref batches back **multiple** records: a
  thread-rename publishes new-thread + old-thread-delete (± a HEAD move) in **one**
  atomic `update_refs` vector (`thread.rs:3074-3100`) while the oplog records a **batch
  of two** via `record_thread_rename` → `ThreadCreate` + `ThreadDelete`
  (`oplog_records.rs:96-110`). A one-`OpRecord` chokepoint would either **drop** a
  backing record for one published ref or **split** the atomic batch into two publishes
  (breaking atomicity); the batch signature represents the multi-record case faithfully —
  single-record ops (fork, collapse) pass a one-element batch, multi-record ops
  (thread-rename) pass their full batch with the matching atomic ref vector. It **takes**
  the caller's records rather than synthesizing them (r15): the generic `RefManager` write
  methods see only ref identity + target, not the `sources`/`from` the `Collapse`/`Fork`
  variants need — so the semantic callers (`cmd_fork`/`cmd_collapse`) build their full
  record and hand it in, while a plain ref edit passes a one-element generic ref-update
  record (the thin `set_thread`/`write_head`/`set_marker` wrappers build it for the
  caller). All
  publication still funnels through the one primitive, so the **46 direct ref-write
  call sites** (verified 2026-05-30; the same enumeration r7 did for readers) inherit
  correct ordering + atomicity **by construction**, and no production path publishes a
  ref without a preceding, replayable, ref-identifying record. The published thread name + HEAD are added
  **directly to the existing `Fork`/`Collapse` variants** — `Fork { from, new_state }`
  gains the published thread name + HEAD it set; `Collapse { sources, result }` gains the
  published ref identity (thread name or detached-HEAD marker) — mutated **in place**, not
  via new `…V2` discriminants. **heddle is pre-1.0 with no users and no production oplogs**,
  so the oplog format is broken freely as a clean in-place change: changing these variants'
  msgpack payload under their existing discriminant index means any old dev-only logs no
  longer deserialize, and that is **fine** — there is nothing to migrate. **No migration shim,
  no compat shim, no versioned `…V2` variants** (that compatibility-preserving escape is the
  no-backcompat stance's opposite and is *not* taken here). So **after the proposed work lands**, "zero
  ref-write paths without a committed ref-carrying record" becomes **structural on
  BOTH sides** — write chokepoint (record before publish) + read chokepoint (reconcile
  on read). Today neither chokepoint exists; both are impl-epic deliverables (§6/§7).
  **(r17) Two refinements make the write side airtight: (a) full-batch-record-coverage —
  the record batch must back *every* ref in the published ref batch, the gap exposed by
  attached-HEAD thread-rename, whose third `RefUpdate::Head` (`thread.rs:3090-3099`) has no
  record because `record_thread_rename` emits only two (`oplog_records.rs:96-110`), cid
  3329019021; and (b) the closure is *structural*, not a writer count — the raw publish is
  private with `commit_and_publish` its sole caller, so the CAS/create wrappers
  (`set_thread_cas`/`set_marker_cas`/`create_marker`) and every future writer are covered
  by construction, the write-side mirror of r7's single-`reconciled_load` read closure, cid
  3329019023.**
  **(r18, cid 3329052679) But r17's single-sole-writer closure seals only the FILE backend.**
  The raw publish r17 privatized is the file path's temp→rename (`refs_transactions.rs:228-256`);
  there is a SECOND ref backend — `PgRefBackend` (`pg_refs.rs:35`, the hosted/server impl of the
  `RefBackend` trait, `ref_backend.rs:15`) — whose `update_refs` (`pg_refs.rs:324`) publishes
  thread/marker/HEAD changes directly with SQL (`pool.begin()…tx.commit()`, `:328`), a publish the
  file seam never touches. So r18 lifts `commit_and_publish` from a repo/oplog-layer free function
  to a **`RefBackend`-trait method both backends implement**, each with a *private* raw publish
  behind it — the single-sole-writer invariant restated PER BACKEND. The two earn atomicity by
  different native mechanisms: the **file** backend via oplog-append-then-publish + per-read
  reconciliation (record and publish are two durable subsystems with a crash window — the whole
  r4–r17 design), the **Postgres** backend via ONE SQL transaction that inserts the ref-carrying
  oplog record(s) AND the ref/head updates together (`PgOpLogBackend` shares the same `PgPool`,
  `pg_oplog.rs:40`,`:259`) — native ACID atomicity, so it needs NO temp→rename, NO oplog-replay
  reconciliation, NO watermark. Lifting the seam to the trait is made possible by the same
  dependency-inversion the read side already uses (a `refs`-defined trait whose `oplog`-backed
  impl is injected from `repo`), which revises r17's "the write primitive consumes an `oplog` type
  so it must sit one layer up" — that very placement is what left the Postgres publish uncovered.
  See §2.2 "The write chokepoint" + §2.4.
- **Nesting = enroll-into-outermost (savepoint) by default; eager-commit only
  when an effect must be visible to another process before the outer commit**
  (the #251 reserve). This is a **type-level split**, not a runtime const:
  savepoint ops implement `DeferredMutation` (opt-in, no blanket impl), eager
  ops implement `EagerMutation` whose only method *returns* the compensator;
  `Tx::enroll` is bounded to the former and `Tx::enroll_eager` to the latter, so
  enrolling an eager op without a compensator is a **compile error** — no
  `COMMIT_KIND` const, no release-build-eliding `debug_assert!`. See §3.3.
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

### 1.2 — Oplog: the append, and a *window-bounded* idempotent append

- `OpLog::record_batch_scoped(ops, scope)` — `oplog/src/oplog/oplog_core.rs:236`
  — takes the oplog `write_lock()` (`:66`, `:245`), reloads fresh from disk
  (`:247`, to catch other processes), `packed.append(new_entries)` (`:256`),
  `packed.save()?` (`:257`). **`packed.save()` is the durable append.**
- `OpLog::record_batch_scoped_if_no_transaction(ops, scope, transaction_id,
  recent_window)` — `oplog_core.rs:281` — is a **window-bounded** atomic dedup:
  under the held write lock (`:292`) it scans **only the most recent
  `recent_window` batches** — `collect_batches_scoped(recent_window, …)`
  (`:295`) — for an `OpRecord::TransactionCommit { transaction_id, op_count }`
  (`oplog_types.rs:84`) matching `transaction_id`, returns `Ok(None)` if found,
  else appends (the heddle#198 r4 fix — comment at `oplog_core.rs:263-280`). It
  is exactly-once **only inside that window**: the sole production caller,
  `flush_rebase_batch` (`rebase_ops.rs:197-202`), passes `64` and its own comment
  concedes "ageing past it is acceptable because the worst-case outcome is a
  duplicate batch" (`rebase_ops.rs:192-196`). **So this helper is the right
  primitive for the immediate-retry race it was built for, but it is NOT the
  primitive's linearization point** — a delayed crash-retry after >`recent_window`
  intervening batches would scan past the prior `TransactionCommit` and append a
  *second* one for the same transaction. The primitive's exact-once commit
  therefore needs an **unbounded, indexed `transaction_id` → committed-index
  lookup** (§2.2 "Idempotency of the commit"), not a windowed scan. The existing
  helper remains useful for the bounded rebase path; the primitive does not
  inherit its window.

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

    /// Forward, staged, fallible side effects that are NOT yet visible to
    /// any other reader: object-store puts (orphan until referenced), FS
    /// temp writes, and ref temp writes — `write_string_temp`
    /// (`refs_transactions.rs:219-224`) WITHOUT the canonical temp→rename
    /// publish (`refs_transactions.rs:230`). MUST NOT rename a ref into its
    /// canonical path and MUST NOT append to the oplog — both happen at/after
    /// the executor's single commit step (§2.2). Every effect performed here
    /// MUST be paired with a rewind recorded into `tx` (see `Tx::on_rewind`).
    fn apply(&mut self, tx: &mut Tx<'_>) -> Result<StagedCommit<Self::Output>>;

    /// Undo whatever THIS op's `apply` staged, given the ledger captured at
    /// apply time. Called in reverse order on any pre-commit failure or
    /// panic-unwind. MUST be idempotent (may be invoked after a partial
    /// apply) and MUST undo ONLY what this invocation created — never
    /// pre-existing user state (the #302 r4 lesson, §5.2). Because `apply`
    /// only writes temp files (never publishes a canonical ref), the rewind
    /// is "unlink the temp files I wrote" — it never has to roll back a
    /// reader-visible ref, because no reader-visible ref was ever written
    /// pre-commit.
    fn rewind(&mut self, ledger: &RewindLedger) -> Result<()>;
}

// NOTE: there is deliberately no `COMMIT_KIND` associated const. Savepoint
// vs. eager is a *type-level* split (`DeferredMutation` vs `EagerMutation`,
// §3.3), not a runtime value the executor branches on — a runtime const that
// only a `debug_assert!` guards would vanish in release builds and let an
// eager op be enrolled through the savepoint path with no compensator.

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

### 2.2 — The commit point: oplog append is the SOLE commit; refs are a materialized view

The issue says "commit at the oplog-append linearization point." The real
workhorse capture today does the *opposite* — it publishes the ref **before**
the oplog append. In `snapshot_with_attribution_profiled`
(`repository_snapshot.rs:52`) the order is:

1. `put_state` + `flush_snapshot_write_batch` — `:224-225` (reversible: orphan).
2. fault-injection checkpoint `snapshot_after_state_before_ref` — `:233`.
3. **ref write** `set_thread` / `write_head` — `:241-250`.
4. **oplog** `record_snapshot` — `:252`.

Step 3 (`set_thread`/`write_head` → `update_refs` → `update_refs_with_lock`,
`refs_transactions.rs:103`) **publishes** the ref by `std::fs::rename`-ing a
temp file onto the canonical path + `sync_directory` (`refs_transactions.rs:230`,
`:235`). The crucial fact that breaks the issue's framing: **ref readers are
lock-free.** `read_head` → `read_head_state` reads the HEAD file directly with
no `lock_refs()` (`refs_head.rs:22-41`); `get_thread`/`get_marker` read the ref
file directly and fall back to `PackedRefs::load`, also un-locked
(`refs_manager.rs:129-135`, `:185-191`). So the instant the rename at
`refs_transactions.rs:230` lands, *any* concurrent process resolving that ref
sees the new value — there is no lock a reader is blocked on.

That makes the naive fixes both wrong:

- **"Publish the ref, then append the oplog" (today's capture order).** A crash
  between step 3 and step 4 leaves a **reader-visible ref with no oplog entry** —
  committed-looking state that is *not undoable*. This is exactly the window the
  R7 SIGKILL test pins (`cli/tests/cli_integration/fault_injection.rs:157-244`:
  the invariant is the *ref* didn't advance). It directly violates
  `committed ⇔ oplog entry exists`.
- **"Append the oplog, then publish the ref, both inside `apply`."** A crash
  after the append but before the rename leaves an oplog entry with no published
  ref. Without a recovery rule that re-publishes, the ref is permanently behind
  the committed log — the inverse violation.

Neither ordering, on its own, holds the invariant against lock-free readers +
temp→rename apply. The fix is to stop treating the canonical ref as the commit
at all:

> **The oplog append is the SOLE commit point.** A mutation is committed iff its
> `TransactionCommit` marker is durable in the oplog. **Ref publication is a
> deterministic, idempotent *post-commit* materialization** — the canonical ref
> is a *cache / materialized view* of the committed oplog, never the source of
> truth. A canonical ref is only ever renamed into place (a) by the executor
> *after* the oplog commit, or (b) by **per-read reconciliation** (§2.2
> "Reader model") lazily re-publishing the committed target. It is **never**
> written pre-commit. Therefore "committed" ⇔ "oplog entry exists," and a
> published (new-valued) ref always has a backing committed entry — and, because
> every *read* reconciles the ref against the oplog before trusting it, a reader
> never treats a lagging cache as authoritative either.

Concretely the canonical order the executor enforces:

| Phase | Domain | What is written | Reader-visible? | Rewind / recovery |
|---|---|---|---|---|
| 1. stage object(s) | object store | state blob (`put_state`, `repository_snapshot.rs:224`) | no (orphan until a ref points at it) | no-op rewind; `gc` reclaims |
| 2. stage FS | filesystem | temp files only | no (temp paths) | executor unlinks temp files |
| 3. stage refs | refs | **temp files only** (`write_string_temp`, `refs_transactions.rs:219-224`); NO canonical rename | **no** (canonical path untouched) | executor unlinks temp files |
| **4. COMMIT** | **oplog** | `TransactionCommit` + the state `OpRecord`s, deduplicated by an **unbounded indexed `transaction_id` lookup** (§2.2 "Idempotency of the commit" — *not* the window-bounded `record_batch_scoped_if_no_transaction`) | the commit itself | none past here — it happened |
| 5. publish refs | refs | temp→**rename**+`sync_directory` (`refs_transactions.rs:230`,`:235`) | **yes** | idempotent; re-derivable from phase-4 records |

This **splits** the existing `update_refs_with_lock` (`refs_transactions.rs:103`)
into its plan-validate-and-stage half (`:111-224`, which only writes temp files)
and its publish half (the rename loop, `:228-256`). Phase 3 runs the first half;
phase 5 runs the second. The CAS *validation* (`matches_expectation`, `:127`,
`:167`, `:199`) still happens in phase 3 against the on-disk value, so a stale
expectation fails before commit; the rename it gates is simply deferred to phase 5.

**Crash table — what is on disk at a crash in each phase, and how recovery
restores `committed ⇔ oplog entry exists`:**

| Crash point | On disk | Committed? | Recovery action | Invariant |
|---|---|---|---|---|
| during/after ph1 | orphan state blob; refs at OLD value; no oplog entry | **no** | `gc` reclaims the orphan; nothing else | holds (no entry ⇒ not committed; ref still OLD) |
| during/after ph2–3 | temp files at `.tmp-*` paths; canonical refs at OLD value; no oplog entry | **no** | unreferenced temp files swept by gc / a startup tmp-sweep (the same orphan-`.tmp-` shape `transaction_replay` handles for the sentinel dir, `transaction_replay.rs` ¶3); canonical refs untouched | holds (no entry; no reader ever saw a temp path) |
| during ph4 | oplog append is itself a `packed.save()` = write-temp+atomic-rename inside the oplog; the entry is either absent or fully present — never torn | atomic boundary | if absent ⇒ treat as ph3; if present ⇒ treat as ph5 | holds either way |
| after ph4, before/during ph5 | oplog entry **present**; canonical ref still at OLD value (rename not yet done) | **yes** | **the next reader reconciles** — its read folds the committed oplog tail, sees the committed target is newer than the lagging canonical value, and resolves the committed value (lazily re-publishing the ref). The `open`-time pass is an eager fast-path, not the guarantee | holds (entry exists ⇒ committed; the read never trusts the lagging cache) |
| after ph5 | oplog entry present; canonical ref at NEW value | **yes** | reconciliation is a no-op (cheap generation check sees no lag; ref already at target) | holds |

**The write chokepoint — every ref publication is preceded by a ref-carrying
record (r11, cid 3328926767), the write-side mirror of the read chokepoint.** The
phase order above is a *contract*; this round makes it **structural**, exactly as r7
did for reads. The motivating bug is not unique to capture (`repository_snapshot.rs:241-250`
then `:252`). Two production writers exhibit the **identical** phase-5-before-phase-4
reversal, and their records are not even replayable for ref purposes (and a **third**,
thread-rename, motivates the batch shape of the chokepoint — see below):

- **`cmd_fork` (`fork.rs:74-92`).** When a name is given it publishes a thread + HEAD
  via `repo.refs().update_refs([RefUpdate::Thread, RefUpdate::Head])` (`:85`); detached,
  via `repo.refs().write_head(..)` (`:88-90`) — and **only then** calls
  `repo.oplog().record_fork(&new_state.change_id, &source_state.change_id)` (`:94-95`).
  Publish (phase 5) runs *before* the record (phase 4) — **and the call's arguments are
  reversed (r15).** `record_fork(from, new_state)` (`oplog_records.rs:113`) names the
  source as `from` and the fork result as `new_state`, but the call passes `new_state`
  as `from` and `source_state` as `new_state` — so today's `OpRecord::Fork` persists
  the source as the fork *result* and the result as its origin (the two positions are
  swapped). This is the concrete reason the retrofit's **published-ref fields**, not
  the from/new_state positional pair, must be the authoritative replay/materialization
  target (the variant audit + impl item below).
- **`cmd_collapse` (`collapse.rs:99-108`).** Publishes the thread ref
  (`set_thread`, `:101`) for an attached HEAD or detached HEAD (`write_head`, `:104`),
  **then** `record_collapse(&sources, &new_state)` (`:112-113`). Same reversal.

And both records are **ref-blind**: `OpRecord::Fork { from, new_state }`
(`oplog_types.rs:38`) carries neither the published thread name nor the HEAD it set;
`OpRecord::Collapse { sources, result }` (`:40`) carries the target state (`result`)
but **not which ref it published** — the attached-vs-detached branch (`collapse.rs:99-108`)
means "which thread, or HEAD-detached" is *not* derivable from the record. So even if
the ordering were fixed, an oplog-first fork/collapse that crashed before phase 5
could not know **which** ref to materialize on replay — the record is unreplayable.

**A third writer — thread-rename — motivates the *batch* shape of the chokepoint
(r16).** `cmd_thread_rename` (`thread.rs:3061-3116`) publishes its refs as **one
atomic `update_refs` batch** — create the new thread, delete the old thread, and (when
HEAD was attached to the renamed thread) move `HEAD` — all in a single `updates` vector
(`thread.rs:3074-3100`), then records via `record_thread_rename` (`thread.rs:3101-3102`),
which appends a **batch of two `OpRecord`s** — `ThreadCreate { name: new, … }` +
`ThreadDelete { name: old, … }` (`oplog_records.rs:96-110`). So this atomic ref batch is
backed by **multiple** records, not one. A chokepoint whose signature took a single
`OpRecord` could not represent this faithfully: it would either **drop** a backing
record (publish create+delete but commit only one of the two) or force the caller to
**split** the one atomic ref batch into two separate `commit_and_publish` calls —
re-introducing exactly the non-atomic multi-publish the primitive exists to forbid (a
crash between the two publishes leaves the new thread visible and the old thread still
present). The chokepoint must therefore accept a **record batch** alongside the atomic
ref batch (next).

**But the batch *signature* is necessary, not sufficient — the records must *span* every
published ref, and thread-rename's attached-HEAD case is where they do not (r17, cid
3329019021).** When the renamed thread IS the current attached HEAD, `cmd_thread_rename`
pushes a **third** update onto the vector — `RefUpdate::Head` re-attaching HEAD from the
old thread name to the new (`thread.rs:3090-3099`) — so the published batch carries
**three** refs: new-thread create, old-thread delete, **and** the HEAD move. But
`record_thread_rename` (`oplog_records.rs:96-110`) appends only **two** records
(`ThreadCreate` + `ThreadDelete`) in *every* case — there is **no record for the HEAD
move**. So an attached rename routed through `commit_and_publish` would publish a HEAD ref
whose backing record is absent: a crash after phase 4 (commit) and before phase 5
(publish) leaves reconciliation with **no ref-carrying record for the HEAD move**, and the
read side cannot re-materialize the HEAD re-attach the batch committed — the atomic batch
it published cannot be fully materialized on replay, and `committed ⇔ a ref-carrying oplog
record exists` fails for that one ref. The rule the chokepoint must enforce is therefore
not merely "take a batch" but **full coverage**:

> **Full-batch-record-coverage invariant (r17): the record batch handed to
> `commit_and_publish(op_records, ref_updates)` must back EVERY ref in `ref_updates`** —
> the set of refs named across `op_records` must equal the set of refs in `ref_updates`,
> so no published ref lacks a replayable record. As a count: the number of distinct refs
> published equals the number of distinct refs backed by a committed record. A
> `commit_and_publish` whose `op_records` name *fewer* refs than `ref_updates` publishes
> is a chokepoint *with a hole* — the very state the write side exists to forbid, merely
> relocated from "no record at all" to "no record for *this* ref."

For thread-rename this means the **attached-HEAD case must commit a three-record batch**:
`record_thread_rename`, when HEAD was attached to the renamed thread, must append a
**HEAD-move record** alongside the create + delete — either by adding a ref-carrying HEAD
record (one that names HEAD's new attached target, the new thread) to the rename's record
batch, or by **extending the rename record shape** to carry the attached-HEAD move it
published. The detached / HEAD-unaffected case keeps its two-record batch (no HEAD ref
published ⇒ no HEAD record owed). Either way the invariant is the same: per batch, record
count tracks ref count, and the attached-HEAD `RefUpdate::Head` (`thread.rs:3090-3099`)
gets its backing record.

The fix mirrors r7 on the write side. Just as r7 made raw ref *loaders* unreachable
from a logical read except through `reconciled_load`, r11 makes the raw ref *publish*
(the temp→**rename**+`sync_directory`, `refs_transactions.rs:230`,`:235`) unreachable
except through **one write chokepoint** that **always appends a ref-carrying record
first**:

> **Write chokepoint (the invariant): no canonical ref is published except by a
> primitive that has *first* appended a durable, ref-identifying oplog record — and an
> atomic ref batch is published with its *full* backing record batch, never split, where
> "full" means the records back EVERY ref in the batch (full-batch-record-coverage, r17,
> cid 3329019021 — the attached-HEAD thread-rename above is the case that fails it without
> the added HEAD-move record).** The
> seam is **one commit-then-publish primitive** —
> `commit_and_publish(op_records: &[OpRecord], ref_updates: &[RefUpdate])` — that appends
> **all** of `op_records` (phase 4, the commit point) and *then* publishes the atomic
> `ref_updates` batch (phase 5, temp→rename) **as one unit, without splitting it**, in
> that order; the bare temp→rename publish is a *private* sub-step below it, never
> callable on its own. **It takes a record *batch*, not a single `OpRecord` (r16)** —
> because some atomic ref batches are backed by **multiple** records: thread-rename
> publishes new-thread + old-thread-delete (± a HEAD move) in one atomic `update_refs`
> vector (`thread.rs:3074-3100`) while `record_thread_rename` records a **batch of two**
> (`ThreadCreate` + `ThreadDelete`, `oplog_records.rs:96-110`). A single-`OpRecord`
> signature would force such a writer to drop a backing record or split the atomic batch
> into multiple publishes (breaking atomicity); the batch signature lets the atomic ref
> batch and **all** its backing records commit-then-publish together. Single-record ops
> (fork, collapse, plain ref edits) pass a **one-element** batch; multi-record ops
> (thread-rename) pass their full record batch with the matching atomic ref vector.
> **The primitive *takes* the caller's records rather than synthesizing them (r15)** —
> because the generic `RefManager` write methods (`set_thread(name, state)`,
> `write_head(head)`, `update_refs(&[RefUpdate])`) receive only ref *identity + target*,
> not the high-level operation data the extended variants carry: on the collapse path
> `set_thread`/`write_head` see only the target state, never the `sources` that
> `OpRecord::Collapse` needs; detached `fork`'s `write_head` sees neither the `from`
> nor any thread context. A write method that *synthesized* its own record from what
> it can see could only emit a generic `ThreadUpdate`/`Goto` — either **losing** the
> fork/collapse semantics the spike preserves, or emitting a **duplicate** generic
> record alongside the real one. So the records' *semantic content* is the caller's
> responsibility; the primitive's job is the *structural* guarantee — every published
> ref in the batch has a backing record committed before publish, and the batch is
> published atomically — which it enforces uniformly no matter which `OpRecord`s it is
> handed. A writer therefore *cannot* publish a ref (or a ref batch) without first
> committing the replayable record(s) that name every ref it publishes.

The callers split by whether they carry a high-level operation:

- **Semantic callers build their full `OpRecord`(s) and hand them in.** `cmd_fork`
  builds a one-element batch `[OpRecord::Fork { from: source_state, new_state, thread,
  head }]` — correct from/new_state order per the arg-reversal fix above, plus the
  published thread name (`None` when detached) and the HEAD it set — and the matching
  `RefUpdate::Thread`/`RefUpdate::Head`. `cmd_collapse` builds a one-element batch
  `[OpRecord::Collapse { sources, result, <published-ref discriminant> }]` (thread name
  or detached-HEAD marker) and the matching `RefUpdate`. **`cmd_thread_rename` builds a
  *two*-element batch** `[ThreadCreate { name: new, … }, ThreadDelete { name: old, … }]`
  (its existing `record_thread_rename` records, `oplog_records.rs:96-110`) for the
  detached / HEAD-unaffected case — **or a *three*-element batch** that adds a HEAD-move
  record when HEAD was attached to the renamed thread and the published vector therefore
  includes `RefUpdate::Head` (`thread.rs:3090-3099`), per the full-batch-record-coverage
  invariant (r17) — paired with the atomic `update_refs` vector of two-or-three
  `RefUpdate`s it already constructs (`thread.rs:3074-3100`), so the record count tracks
  the ref count. Each calls `commit_and_publish(op_records, ref_updates)`.
- **Plain ref edits (no high-level operation) pass a *one-element generic* ref-update
  record** — `[ThreadUpdate { name, state }]`, `[Goto { target }]`,
  `[MarkerCreate { name, state }]`, … — for which the ref identity + target *is* the
  whole semantic content, so the thin `set_thread`/`write_head`/`set_marker` wrappers
  build that generic record themselves and call `commit_and_publish` for the caller.
  These keep their existing call shape.

**The closure is STRUCTURAL — `commit_and_publish` is the sole caller of a *private*
raw publish — not a writer count (r17, cid 3329019023; the write-side mirror of r7's
read closure).** An earlier draft proved this by *counting*: a grep for four
canonical-publishing method names — `refs().{set_thread,write_head,update_refs,set_marker}`
— returned 46 non-test call sites, and the argument was "all 46 route through the
chokepoint." But a count over a hand-picked set of method *names* is exactly the wrong
frame, for the same reason r6's enumerated reader hook was: it is incomplete, and
silently so. Production publishes refs through **more** entry points than those four
names — the public **CAS wrappers** `set_thread_cas` (`thread.rs:1647-1650`) and
`set_marker_cas` (`grpc_remote/mod.rs:334-338`), the **create/delete** wrappers
`create_marker` (`grpc_remote/mod.rs:339`) and `delete_marker`, the `delete_*_cas`
siblings, and the r9 remote/undo setters — none of which the four-name grep counts.
(The grep was worse than incomplete: `set_marker` is not even a `RefManager` method —
markers publish via `set_marker_cas`/`create_marker`/`delete_marker` — so the audit
spent one of its four names on a non-writer while missing the real marker writers.)
Enumerating "…also CAS, also `create_marker`" just moves the hole: the *next* writer
(`delete_marker`, `set_remote_thread`, a not-yet-written sibling) is missed again. This
is r6's "the 3, then the 5, then the 7" reader drip, transposed to the write side — a
longer writer list is not a closed class.

The fix is the write-side mirror of r7. r7 did not enumerate readers; it made the raw
loaders **unreachable** from a logical read except through one `reconciled_load`. The
write side closes identically — and the code already hands us the convergence point.
**Every** standard public writer already funnels through a single internal seam,
`update_refs` (`refs_manager.rs:319`): `set_thread`/`set_thread_cas` (`:137-152`),
`write_head`/`write_head_cas` (`:118-127`), `set_marker_cas`/`create_marker` (`:193-208`),
and `delete_thread`/`delete_thread_cas`/`delete_marker`/`delete_marker_cas` (`:154-228`)
each construct a `&[RefUpdate]` and call `update_refs`, which calls `update_refs_with_lock`
(`refs_transactions.rs:103`, already `pub(super)`) whose temp→**rename** + `sync_directory`
loop (`:228-256`) is **the one raw publish**. The CAS/create wrappers Codex flagged do
**not** reach the raw publish independently — they reach it *via* `update_refs`. So the
seam below all of them is a single function, exactly as the two raw loaders sat below the
ten readers.

> **Single-sole-writer invariant (r17): there is exactly one raw ref publish, and
> `commit_and_publish` is its sole caller.** The low-level durable ref write — the
> temp→rename + `sync_directory` half of `update_refs_with_lock`
> (`refs_transactions.rs:228-256`) — is made **private/internal**, callable only from
> inside `commit_and_publish`. Every public writer family delegates through
> `commit_and_publish`, each constructing its own `OpRecord`(s) first: the plain
> `set_thread`/`write_head`/`update_refs` edits, the CAS wrappers
> `set_thread_cas`/`set_marker_cas`/`write_head_cas` (and the `delete_*_cas` siblings),
> the create/delete wrappers `create_marker`/`delete_marker`, and — once r9 routes them
> off their current direct `write_string`/`remove_file` path (`refs_manager.rs:242`,`:261`,
> `:284`) — `set_remote_thread`/`delete_remote_thread`/`set_undo_recovery`. None reaches
> the raw publish on its own.

**Why this is a *proof* and not a longer list.** Because the raw publish is private and
`commit_and_publish` is its only caller, *any* writer — the CAS/create entry points Codex
named, the `delete_marker`/remote/undo siblings it did not, or one written next year —
**must** go through the record-committing chokepoint to publish a ref at all: there is no
raw publish to call around. The set of covered writers is therefore "all of them, present
and future," with no enumeration to keep complete — the exact write-side equivalent of
r7's "the 92nd reader is covered because raw storage has no other logical-read entry." A
one-line conformance check (the analog of the read-side one) asserts the raw publish has
no caller but the primitive, so a "47th writer" that tried to bypass it would have no
publish to compile against. The 46-count survives only as an *illustration* that the
known writers already converge — it is no longer load-bearing, because the bound is now
the visibility boundary, not the count.

**Two backends — the seam is on the `RefBackend` trait, not just `RefManager` (r18, cid
3329052679).** Everything above privatized exactly ONE raw publish: the file path's
temp→rename + `sync_directory` (`refs_transactions.rs:228-256`). But `RefManager` is only one
implementor of the backend interface. The `RefBackend` trait (`ref_backend.rs:15`,
`RefBackend: CoreRefBackend<Error = HeddleError>`) has a second production impl —
`PgRefBackend` (`pg_refs.rs:35`), the hosted/server backend — and its publish path is
**entirely separate code**: `PgRefBackend::update_refs` (`pg_refs.rs:324`, the
`CoreRefBackend::update_refs` impl, `backend.rs:74`) applies every thread/marker/HEAD update
with SQL inside a single `pool.begin()…tx.commit()` (`pg_refs.rs:328`) against the
`refs`/`heads` tables — it never goes near `update_refs_with_lock` or the temp→rename. So a
closure that only makes the *file* temp→rename private leaves `PgRefBackend::update_refs` as a
**second public raw publish that can still emit a ref with no preceding ref-carrying record** —
the write class is closed for the file backend and wide open for the Postgres backend. A
per-`RefManager` chokepoint is a chokepoint with a whole *backend* outside it.

The fix is to state the single-sole-writer invariant **per backend** by lifting
`commit_and_publish` onto the `RefBackend` trait:

> **Single-sole-writer invariant, per backend (r18): each `RefBackend` impl has exactly one
> raw ref publish, private, with that backend's `commit_and_publish` its sole caller.**
> `commit_and_publish(op_records, ref_updates)` is a **method on the `RefBackend` trait**, not
> a free function over one concrete manager. Every backend implements it; every backend's bare
> publish — the file backend's temp→rename (`refs_transactions.rs:228-256`), the Postgres
> backend's SQL-tx-of-refs (`PgRefBackend::update_refs`, `pg_refs.rs:324`/`:328`) — is private
> and reachable only through it. There is no public writer, on either backend, that publishes a
> canonical ref without first committing a ref-carrying record.

**Each backend earns the invariant by its substrate's native atomicity mechanism — same
invariant, different proof per backend.**

- **File backend (`RefManager`) — oplog-as-commit + reconciliation (the r4–r17 design,
  unchanged).** The record append (to the file oplog) and the ref publish (temp→rename) are
  **two durable steps in two subsystems** with a crash window between phase 4 and phase 5.
  `commit_and_publish` orders append-then-publish; the residual lag (record durable, ref not yet
  renamed) is repaired by **per-read reconciliation** + the class-split watermark (§2.2 "Reader
  model," §2.4). This backend *needs* reconciliation precisely because it has no single durable
  transaction spanning both subsystems.
- **Postgres backend (`PgRefBackend`) — one SQL transaction, no reconciliation needed.** The
  oplog records and the refs/heads rows live in the **same database**: `PgOpLogBackend`
  (`pg_oplog.rs:39`) appends via `INSERT INTO oplog (…)` (`pg_oplog.rs:259`) over the same
  `Arc<PgPool>` (`pg_oplog.rs:40`) that `PgRefBackend` holds (`pg_refs.rs:36`). So
  `PgRefBackend::commit_and_publish` does, in **one** `pool.begin()…tx.commit()`, both the
  ref-carrying record `INSERT` **and** the ref/head updates that `update_refs` already applies
  (`pg_refs.rs:328`). Postgres's ACID transaction makes "committed ⇔ the record row exists" hold
  **atomically** — there is no crash window between record and publish, because they commit or
  roll back as one. The Postgres path therefore needs **NO temp→rename, NO oplog-replay
  reconciliation, NO watermark** — the file backend's entire crash-recovery apparatus is moot
  here. r18 routes `PgRefBackend::update_refs` through the seam: the existing SQL-tx-of-just-refs
  becomes the private raw publish, and `commit_and_publish` adds the record `INSERT` to that same
  transaction; no caller reaches the bare SQL publish, mirroring the file backend's
  private-temp→rename rule. (The test-only in-memory `MemRefBackend`, `backend.rs:127`, is
  non-durable and outside the durability argument.)

The writers that route through the chokepoint, **illustratively** (this table is coverage
illustration, not the proof — the proof is the visibility boundary above; the CAS/create
entry points cid 3329019023 flagged are folded in):

| File | Role of the ref write |
|---|---|
| `cli/.../commands/fork.rs` | thread+HEAD publish on fork (the ordering bug above) |
| `cli/.../commands/collapse.rs` | thread/HEAD publish on collapse (the ordering bug above) |
| `cli/.../commands/thread.rs`, `thread_cmd.rs` | thread create/start (incl. `set_thread_cas` `:1647-1650`), rename (incl. attached-HEAD move), HEAD moves |
| `cli/.../commands/clone.rs`, `actor_cmd.rs`, `attempt.rs`, `context/mod.rs`, `bridge.rs`, `undo_apply.rs`, `git_overlay_health.rs` | HEAD / thread / marker publishes |
| `cli/.../commands/remote/{mod,remote_ops}.rs` | remote-thread + HEAD publishes |
| `cli/src/bridge/{git_core,git_sync}.rs` | git-overlay HEAD/thread sync |
| `client/src/grpc_remote/{hydration,sync,mod}.rs` | hosted-sync ref materialization (incl. `set_marker_cas`/`create_marker` `mod.rs:334-339`) |
| `repo/src/repository_thread_materialize.rs` | thread-materialize HEAD/thread publish |

The op-descriptor model (r15) cleanly **separates the two concerns**: the primitive
guarantees the *structural* property — ordering (record before publish), atomicity, and
full-coverage (every published ref backed by a record, r17) — *universally*, for every
writer; each writer supplies the record's *content* (its own `OpRecord`). A semantic
writer (fork/collapse) hands in its rich variant; a plain ref edit hands in a generic
ref-update record; **both** route through the one primitive, so the by-construction
guarantee holds without the primitive ever needing context it cannot see. This is the
difference between an enumerated audit ("we fixed fork and collapse," "we also counted
CAS") and a closed class ("no path can publish without a record").

**Crate-dependency seam — why the seam can live on the trait (r18 revises r17).**
`commit_and_publish` must commit `OpRecord`s (an `oplog` type) *and* drive the canonical-ref
publish (a `refs` operation), but `refs` does **not** depend on `oplog`
(`crates/refs/Cargo.toml`). r17 read this as "the fusion point cannot live inside `refs` — it
would have to name `OpRecord` — so `commit_and_publish` sits at the `repo`/`oplog` layer as a
free function over `RefManager`, to which `refs` exposes only a private temp→rename sub-step."
That placement *did* fuse append-then-publish in one place — **but only for `RefManager`.** A
free function over the file manager is structurally incapable of being a chokepoint for the
*other* `RefBackend` impl (`PgRefBackend`, `pg_refs.rs:35`), whose publish is separate SQL
(`pg_refs.rs:324`/`:328`) it never calls — which is exactly the hole cid 3329052679 found. The
asymmetry claim ("the write primitive consumes an `oplog` type, so unlike `RefReconciler` it
cannot be inverted into `refs`") was the error: the record crosses the `RefBackend` boundary
through the **same dependency-inversion the read side uses** — a **`refs`-defined committer
trait** (the write-side dual of `RefReconciler`) whose `oplog`-backed impl is injected from the
`repo`/`oplog` layer (`crates/repo/Cargo.toml:22` oplog, `:24` refs) at `Repository`/server
construction. The `RefBackend::commit_and_publish` signature names that `refs`-defined trait,
**not** `OpRecord`; the concrete `OpRecord` batch (including the `Fork`/`Collapse` payloads
carrying `sources`/`from`, data `refs` does not own) is built and encoded at the `repo`/`oplog`
layer — where both crates are visible — and handed across opaquely. So `refs` still never names
`oplog`, *and* the seam lives on the trait both backends implement. Each backend's
`commit_and_publish` then fuses by its native mechanism: the **file** backend calls the injected
committer to append to the file oplog, then runs its private temp→rename
(`refs_transactions.rs:228-256`) — two subsystems, so the crash window is bridged by
reconciliation; the **Postgres** backend runs one SQL tx inserting the record row + the ref/head
rows (one subsystem ⇒ natively atomic, no reconciliation). The **per-backend** write-side
conformance check asserts each backend's bare publish has no caller but its own
`commit_and_publish`. A bootstrap path with no committer wired keeps today's plain-publish
behavior (used only before the oplog exists, mirroring the no-reconciler bootstrap read).

**Relationship to the `execute`/`Tx` model — one invariant, two embodiments.** The
chokepoint is the *lowest* enforcement layer, below `execute`. A writer reaches the
record-before-publish guarantee in one of two ways, both honoring phase-4-before-phase-5:

- **Standalone (un-migrated) writer** — the 46 sites today: the caller builds its
  record batch and calls `commit_and_publish(op_records, ref_updates)`, which **fuses**
  append-records then publish-refs in a single call (for a plain ref edit the thin
  `set_thread`/`write_head`/`set_marker` wrapper builds a one-element generic record and
  makes that call for it; fork/collapse hand in a one-element rich batch; thread-rename
  hands in its two-element `ThreadCreate`+`ThreadDelete` batch with the matching atomic
  ref vector). This
  is what closes the class for code that has *not* yet been migrated to
  `AtomicMutation`, exactly as `reconciled_load` reconciles for readers that were never
  migrated to `execute`.
- **Inside an `execute` transaction** — §2.1/§3.4: the op already yields its records
  via `StagedCommit { oplog: Vec<OpRecord> }` (§2.1), so the executor has the caller's
  descriptors in hand: ref writes **stage** the temp in phase 3, the executor appends
  **all** those records at the single phase-4 commit, then publishes **all** refs in
  phase 5. The append is batched across the transaction rather than per-write, but the
  record still precedes every publish — the same op-descriptor-into-commit shape as the
  standalone primitive, just batched. (The `execute`/`Tx` model is the spike's proposed
  migration target, not an existing abstraction in `refs`/`repo`.)

Either way, **every published ref in an atomic batch has a preceding ref-carrying
record committed in the same atomic publish** (a single-ref publish is the one-element
case), and the `OpRecord` variant that backs each publish must carry the ref it
publishes — which the audit below enforces.

**Audit: every ref-publishing `OpRecord` variant must carry the published ref
identity + target.** Reconciliation/replay can only re-materialize a published ref if
the record names *which* ref and its target value. The table below classifies every
publishing variant by **whether it exists in today's tree** and **what this spike
proposes** (`oplog_types.rs`). The "Status" column is the load-bearing distinction:
rows tagged **EXISTING** are in the current enum; rows tagged **PROPOSED** are net-new
or retrofitted impl-epic work this spike designs — they are *not* in the tree (grep
confirms the proposed variant names appear only in this doc):

| Variant | Status | Publishes | Carries ref identity + target? | Action |
|---|---|---|---|---|
| `Snapshot { new_state, prev_head, thread }` (`:18`) | EXISTING | thread + HEAD | ✅ `thread` name + `new_state` target | none — already correct |
| `Goto { target, prev_head }` (`:24`) | EXISTING | HEAD | ✅ HEAD is the unique implicit ref; `target` is its value | none — already correct |
| `ThreadCreate`/`ThreadCreate`/`ThreadUpdate`/`ThreadDelete` (`:29`,`:215`,`:33`,`:31`) | EXISTING | thread | ✅ `name` + `state` | none — already correct |
| `MarkerCreate`/`MarkerDelete { name, state }` (`:46`,`:47`) | EXISTING | marker | ✅ `name` + `state` | none — already correct |
| `Checkpoint { parent, state, thread }` (`:53`) | EXISTING | thread + HEAD | ✅ `thread` + `state` | none — already correct |
| `FastForward { source_thread, target_thread, pre_target_id, post_target_id }` (`:169`) | EXISTING | `target_thread` ref | ✅ `target_thread` + `post_target_id` — **the precedent**: heddle#99 r2 added `post_target_id` *specifically* so replay names the published ref *and* its target | none — already correct |
| `RemoteThreadUpdate`/`RemoteThreadDelete`/`UndoRecoveryUpdate` | **PROPOSED (r9)** — **do NOT exist** in the tree; enum tail ends at `GitCheckpoint` (`oplog_types.rs:223`); the corresponding writes (`set_remote_thread`/`delete_remote_thread`/`set_undo_recovery`, `refs_manager.rs:261`,`:284`,`:242`) currently write the ref **directly with no `OpRecord`** | remote-thread / undo-recovery | (designed to) ✅ remote+thread / recovery target | **BUILD** → add these three tail variants + route the three direct setters through the oplog-commit path so their reconciliation is non-vacuous |
| **`Fork { from, new_state }`** (`:39`) | **EXISTING, ref-blind → EXTENDED IN PLACE** | thread + HEAD (`fork.rs:85`,`:88`) | ❌ **no** published thread name, **no** HEAD | **BUILD** → extend the existing variant in place to `Fork { from, new_state, thread: Option<String>, head }` (published thread name, `None` for detached, + the HEAD it set), modelled on `ThreadUpdate`'s `{ name, state }` shape |
| **`Collapse { sources, result }`** (`:41-44`) | **EXISTING, ref-blind → EXTENDED IN PLACE** | thread *or* detached HEAD (`collapse.rs:101`,`:104`) | ❌ carries `result` target but **not** which ref it published | **BUILD** → extend the existing variant in place to `Collapse { sources, result, <published-ref discriminant> }` (thread name or detached-HEAD marker) |

So among the variants **in the tree today**, only **`Fork` and `Collapse`** lack ref
identity (every other *existing* publishing variant — including `FastForward`, which
already carries it — needs no change); and the **remote-thread / undo-recovery** publishing
variants **do not exist at all yet** — they are net-new spike-proposed work, not
"covered." The retrofit adds the ref identity by **mutating the existing `Fork`/`Collapse`
variants in place** — extra fields on the existing variant bodies (their existing
discriminant indices unchanged), modelled on the field shape of existing variants like
`ThreadUpdate { name, state }`. **heddle is pre-1.0 with no users and no production
oplogs**, so the oplog format is broken freely as a clean in-place change: changing the
`Fork`/`Collapse` payload shape under an unchanged discriminant index means any old
dev-only logs no longer deserialize those records, and that is **fine** — there is nothing
to migrate, and discardable dev logs are not a constraint. **No migration shim, no compat
shim, no versioned `…V2` variants.** (The compatibility-preserving alternative — keep the
old ref-blind variants and append new `…V2` discriminants so pre-existing entries still
deserialize — is exactly the kind of back-compat shim the pre-1.0 no-backcompat stance
rejects; it is *not* taken here.) This is still
**format-stability-sensitive** and reviewed as such (§6 O9) — not because old logs must
survive (they need not), but because the impl must land the new field shapes coherently
across every reader/writer of these variants. Once `Fork`/`Collapse` carry the published
ref, the chokepoint's phase-4 record is
replayable for *every* writer, and the §2.4 universal proof spans the write side too.

**Note the two different mechanisms, both fine pre-1.0.** The r9 remote/undo records
(`RemoteThreadUpdate`/`RemoteThreadDelete`/`UndoRecoveryUpdate`) are **net-new ref
classes** that never had a variant, so they are naturally **new tail variants**. The
`Fork`/`Collapse` records are **existing ref-blind variants** whose payloads this spike
extends, so they are **mutated in place**. New ref class ⇒ new variant; existing
ref-blind variant ⇒ edit in place. Both are clean format changes pre-1.0; neither needs a
compat shim or a versioned `…V2` escape.

**Reader model — per-read reconciliation (the universal correctness rule).**
"Materialize at open" cannot be the guarantee, because **not every reader opens
the repo per read.** The daemon builds its `Arc<Repository>` **once** at serve
time (`local_daemon.rs:330`, wrapping the `repo` passed into `serve` at `:257`)
and every handler reads refs off that **long-held** handle for the life of the
process (`GrpcLocalService.repo`, `grpc_local_impl/mod.rs:38`, borrowed via
`repo()` `:57-59`; e.g. `begin_transaction` reads `repo.head()` /
`repo.refs().get_thread(..)` at `transaction.rs:143-152`). That handle **never
re-passes `Repository::open`** (`repository.rs:594`), so an open-time materialize
pass — however well placed — structurally cannot repair a ref that goes stale
*after* the handle is already open: a concurrent CLI crash in the "after ph4,
before ph5" window would leave the daemon's already-open handle resolving the
stale canonical ref indefinitely. The guarantee must therefore live one seam
deeper — at the **read** itself:

> **Universal rule: a ref read reconciles against the committed oplog at read
> time; the *scope* of that reconciliation is a property of the ref class.** A
> reader NEVER treats a canonical ref as authoritative-committed unless its
> committing oplog entry exists; and if the committed oplog tail names a newer
> target for that ref than the canonical value (a publication not yet
> materialized), the read resolves the **authoritative value from the oplog** (and
> MAY re-publish the canonical ref lazily). So the read never trusts a *lagging*
> cache (oplog ahead of ref) and never trusts a *committed-looking* ref with no
> backing entry (ref ahead of oplog — structurally impossible pre-commit anyway).
> **What counts as "the committed tail" for a given read depends on whether the
> ref is per-checkout (local) or shared across worktrees — the r5 `op_scope`
> filter applies to the former, never the latter:**
>
> - **Local ref classes — `HEAD` and undo-recovery — reconcile within the current
>   worktree's `op_scope` (the r5 rule, now scoped down to exactly these two
>   classes).** Both live beside the *per-worktree* `HEAD` pointer, not under the
>   shared ref root: `head_path()` returns the per-checkout `local_head` when set
>   (`refs_storage.rs:69-74`) and `undo_recovery_path()` is its sibling
>   (`refs_storage.rs:89-93`, whose own doc-comment pins "undo/redo recovery state
>   is scoped to the same checkout … never the shared ref root"). `Repository::open`
>   builds the per-worktree manager as
>   `RefManager::new(&shared_galeed_dir).with_local_head(local_head_path)`
>   (`repository.rs:659`; `with_local_head` builder `refs_manager.rs:50`). So a read
>   of `HEAD`/undo-recovery in checkout B resolves **B's lane only** — the committed
>   tail it folds is the `Some(&op_scope())`-filtered one. The filter is
>   load-bearing *here*: without it a long-lived checkout B reconciling its local
>   `HEAD` to checkout A's newest committed target would lazily publish A's HEAD
>   into B's lane (cid 3328776063) — a cross-lane leak that is real precisely
>   *because each worktree has its own HEAD file*. The scope is the worktree
>   discriminator `Repository::op_scope()` already provides
>   (`repository.rs:1636-1654`), reused unchanged — undo/redo scope every oplog
>   scan this way (`undo.rs:108-109`, `:131-132`:
>   `recent_batches_scoped`/`undo_batches_scoped` with `Some(&scope)`; redo at
>   `repository.rs:941-942`); per-read reconciliation of a local ref reuses that
>   same filter and invents no new mechanism.
>
> - **Shared ref classes — thread, marker, remote-thread — reconcile GLOBALLY,
>   NOT `op_scope`-filtered.** These live under the *shared* ref root
>   (`threads_dir`/`flat_threads_dir` `refs_storage.rs:50-55`, `markers_dir` `:63`,
>   `remotes_dir` `:66` — all under `refs_dir()` = `root/refs`, the
>   `shared_galeed_dir` every sibling worktree shares, `repository.rs:659`). There
>   is exactly **one** file per shared ref, visible to all worktrees, so its
>   authoritative value is whatever the **newest committed oplog entry names —
>   regardless of which worktree's `op_scope` committed it**. A read of a shared
>   ref therefore folds the **full** committed tail across all lanes, newest-wins.
>   **Applying the `op_scope` filter to a shared ref is a correctness bug:** if
>   checkout A commits a thread/marker/remote update to the shared oplog and
>   crashes after phase 4 (oplog commit) but before phase 5 (ref publish), a
>   checkout-B read that filtered the tail to B's `op_scope` would **miss A's
>   committed shared-ref update** — A's record sits under A's lane — and keep
>   returning the stale shared ref indefinitely, even though a non-crashed A would
>   have published that value *for all worktrees*. The global fold is exactly what
>   a shared ref's materialized-view semantics require; r5's filter is **removed
>   for these classes** (it was over-applied — r5's own example wrongly named
>   "local HEAD/thread refs," but thread refs are shared, not per-lane). Note this
>   changes only *reconciliation read* scope: undo/redo keep their per-lane oplog
>   scans (undoing your own lane's batches is correct), and `op_scope` still keys
>   every committed `OpRecord` for undo/redo purposes — a shared ref's reconcile
>   simply reads across all lanes' records to find the newest committed target.

This single rule holds the invariant across **all four axes at once** — reader
path (daemon RPC vs direct CLI), **handle age** (freshly opened vs a long-held
`Arc<Repository>`), crash timing (immediate vs delayed), and **oplog topology**
(a private oplog vs a shared backend fronting multiple worktrees) — precisely
because reconciliation happens *per read*, not *per open*, and *at the scope its
ref class demands*: it re-reads the current oplog state from disk on every
resolve, so a handle opened once at process start still reconciles on its
ten-thousandth read. A local-ref read (HEAD, undo-recovery) folds this worktree's
lane and never crosses into another lane's state; a shared-ref read (thread,
marker, remote-thread) folds the full committed tail so it never misses a
co-tenant worktree's committed-but-unpublished update to a shared ref. "Recover
at open" structurally cannot do this; per-read reconciliation is what makes the
daemon-handle cell hold, and it subsumes the daemon-vs-CLI and
immediate-vs-delayed cells the prior rounds enumerated one at a time.

**Where the rule hooks (grounded) — ONE internal load primitive, not a set of
enumerated read methods.** The seam must be the single place every read funnels
through *structurally* — not a hand-picked list of entry points. This is exactly
where r6 fell short: it placed reconciliation "inside `RefManager`" but treated the
seam as three enumerated methods (`read_head`/`get_thread`/`get_marker`).
`RefManager` actually exposes **ten** public read methods, over **two distinct
raw-storage loaders**, and the other seven bypass a three-method hook (verified
2026-05-30):

| # | Public read method | `refs_manager.rs` | Raw loader it touches today |
|---|---|---|---|
| 1 | `read_head` | `:114` | `read_head_state` (`refs_head.rs:22`) |
| 2 | `get_thread` | `:129` | `read_change_id_at` (`refs_head.rs:43`) + `PackedRefs::load` (`:134`) |
| 3 | `get_marker` | `:185` | `read_change_id_at` + `PackedRefs::load` (`:190`) |
| 4 | `get_undo_recovery` | `:252` | `read_change_id_at` |
| 5 | `get_remote_thread` | `:256` | `read_change_id_at` |
| 6 | `list_threads` | `:178` | `try_read_ref_summary_index` (`ref_summary_index.rs:378`) / `list_threads_from_storage` (`:330`) |
| 7 | `list_markers` | `:230` | `try_read_ref_summary_index` / `list_markers_from_storage` (`:343`) |
| 8 | `list_remotes` | `:305` | `try_read_ref_summary_index` / `list_remotes_from_storage` (`:356`) |
| 9 | `list_remote_threads` | `:312` | `try_read_ref_summary_index` / `list_remote_threads_from_storage` (`:374`) |
| 10 | `resolve` | `:327` | composite — delegates to #1/#2/#3/#4 (`:330-333`) |

r6 hooked #1–#3 (the point readers on the `read_change_id_at`/`read_head_state`
loader). The four **list** readers (#6–#9) read a *different* substrate — the
`RefSummaryIndex` sidecar via `try_read_ref_summary_index` (`ref_summary_index.rs:378`)
— so they bypassed the hook entirely, and they are live on production paths:
`thread list` → `collect_thread_summaries` (`cli/src/cli/commands/thread.rs:507`),
`marker list` → `cmd_marker_list` (`cli/src/cli/commands/marker.rs:64`), and
hosted-sync freshness (`client/src/grpc_remote/sync.rs:1001`). The **remote** (#5,
#8, #9) and **undo-recovery** (#4) readers likewise bypassed it. After an
oplog-committed create/delete in the "after ph4, before ph5" window, every one of
#4–#9 would observe a *stale* name set — the "every read reconciles" invariant had
seven holes (cid 3328832780). Enumerating "the 3," then "the 5," then "the 7" is
the drip pattern at the method level: a longer list is not a closed class.

**The structural fix — one load primitive, the sole path for LOGICAL READS.**
Reconciliation does not live in the ten read methods; it lives in **one internal
primitive `reconciled_load`** that is the *only* path in `RefManager` by which a
**logical read** — a read that serves a ref value to a caller — obtains ref data.
Every public read method obtains its ref data exclusively by calling it. (This is
"sole path for logical reads," not "sole toucher of raw storage": the
maintenance/compaction path `pack_refs` legitimately touches the raw loaders
directly and is explicitly exempt — see "Maintenance paths are exempt" below.)

```rust
// The ONE place a LOGICAL read loads ref data. The raw-storage loaders
// (read_change_id_at, read_head_state, try_read_ref_summary_index,
// *_from_storage, PackedRefs::load) are PRIVATE sub-steps invoked from here
// for logical reads — no public *read* method may call them directly. (The
// maintenance path `pack_refs` is the one non-read caller; it rewrites the
// storage representation and serves no logical value — exempted below.)
fn reconciled_load(&self, req: LoadRequest) -> Result<Loaded> {
    let raw = self.raw_load(req);                  // the request-scoped raw read
    let Some(rec) = &self.reconciler else { return Ok(raw) };   // bootstrap: plain cache
    let tip = rec.generation();                    // current oplog head_id (O(1) header read)
    // Scope + watermark are a property of the ref CLASS (r10):
    //   local  (Head, UndoRecovery)        → this worktree's op_scope, cached_local_generation
    //   shared (Thread, Marker, Remote*)   → GLOBAL (all lanes),       cached_shared_generation
    let class = req.ref_class();
    let scope = class.reconcile_scope(self);       // Some(&op_scope()) for local; None (global) for shared
    let watermark = class.watermark(self);         // the per-op_scope cell for local; the global cell for shared
    if tip == watermark.get() {                     // r4/r8 O(1) gate, now SELECTED BY CLASS
        return Ok(raw);                            // every batch ≤ this class's watermark FULLY materialized
    }
    // Watermark lagged ⇒ some committed batch ≤ tip touching THIS class is not yet
    // fully materialized. Reconcile is BATCH-ATOMIC (r8): materialize EVERY ref of
    // EVERY lagged committed batch (within `scope`) — not just `req`'s ref — then
    // advance. Request shape (point/list) selects what is RETURNED, never how much
    // of a lagged batch is MATERIALIZED, so the gate never advances on a partial
    // single-ref reconcile that would leave a batch sibling stale-but-gated.
    let loaded = rec.reconcile_batches(raw, req, tip, scope)?;  // scope: local→op_scope, shared→global
    watermark.set(tip);                            // advance THIS class's watermark ONLY after full materialization
    Ok(loaded)
}
```

`LoadRequest` discriminates which ref or set is wanted — `Head | Thread(name) |
Marker(name) | UndoRecovery | RemoteThread(remote, thread) | ThreadList |
MarkerList | RemoteList | RemoteThreadList(remote)` — so a *point* read's raw-read
sub-step still reads a single ref (it does **not** scan the whole summary set on the
hot path), and only that request-scoped raw-read sub-step branches on `LoadRequest`;
when the gate lags the reconcile is **batch-atomic** (the r8 watermark, below): it
materializes every ref of every lagged committed batch, never just `req`'s ref, so a
point read of one batch member can never leave a sibling member of the same
committed batch stale-but-gated. The reconcile body —
generation gate (r4) + `Some(&op_scope())` filter (r5) + committed-tail fold — is
**shared and unconditional**. The ten public readers become thin projections:

| Public read | Becomes |
|---|---|
| `read_head` / `get_thread` / `get_marker` / `get_undo_recovery` / `get_remote_thread` | `reconciled_load(Head / Thread / Marker / UndoRecovery / RemoteThread)` |
| `list_threads` / `list_markers` / `list_remotes` / `list_remote_threads` | `reconciled_load(ThreadList / MarkerList / RemoteList / RemoteThreadList)` |
| `resolve` (`:327`) | composes over the four point readers (`:330-333`) — transitively reconciled, touches no raw storage of its own |

**The five methods Codex did *not* flag are covered by the same primitive — that
is the test of closure.** Codex (cid 3328832780) named only `list_threads` and
`list_markers`. But `get_remote_thread` (#5), `list_remotes` (#8),
`list_remote_threads` (#9), `get_undo_recovery` (#4), and `resolve` (#10) are
**not** special-cased either: each gets its data from `reconciled_load` (`resolve`
transitively, via the four point readers it already delegates to at `:330-333`). A
fix that only added `list_threads`/`list_markers` to a three-method hook would
still leave these five bypassing — i.e. it would have added two cells, not closed
the class. The primitive covers all ten *by construction*, because there is no
other way to obtain ref data.

**Structural impossibility of bypass (the future-proofing).** The raw loaders
(`read_change_id_at`, `read_head_state`, `try_read_ref_summary_index`, the
`*_from_storage` scanners, and `PackedRefs::load`) are reachable from a **logical
read** path **only** through `reconciled_load` — a module-visibility boundary
enforced by a one-line conformance check. The check targets *logical-read*
call-sites: it asserts each raw loader's only logical-read caller is
`reconciled_load`, against a small explicit allowlist of **maintenance** callers
(`pack_refs`, below). (A naive "exactly one caller" check would fail, because
`pack_refs` legitimately calls four of these loaders directly — that is precisely
the carve-out the next paragraph reasons through, not a hole.) Because a public
*reader* can reach ref data *only* through `reconciled_load`, and `reconciled_load`
*always* reconciles:

- every one of the ten present readers reconciles, and
- **any read method added to `RefManager` in the future reconciles automatically**
  — it has no raw storage to read except via the primitive; a "method #11" that
  tried to skip reconciliation would have no data source to compile against. The
  "92nd reader" / "next axis" hole that broke every prior round is closed at the
  type/visibility level, not by vigilance.

That is the difference between r6 and r7: r6 enumerated entry points (and the
enumeration was incomplete); r7 makes raw storage *unreachable* from a logical read
except through the one reconciling primitive, so the set of covered readers is "all
of them, present and future," with no list to keep complete.

**Maintenance paths are exempt — `pack_refs` rewrites the representation, it does
not serve a logical value (r10, cid 3328894984).** The "sole path for logical
reads" claim is about reads that *return a ref value to a caller*. `pack_refs`
(`refs_manager.rs:337-380`, exposed as `CoreRefBackend::pack_refs` `:479` and run by
`heddle gc`, `gc.rs:104`) is not such a read: under `lock_refs()` (`:338`) it loads
each loose thread/marker via the raw loaders — `PackedRefs::load` (`:340`),
`list_threads_from_storage` (`:342`), `read_change_id_at` (`:345`,`:352`),
`list_markers_from_storage` (`:349`) — folds them into `packed-refs`, saves, and
deletes the now-redundant loose files. It **compacts the storage representation**;
it returns no ref value to a caller, so it has no reconciliation to do and is
correctly exempt from the conformance check (it appears on the allowlist, not as a
logical-read caller). The exemption is a *reasoned carve-out*, not a hand-wave:

- **Consistency w.r.t. reconciliation.** `pack_refs` operates on a **committed,
  already-materialized snapshot** — it reads loose ref files (the phase-5
  materialized form) under `lock_refs()` and rewrites them into `packed-refs`,
  which the loaders read back identically (`get_thread`/`get_marker` fall back to
  `PackedRefs::load`, `refs_manager.rs:134`,`:190`). Packing a ref is value-
  preserving: the loose file and its packed entry resolve to the same `ChangeId`,
  so a subsequent `reconciled_load` sees the same raw value before and after — the
  generation/head_id gate is unaffected (packing appends nothing to the oplog), and
  reconciliation against the committed tail yields the identical result.
- **It must not drop an oplog-committed-but-not-yet-materialized ref.** The one
  hazard is ordering: a ref whose update is committed in the oplog (phase 4) but
  whose canonical file is not yet renamed into place (phase 5) is invisible to
  `pack_refs`'s loose-file scan, so packing *that* ref's *current* file is a no-op
  on the unmaterialized value — packing cannot "lose" it, because there is nothing
  on disk to pack and reconciliation will still materialize it on the next read
  from the oplog tail. `pack_refs` only ever compacts files that already exist; it
  never deletes a ref the oplog still names. The standing impl requirement (carried
  to §6 O7) is that `pack_refs` runs under the same `lock_refs()` that phase-5
  publication takes, so it observes a *quiescent* materialized snapshot and never
  races a half-done rename — it packs what is durably materialized and leaves the
  reconciler to materialize the rest.

*Why an accessor-layer hook still fails (unchanged from r6, now at the primitive
level).* Placing reconciliation on the `Repository` accessors (`repo.head()`
`repository.rs:1737`, etc.) is bypassable: **~124 production call-sites read
`repo.refs().<read>(…)` directly on `RefManager`** across the ten methods
(`rg "refs\(\)\.(read_head|get_thread|get_marker|list_threads|list_markers|get_undo_recovery|get_remote_thread|list_remotes|list_remote_threads|resolve)"`
→ 124 non-test hits, verified 2026-05-30), going straight around any `Repository`
accessor — e.g. `status` (`cli/src/cli/commands/status.rs:1795`), `collapse`
(`cli/src/cli/commands/collapse.rs:99`), hosted sync
(`client/src/grpc_remote/sync.rs:588`, `:1001`), `thread list` (`thread.rs:507`),
`marker list` (`marker.rs:64`). Enforcement at a layer callers skip is not
enforcement. The primitive sits *below* every `RefManager` read method, so all
~124 direct readers — plus every `Repository` accessor and the daemon handler
(`transaction.rs:143-152`) — inherit reconciliation with zero call-site changes,
and there is no lower-level raw read a caller can reach around it.

*The crate-dependency objection, and how the primitive clears it (retained from
r6).* Reconciliation must consult the committed oplog tail, yet `RefManager` lives
in the `refs` crate, which **does not depend on `oplog`** (`crates/refs/Cargo.toml`
declares no oplog dep — verified 2026-05-30). The resolution is **dependency
inversion, kept exactly as r6 designed it**: define a narrow `RefReconciler` trait
*in the `refs` crate*, over types `refs` already owns (`Head`, `ChangeId`, the ref
name, and the cached `head_id: u64` generation); have `RefManager` hold an
`Option<Arc<dyn RefReconciler>>` set by a `with_reconciler(..)` builder — the same
shape as the existing `with_local_head` builder (`refs_manager.rs:50`) — and inject
the concrete oplog-backed implementation from the `repo`/`oplog` layer at
`Repository` construction (`crates/repo/Cargo.toml:22` oplog, `:24` refs — the
layer that sees both crates). The trait is invoked **inside `reconciled_load`** (not
in each read method), so the `refs` crate gains **no** oplog dependency and the
reconcile logic exists in exactly one place. Both the O(1) `head_id` generation
gate (r4) and the `Some(&op_scope())` filter (r5) run inside the primitive. A
bootstrap `RefManager::new` with no reconciler keeps today's plain-cache behavior.

Reconciliation re-derives the committed target with no extra bookkeeping: every
committed state `OpRecord` carries the ref identity + target — `Snapshot {
new_state, thread }` (`oplog_types.rs:18-22`), `ThreadCreate/ThreadUpdate { name,
… state }` (`:29`, `:33`), `Goto { target }` (`:24`) for HEAD, and — as of r9 —
`RemoteThreadUpdate { remote, thread, state }` / `RemoteThreadDelete { remote,
thread, state }` / `UndoRecoveryUpdate { state }` for the remote-thread and
undo-recovery classes (new tail variants appended after `GitCheckpoint`
(`oplog_types.rs:222-228`) so existing on-disk discriminants are unperturbed; see
"Remote-thread and undo-recovery writes are oplog-committed too" below) — and the read
takes the newest committed target in the tail *at the scope its ref class demands*
— this worktree's `op_scope` for a **local** ref (HEAD, undo-recovery), the **full**
tail across all lanes for a **shared** ref (thread, marker, remote-thread)
(newest-wins, so two committed txns on one ref resolve to the same value a
non-crashed run would produce). **For a set-valued (list) request the same records
drive membership, not just a single target:** a committed `ThreadCreate` /
`MarkerCreate` (and the corresponding delete) in the tail adds or removes the name
from the reconciled set the primitive returns, so `list_threads`/`list_markers`/
`list_remotes`/`list_remote_threads` never report a name whose create/delete is
committed-but-not-yet-materialized — exactly the stale-list hole cid 3328832780
flagged. (Remote-thread and undo-recovery refs reconcile identically — and, as of
r9, *non-vacuously*: their committed records — `RemoteThreadUpdate` /
`RemoteThreadDelete` / `UndoRecoveryUpdate` — now actually exist in the tail and
name the remote-thread / recovery target, folded exactly like a thread/marker
record (a `RemoteThreadDelete` removes the name from the reconciled
`list_remotes`/`list_remote_threads` set, just as `MarkerDelete` does for markers).
Before r9 these two classes had **no** `OpRecord` at all — `set_undo_recovery`
(`refs_manager.rs:242`) and `set_remote_thread` (`:261`) wrote the ref directly with
no oplog append (cid 3328869364) — so reconciling them folded an *empty* tail; r9
closes that by giving them committed records, immediately below.) "Newest in
the tail" is scoped **by ref class** (r10): for a **local** ref (HEAD,
undo-recovery) it means newest among *this worktree's* entries — the scan is the
`Some(&op_scope())`-filtered one undo/redo already run (`undo.rs:108-109`,
`:131-132`), so in a shared-oplog setup a read in checkout B resolves B's lane only
and never lifts checkout A's newest committed HEAD. For a **shared** ref (thread,
marker, remote-thread) it means newest across **all** lanes — there is one shared
ref file (under the shared `refs/` root, `refs_storage.rs:50-67`) whose
authoritative value is whatever the newest committed entry names, so checkout B's
read of a shared thread/marker/remote ref MUST fold checkout A's committed update
too; filtering it to B's lane would miss A's committed-but-unpublished shared
write (the §2.2 universal-rule bug).

**Remote-thread and undo-recovery writes are oplog-committed too (r9 — closing the
last direct-write exceptions, cid 3328869364).** The reconciliation above is only
non-vacuous if the ref classes it reconciles *have* committed records to reconcile
against. Two did not. `set_undo_recovery` (`refs_manager.rs:242`) and
`set_remote_thread` (`:261`) — together with the removal path `delete_remote_thread`
(`:284`) — wrote their refs **directly** (`lock_refs()` + `write_string`/`remove_file`
+ a summary-index rebuild), with **no** `OpRecord` appended; the `OpRecord` enum
(`oplog_types.rs:16`) carried variants for thread/marker/HEAD writes but **none**
for remote-thread or undo-recovery. So `get_remote_thread` / `list_remote_threads`
/ `list_remotes` (#5/#9/#8) and `get_undo_recovery` (#4) reconciled against a tail
that, for their class, was always empty — after an oplog-only commit + a hard crash
those refs could not be re-derived, and "every read reconciles" was *vacuously*
true for them. r9 removes the exception by treating these writes exactly like
thread/marker writes:

- **New committed `OpRecord` variants**, modeled on `ThreadUpdate { name, old_state,
  new_state }` (`oplog_types.rs:33`) and `MarkerCreate`/`MarkerDelete { name, state }`
  (`:46`,`:47`) and appended at the enum tail after `GitCheckpoint` (`:222-228`) per
  the append-only, discriminant-stable rule (`:12-14`: rmp-serde encodes variants by
  index, so new variants append and never reorder): `RemoteThreadUpdate { remote:
  String, thread: String, state: ChangeId }` (covers `set_remote_thread`),
  `RemoteThreadDelete { remote: String, thread: String, state: ChangeId }` (covers
  `delete_remote_thread`, the remote-thread removal path), and `UndoRecoveryUpdate {
  state: ChangeId }` (covers `set_undo_recovery` — a single rolling ORIG_HEAD-style
  pointer with no delete path, so one update variant suffices).
- **The setter's commit point becomes the oplog append, not the `write_string`.**
  `set_remote_thread`/`delete_remote_thread`/`set_undo_recovery` are re-specified to
  append their `OpRecord` as the phase-4 linearization point under the oplog write
  lock — the same oplog-as-sole-commit rule §2.2 already mandates for thread/marker
  writes — and only then publish the canonical ref (the `write_string`/`remove_file`
  + index rebuild become a phase-5 post-commit materialization, temp→rename like
  every other ref). These three writers are therefore **no longer exceptions** to
  "the oplog append is the SOLE commit point"; there are now **zero** ref-write paths
  that publish a committed-looking ref without a backing committed record.
- **Recovery replay materializes them like any other class.** Because the writes now
  carry committed records, the `reconciled_load` primitive (and the crash-replay in
  the crash table above) re-derives a remote-thread or undo-recovery ref from its
  newest in-scope `RemoteThreadUpdate`/`RemoteThreadDelete`/`UndoRecoveryUpdate`
  exactly as it does a thread ref from `ThreadUpdate`: a delete record drops the name
  from the reconciled `list_remotes`/`list_remote_threads` set, an update record sets
  the point value. The "after ph4, before ph5" lag and the hard-crash residue resolve
  identically for these classes — no longer a no-op against an empty tail.

**Keeping it cheap (the cost the decision accepts).** Reconciling on *every* read
must not become a full oplog scan per read. The hot-path check is a **generation
/ commit-index** comparison against the oplog's monotonic head id. The packed
oplog already carries `head_id: u64` (`packed_oplog.rs:26`) and writes it as the
**leading field** of the packed file (`packed_oplog.rs:55`), so the current value
is readable from the file header without parsing the log; every append advances it
(`packed_oplog.rs:206-209`; `start_id = packed.head_id + 1`, `oplog_core.rs:249`,
`:308`). A reader caches the `head_id` it last reconciled against, and on each
read reads the current `head_id` (an O(1) header read) and:

- if **this ref class's watermark** is **unchanged** vs the current `head_id`, no
  commit affecting that class has landed since the last reconcile ⇒ the canonical
  ref (or the cached summary set) is current ⇒ return it directly — **no tail scan,
  no write**;
- only if it has **advanced** does the primitive scan the tail from `cached+1` for
  committed `TransactionCommit` entries — **filtered to this `op_scope` for a local
  ref (HEAD, undo-recovery), unfiltered/global for a shared ref (thread, marker,
  remote-thread)** (r10) — and reconcile **batch-atomically**: for every lagged
  committed batch it materializes **every** ref of that class the batch touched
  (newest-committed target / membership change per ref, lazily re-publishing each
  lagging ref or summary) — *not only* the ref `req` asked for — and **only then**
  advances **that class's** cached watermark to the scanned `head_id`. Materializing
  the whole batch before the watermark moves is what makes the gate safe for the
  *next* read of any sibling ref (the r8 watermark, below). For a **local**-ref scan
  the same `Some(&op_scope())` exact-match filter undo/redo use (`undo.rs:108-109`,
  `:131-132`) applies, so a `head_id` advance driven purely by *another* worktree's
  *local* commit finds no entry for this lane and the reconcile is a no-op — the
  reader keeps returning its own canonical HEAD while still advancing its
  per-`op_scope` watermark. For a **shared**-ref scan there is no lane filter: a
  `head_id` advance driven by *any* worktree's shared-ref commit is folded, so the
  reader picks up a co-tenant's committed thread/marker/remote update (the global
  watermark, below).

So the steady-state hot path is one small header read plus an integer compare
against the **class-selected** watermark (local→per-`op_scope`, shared→global);
the full reconcile runs only on the rare post-crash lag. (A per-ref committed
index tightens this further — reconcile only when *this ref's* newest committed
target advanced — but the two class-split `head_id` watermarks are the simple floor.
**If a per-ref index is adopted it must still be batch-atomic *and* class-scoped:**
a reconcile advances the per-ref watermark for **every** ref the lagged batch
materialized, not just the requested one — else it reintroduces exactly the sibling gap r8
closes. Exposing a cheap `OpLog::head_id()`/`tip()` header accessor is net-new
impl work, §6 O7.)

**The generation is a watermark of *fully-materialized* batches, not of partial
reconciles (r8, cid 3328853451) — and it is SPLIT by ref class (r10, cid
3328894983).** The cheap gate above is a `head_id` compare, so its meaning must be
pinned precisely or it can short-circuit a sibling ref. Because reconciliation
scope is now a property of the ref class (§2.2 universal rule), the single watermark
splits into **two** cached cells, each compared against the same monotonic oplog
`head_id` but advanced by a scan at its class's scope:

> - **`cached_local_generation == N`** (gates HEAD + undo-recovery reads) means
>   **every *local* ref of every committed batch with generation ≤ N *in this
>   `op_scope`* has been materialized.**
> - **`cached_shared_generation == N`** (gates thread + marker + remote-thread
>   reads) means **every *shared* ref of every committed batch with generation ≤ N
>   *across all lanes* has been materialized.**
>
> Each is a watermark of *fully-materialized* batches for its class — neither ever
> names a batch only partially reconciled. A read checks the watermark **matching
> its ref class**; a committed batch's reconcile advances the watermark(s) for the
> ref class(es) it touched — a batch touching both a local and a shared ref (e.g. a
> `capture`, which writes HEAD *and* a thread ref) advances **both** once each
> class's refs are materialized. The O(1) hot path is unchanged: one header read +
> one integer compare against the class-selected watermark.

*The bug a looser definition admits.* A single committed batch can update
**multiple** refs (`a`, `b`, …) — `update_refs(&[RefUpdate])` applies a batch of
thread/marker/head writes under one refs lock (`refs_manager.rs:319`, `types.rs:16`)
— and a hard crash can leave the whole batch committed-in-oplog but **none** of its
refs materialized. If a lagged `get_thread(a)` were to materialize **only** `a` and
then advance the *global* gate to "current," a subsequent point read `get_thread(b)`
of `a`'s batch sibling on the same handle would see `head_id` unchanged, take the
O(1) fast path, and return **raw stale storage for `b`** — `committed ⇔ observed`
broken for `b` while the gate falsely reads "current." This is **not** a reader
bypassing the primitive (r7 closed that class — all ten readers funnel through
`reconciled_load`); it is the gate *inside* the primitive advancing on a **partial**
reconcile.

*The fix — batch-atomic materialization, one rule.* A lagged read reconciles the
**full** pending batch(es): it materializes **all** refs *of the read's class* each
lagged committed batch touched (within that class's scope — this `op_scope` for
local, all lanes for shared), **before** advancing that class's watermark. The
watermark moves in lockstep with *whole batches*, never with individual refs, so it
is "current" iff every ref of its class in every batch ≤ it is materialized. The
read stays lazy — it does real
work *only* on the rare lag; the no-lag hot path is the unchanged O(1) header
compare with no materialization — the change is solely that when it *does*
reconcile, it completes the whole crashed batch, not just the one ref it was asked
for.

*Why this closes the class for **every** read shape — no sibling left (the
structural test).* Because the watermark advances *only* on full-batch
materialization, the very next read — of *any* ref, in *any* shape — is correct the
instant the gate reads "current":

- the flagged **point-read sibling** `get_thread(b)` after `get_thread(a)`: when
  `a`'s read advanced the gate it materialized `b` (same batch) in the same pass, so
  `b`'s fast path returns the materialized value, never stale storage;
- the **list-read sibling** `list_threads` / `list_markers` / `list_remotes` /
  `list_remote_threads` (which return **many** names at once): once the gate is
  current, every name's create/delete in every batch ≤ the watermark is
  materialized, so no list can report a committed-but-unmaterialized membership —
  the stale-set hole cid 3328832780 flagged stays closed even under the *global*
  gate;
- the **remote / undo-recovery** readers (`get_remote_thread`, `get_undo_recovery`)
  and **any future** `RefManager` read method: identical — they read through the
  same gate, and "gate current" means "all refs of all batches ≤ watermark
  materialized," independent of which ref or shape was requested. And as of r9 a
  *write* to either class is itself an oplog-committed batch
  (`RemoteThreadUpdate`/`RemoteThreadDelete`/`UndoRecoveryUpdate`, §2.2 "Remote-thread
  and undo-recovery writes are oplog-committed too"), so a batch that touches a
  remote-thread or undo-recovery ref is materialized whole before the watermark
  advances, exactly like a thread/marker batch — the watermark covers these classes
  too, not just thread/marker/HEAD.

A *per-ref* gate (Codex's other option) also fixes the flagged point read, but it
would need **separate** reasoning to show the list readers are safe (a list touches
a whole name-set, not one ref). The full-batch **watermark** pre-empts the list-read
*and* remote-read siblings *by construction* — one invariant covers point, list,
remote, and future shapes — so r8 closes the class structurally instead of patching
the single point-read cell Codex named.

*The cost this accepts (the tradeoff).* A lagged read now pays to materialize the
**entire** crashed batch, not just its one requested ref. Acceptable because: (a)
lag occurs **only** after a *hard* crash (kill -9 / power loss) left a batch
committed-but-unmaterialized — rare; an in-process crash is pre-commit by
construction (§4 `Drop` backstop), and the `Repository::open` eager pass already
repairs lag once up front on the common one-shot path; and (b) the **hot path is
unchanged** — a no-lag read still does exactly one header read + one integer
compare, materializing nothing. We trade a slightly heavier *rare* reconcile for a
global O(1) gate correct across all read shapes.

*Watermark scope is per ref class (r5 for local; global for shared — r10).* The
**local** watermark stays `op_scope`-scoped exactly as r5 mandated: its
batch-materialization scan is the same `Some(&op_scope())` exact-match filter
undo/redo apply (`undo.rs:108-109`, `:131-132`), so "every local ref of every batch
≤ N" means every HEAD/undo-recovery ref of every batch ≤ N **in this worktree's
lane**. A `head_id` advance driven purely by a *co-tenant* worktree's **local**
commit materializes nothing for this lane and still advances this lane's local
watermark — the cross-lane-publish hazard cid 3328776063 fixed stays fixed (a
checkout never lifts a co-tenant's HEAD). The **shared** watermark is **global**, by
construction: its scan is *unfiltered*, so a `head_id` advance driven by *any*
worktree's shared-ref commit materializes that committed thread/marker/remote update
for this reader too — which is required, because a shared ref has one file visible
to all worktrees and filtering its reconcile to a lane would miss a co-tenant's
committed-but-unpublished shared write (cid 3328894983). The generation/commit-index
identity from r4 (the gate *is* the oplog's monotonic `head_id`,
`packed_oplog.rs:26`,`:55`) is unchanged; r8 pins *when* a cached copy may advance
(after a full-batch materialization, never a partial one); r10 pins *which* copy and
*at what scope* (local→op_scope, shared→global).

**"Recover at open" is demoted to an optimization, not the guarantee.** Keeping
an eager materialization pass at `Repository::open` (`repository.rs:594`, hit by
the daemon pre-serve and by the CLI harness per invocation `harness/mod.rs:127`)
is still *useful*: it converts any lagging ref to its committed value **once**,
eagerly, so the subsequent reads on that freshly-opened handle hit the cheap-check
fast path with nothing to reconcile (and a one-shot `heddle status`/`log`/`capture`
that opens, reads, and exits is repaired up front). But it is **not load-bearing**:
correctness comes from the per-read rule, which holds even for the daemon handle
that never re-opens. The open-time pass is an *eager prefetch* of work the read
would otherwise do lazily — drop it and the invariant still holds; keep it and the
common path is faster. The daemon keeps its separate sentinel-abort recovery
(`replay_active_transactions`, `local_daemon.rs:296`, its sole production caller;
`transaction_replay.rs:185-204` only aborts stuck on-disk sentinels, never
materializes refs) — a distinct job (sentinel lifecycle, daemon-scoped) from ref
reconciliation (reader-scoped, per read). The per-verb detection primitive
`active_transactions` (`transaction_sentinel.rs:60`) is *not* a substitute: it is
documented as something "every state-changing CLI verb *should* consult"
(`transaction_sentinel.rs:4-8`) but is not wired into dispatch (its only non-test
references are its own module, `:92`), so relying on each verb to remember it would
re-open the class one verb at a time. The single `reconciled_load` primitive is
the structural choke point every read funnels through — all ~124 direct
`repo.refs()` readers across the ten read methods reach ref data only through it —
so enforcing reconciliation *inside the primitive* (not at the bypassable accessor
layer, and not by enumerating read methods) is what closes the class.

**Why a reader NEVER sees a committed-looking ref without its oplog record.** The
canonical ref path is written *only* by the phase-5 rename or by a reconciling
read's lazy re-publish, both of which strictly follow the phase-4 oplog commit. A
reader resolving a ref through `RefManager`'s reconciling read methods therefore
observes exactly one of:

- the **OLD** value with the oplog tail naming a **newer committed** target (the
  "after ph4, before ph5" lag, or a delayed hard crash): the cheap check sees
  `head_id` advanced, the reconcile resolves the **committed** target and returns
  *that*, never the stale cache. The invariant holds — the entry exists, the read
  does not trust the lagging ref — and it holds **on every path and handle age**:
  the daemon's long-held handle reconciles on its next handler read just as a
  fresh CLI open does. (An *in-process* crash never even produces this lag — it is
  pre-commit by construction via the `Drop` backstop, §4; only a *hard* crash,
  kill -9 / power loss, leaves the on-disk lag, and the very next *read* on any
  handle resolves it.)
- the **OLD** value with **no** newer committed target in the tail (a pre-commit
  crash, ph1–3): not committed, ref correctly still OLD. Holds.
- the **NEW** value (phase-5 rename, or a prior reconcile already re-published) —
  which can only have happened *after* the phase-4 commit. A backing committed
  oplog entry is therefore guaranteed. Holds.

The one direction the invariant forbids — a NEW, committed-*looking* ref with
**no** oplog entry — is structurally impossible, because nothing publishes the
canonical ref before the oplog entry is durable. Today's capture violates exactly
this (it renames the ref at `repository_snapshot.rs:241-250` *before* the append
at `:252`); the migration (§5.3) moves the publish to phase 5, and per-read
reconciliation closes the residual lag for *every* reader and handle regardless of
when the publish lands.

**Idempotency of the commit — unbounded indexed `transaction_id`, NOT the
window-bounded helper.** The phase-4 linearization point must be exact-once at
*any* retry timing, including a delayed crash-retry that re-runs `execute` after
an arbitrary number of intervening commits. The existing
`record_batch_scoped_if_no_transaction` (`oplog_core.rs:281`) is **not** that
point: under the write lock it scans only the most recent `recent_window` batches
(`collect_batches_scoped(recent_window, …)`, `oplog_core.rs:295`), so a retry
after >`recent_window` intervening batches scans *past* the prior
`TransactionCommit` and appends a **second** one for the same transaction. Its
sole production caller passes `64` and explicitly accepts that "ageing past it"
duplicates the batch (`rebase_ops.rs:192-202`) — fine for the immediate-retry
race it was built for, wrong as a general commit point.

The primitive's commit therefore deduplicates on an **unbounded, indexed
`transaction_id` → committed-batch-id map**, maintained **under the same oplog
write lock** as the append and updated atomically with it (so the index can never
disagree with the log). The commit step:

1. take the oplog write lock (`oplog_core.rs:66`);
2. look up `transaction_id` in the index — an O(1) hash lookup over the *entire*
   committed history, not a windowed scan. If present ⇒ the transaction already
   committed (a prior, possibly long-ago, retry) ⇒ no-op, return the recorded
   ids;
3. else append the batch (`packed.append` + `packed.save()`, `oplog_core.rs:315-316`)
   **and** insert `transaction_id → start_id` into the index in the same locked
   section, then release.

Because the lookup domain is unbounded, a retry at **any** later time finds the
existing commit and refuses to double-append; because the index update is inside
the same critical section as the append, two concurrent retriers serialize on the
lock and exactly one wins (the heddle#198 r4 *atomicity* guarantee carried
forward — only the *window* is removed). The correctness floor, if the impl
prefers no sidecar, is a full-tail scan for the marker (O(n) but unbounded); the
indexed map is the performant form. Either way the defining property is
**unbounded domain** — the window-bounded helper is explicitly *not* the
linearization point. Building this index is net-new impl work (§6 O7).

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

### 2.4 — Crash/retry coverage (the close-the-class proof)

> **Precondition (PROPOSED, not shipped).** This proof establishes the invariant
> **for the design after the impl epic lands** — i.e. after the three net-new
> mechanisms below are *built*: (1) the read chokepoint (`reconciled_load` +
> `RefReconciler`), (2) the write chokepoint as a **per-backend `RefBackend`-trait
> `commit_and_publish` seam** (r18, cid 3329052679) — closing the record-before-publish
> property on **both** the file backend (`RefManager`, temp→rename) and the hosted
> backend (`PgRefBackend`, its own SQL publish, `pg_refs.rs:324`/`:328`) — **plus** the
> `OpRecord` format changes it relies on (the net-new
> `RemoteThreadUpdate`/`RemoteThreadDelete`/`UndoRecoveryUpdate` variants and the
> `Fork`/`Collapse` ref-identity retrofit), and (3) the unbounded indexed
> `transaction_id` commit. **None of these exist in the current tree**
> (see the EXISTING-vs-PROPOSED note in the header and §6 O1/O7/O9). Today's code
> still has the direct-write exceptions (`set_remote_thread`/`set_undo_recovery`
> write directly; `cmd_fork`/`cmd_collapse` publish before recording and emit
> ref-blind records), so the universal claim does **not** describe shipped
> behavior — it is the target state the spike designs and the impl epic builds.

The invariant — stated precisely, **`committed ⇔ a ref-carrying oplog record
exists`, with the *recovery domain* set by the ref class** (a **local** ref — HEAD,
undo-recovery — recovers within its own `op_scope`/lane; a **shared** ref — thread,
marker, remote-thread — recovers **globally**, across all lanes, since one file serves
every worktree) — must hold for *every* reader, *every* **writer**, and *every* retry
timing. "Ref-carrying" is load-bearing: the record must name the ref it published, or
replay cannot materialize it (the `Fork`/`Collapse` gap, §2.2 "The write chokepoint").
The prior rounds
tried to prove this **cell by cell** —
r1 fixed ordering, r2 added daemon recovery + a window dedup, r3 moved recovery to
the `open` seam to cover the direct-CLI cell — and each round closed one cell only
to have Codex surface its **sibling**: the daemon-only gap, then the windowed-dedup
gap, then (cid 3328112197) the **already-open daemon handle** that no open-time
pass can reach. That drip is the symptom of the wrong frame: a 2-axis
{reader path} × {retry timing} matrix silently assumed a third axis — **handle
age** — was fixed at "freshly opened," which is exactly the assumption the
long-held `Arc<Repository>` (`local_daemon.rs:330`) violates. Enumerating cells
will always miss the next axis. **So this round stops enumerating and proves the
invariant once, from a mechanism that sits in the path every reader shares.**

**The collapse.** Three orthogonal mechanisms — one on the *read* side, two on the
*write* side — make the entire product space hold, with no per-cell case analysis. The
two sides are duals: the write chokepoint guarantees a ref is never published without a
preceding ref-carrying record; the read chokepoint guarantees a published ref is never
*trusted* without confirming that record. Neither alone closes the class — a record
with no reconcile leaves stale caches authoritative; a reconcile with no record (the
`Fork`/`Collapse` gap, or the pre-r9 remote/undo gap) folds an empty or ref-blind tail.
Together they hold the invariant for every writer, reader, and crash timing:

- **Read side — per-read reconciliation (§2.2 "Reader model").** Every ref read
  reconciles against the committed oplog tail at read time, because all **ten**
  public `RefManager` read methods (point, list, remote, undo-recovery, and
  `resolve` — `refs_manager.rs:114`–`:327`) obtain ref data through **one internal
  `reconciled_load` primitive**, the sole path for **logical reads** to touch raw
  ref storage (§2.2 "Where the rule hooks"; the maintenance path `pack_refs` touches
  the loaders directly but serves no logical value and is exempt, §2.2 "Maintenance
  paths are exempt"). **The reconciliation is non-vacuous for every one
  of those classes:** as of r9 the remote-thread and undo-recovery writes carry
  committed `OpRecord`s (`RemoteThreadUpdate`/`RemoteThreadDelete`/`UndoRecoveryUpdate`,
  `oplog_types.rs:16`) appended at the oplog-commit point — they are no longer
  written directly (cid 3328869364) — so `get_remote_thread`/`list_remote_threads`/
  `list_remotes`/`get_undo_recovery` reconcile against records that actually exist,
  exactly like the thread/marker readers; no reader resolves a class whose tail is
  empty by construction. The reconciliation lives *inside* that primitive,
  so every present reader — and any future `RefManager` read method, since raw
  storage is unreachable except through the primitive — reconciles automatically;
  the ~124 direct `repo.refs()` readers (and every `Repository` accessor, and the
  daemon handler `transaction.rs:143-152`) inherit it with no call-site changes,
  reached via a `refs`-crate `RefReconciler` trait the `repo`/`oplog` layer injects
  (dependency inversion — no `refs`→`oplog` crate dep). **The reconciliation scans
  at the scope its ref class demands** (r10): a **local**-ref read (HEAD,
  undo-recovery) folds **only this repository's `op_scope`** — the same
  `Some(&op_scope())` exact-match filter undo/redo apply to every oplog scan
  (`undo.rs:108-109`, `:131-132`; `Repository::op_scope()` `repository.rs:1636`), so
  each lane resolves its own HEAD/recovery; a **shared**-ref read (thread, marker,
  remote-thread) folds the **full** tail across all lanes, because one shared ref
  file serves every worktree and its authoritative value is the newest committed
  entry from *any* lane. Therefore a reader on
  **any path** (daemon RPC or direct CLI), holding a handle of **any age**
  (freshly opened or a long-held `Arc<Repository>`), observing a crash at **any
  time** (immediate or delayed), under **any oplog topology** (a private oplog or
  a shared backend fronting multiple worktrees):
    1. never treats a canonical ref as committed without its backing oplog entry
       *in its recovery domain* — its own `op_scope` for a local ref, any lane for
       a shared ref (the read confirms the entry), and
    2. never returns a stale canonical ref the oplog has already superseded: for a
       local ref it resolves the committed target **within its own `op_scope`** and
       **never lifts another lane's HEAD**; for a shared ref it resolves the newest
       committed target **across all lanes** and **never misses a co-tenant's
       committed-but-unpublished shared write** (cid 3328894983).
  Because the check is *in the read*, not *at the open*, and *at each class's
  correct scope*, there is no reader, handle, timing, or co-tenant worktree
  that escapes it. (The cheap O(1) gate that makes this affordable is a **watermark
  of *fully-materialized* batches, split by ref class** — a per-`op_scope` cell for
  local refs and a global cell for shared refs — advanced only after a lagged read
  materializes a crashed batch's *every* ref of that class, never on a partial
  single-ref reconcile (§2.2 r8/r10, cid 3328853451 + 3328894983) — so a
  point/list/remote read of a *batch sibling* cannot slip past the gate onto stale
  storage: "gate current" *means* every ref of its class in every batch ≤ it is
  materialized, so the collapse below is not undermined by the optimization that
  makes it cheap.) The matrix collapses to **"all reads reconcile, at their class's
  scope, ∎"** — there are no cells left to enumerate, because the third axis (handle
  age) and fourth axis (shared-oplog topology) the per-cell frame missed are closed
  by the same mechanism as the first two.

- **Write side (1) — the write chokepoint: every published ref preceded by a
  ref-carrying record, the whole atomic batch committed together (§2.2 "The write
  chokepoint," r11, cid 3328926767; op-descriptor refinement r15, cid 3328979498;
  record-batch refinement r16, cid 3329003333).** The canonical-ref publish (temp→rename,
  `refs_transactions.rs:230`) is reachable **only** through one commit-then-publish
  primitive `commit_and_publish(op_records, ref_updates)`, which appends **all** of the
  **caller-supplied** ref-carrying record batch `op_records` (phase 4) *before* publishing
  the atomic `ref_updates` batch (phase 5) **without splitting it**. The primitive *takes*
  the records rather than synthesizing them — the generic write methods see only ref
  identity + target, not the `sources`/`from` the `Collapse`/`Fork` variants need — so the
  semantic caller builds its full record(s) and hands them in, while a plain ref edit
  passes a one-element generic ref-update record; either way the publish routes through the
  one primitive, which enforces ordering + atomicity *universally* regardless of which
  records it is handed. It accepts a **batch** (r16) so a multi-record atomic op is faithful:
  thread-rename publishes new-thread + old-thread-delete (± HEAD) in one atomic ref vector
  (`thread.rs:3074-3100`) backed by a two-record batch (`ThreadCreate`+`ThreadDelete`,
  `oplog_records.rs:96-110`) — a single-`OpRecord` chokepoint would have dropped a backing
  record or split that atomic batch. **And the batch must back EVERY ref it publishes
  (full-batch-record-coverage, r17, cid 3329019021):** when the renamed thread is the
  current attached HEAD the vector gains a third `RefUpdate::Head` (`thread.rs:3090-3099`)
  but `record_thread_rename` still emits only the two records, so that case must commit a
  **three-record** batch (a HEAD-move record added), else the published HEAD ref has no
  replay record and the atomic batch cannot be fully re-materialized on crash recovery.
  **Every published ref in the batch has a backing record in the same atomic publish.** The
  ref-write call sites (verified 2026-05-30) inherit the record-before-publish ordering by
  construction, and — because the raw publish is private with `commit_and_publish` its sole
  caller (r17, cid 3329019023) — so do the CAS/create wrappers and every future writer — including the two
  that had it backwards (`fork.rs:74-92`, `collapse.rs:99-108`, which published *before*
  recording, and whose `record_fork(from, new_state)` call (`oplog_records.rs:113`) also
  passed its arguments reversed, `fork.rs:94-95`, persisting the source as the result) — and
  any future writer does too, because there is no raw publish to call around the
  chokepoint. Combined with the variant audit (every publishing `OpRecord` now names
  the ref it publishes — the existing `Fork`/`Collapse` variants are extended in place
  to carry the published thread+HEAD, which become the **authoritative replay target**
  over the from/new_state positions, the rest already do), this guarantees the
  forbidden state — a NEW, committed-*looking* canonical ref with **no backing
  ref-carrying record** — is **unreachable on the write path**, for *any* writer,
  single- or multi-record, not merely for capture. This is the structural dual of the read chokepoint: r7 made every
  *read* reconcile; r11 makes every *write* record-first.
  **(r18, cid 3329052679) The chokepoint is a `RefBackend`-trait seam — closed for BOTH
  backends, not just the file one.** Everything above privatized one raw publish: the file
  path's temp→rename (`refs_transactions.rs:228-256`). But the hosted backend `PgRefBackend`
  (`pg_refs.rs:35`, impl of `RefBackend`, `ref_backend.rs:15`) publishes refs through separate
  SQL — `PgRefBackend::update_refs` (`pg_refs.rs:324`) applies every thread/marker/HEAD update
  in one `pool.begin()…tx.commit()` (`:328`) — which a file-only closure never touches, leaving
  a second public raw publish with no record-first guarantee. So `commit_and_publish` is a
  **method on the `RefBackend` trait**; each backend's bare publish is private behind its own
  impl. They satisfy the invariant by different native mechanisms: the **file** backend by
  oplog-append-then-publish + per-read reconciliation (record and ref are two durable subsystems
  with a crash window — the entire reconciliation/watermark apparatus exists to bridge it); the
  **Postgres** backend by ONE SQL transaction that inserts the ref-carrying record row **and**
  the ref/head rows together (`PgOpLogBackend` shares the `PgPool`, `pg_oplog.rs:40`,`:259`), so
  "committed ⇔ the record row exists" is atomic and the Postgres path needs no temp→rename, no
  reconciliation, no watermark. Routed through the seam, `PgRefBackend::update_refs` becomes the
  private raw publish and `commit_and_publish` adds the record `INSERT` to the same tx. The
  forbidden state — a published ref with no backing record — is now unreachable on *both*
  backends' write paths.
- **Write side (2) — unbounded indexed exact-once commit (§2.2 "Idempotency of the
  commit," retained from r3).** The phase-4 linearization point deduplicates on an
  **unbounded, indexed `transaction_id` → committed-batch-id** lookup under the
  oplog write lock — *not* the window-bounded `record_batch_scoped_if_no_transaction`
  (`oplog_core.rs:281`). A crashed `execute` re-run at any timing — immediate, or
  delayed past *any* fixed window `N` — finds the prior `TransactionCommit` and
  refuses the second append. Window size stops being a correctness parameter.

These three are **independent**: the write chokepoint governs *how a commit is
published* (record first, with ref identity), the indexed dedup governs *that a commit
is written at most once*, and reconciliation governs *reading* a commit. Together —
every write records a ref-carrying entry before publishing, every commit appends at
most once, every read reconciles — they hold `committed ⇔ a ref-carrying oplog record
exists` universally, across every writer, reader, handle age, crash timing, and oplog
topology.

The one forbidden state — a NEW, committed-*looking* canonical ref with **no**
backing ref-carrying record — is structurally impossible regardless of
writer/reader/handle/timing, because the **write chokepoint** (§2.2) makes the
canonical-ref publish unreachable except through a primitive that has *already*
appended a ref-identifying record — **on every backend** (r18, cid 3329052679): the
seam is a `RefBackend`-trait method, so the file backend's temp→rename and the Postgres
backend's SQL publish (`pg_refs.rs:324`/`:328`) are *each* private behind their own
`commit_and_publish` (the file backend ordering append-then-publish across two
subsystems, the Postgres backend committing record + refs in one ACID transaction).
Nothing publishes a canonical ref before its
phase-4 record is durable, and the record names the ref it publishes (the
`Fork`/`Collapse` retrofit closes the last variants that did not);
and the only post-crash residue (a *lagging* OLD ref with the record already present)
is resolved at the read by reconciliation, on every path and handle age, not merely
at the next `open` — and, because the reconcile scopes **by ref class**, a
**local**-ref read in a shared-oplog setup resolves only its own lane (never lifting
a co-tenant's HEAD), while a **shared**-ref read folds all lanes (never missing a
co-tenant's committed-but-unpublished thread/marker/remote write). That is the
close-the-class result: not a covered matrix, but a single invariant —
`committed ⇔ a ref-carrying oplog record exists`, with the recovery domain set by ref
class (per-`op_scope` for local, global for shared) — enforced in the **shared write
path** (the chokepoint records before publishing) *and* the **shared read path** (the
primitive reconciles before trusting). The impl epic (§6 O1, O7, O9) carries the three
mechanisms — the write chokepoint + the `Fork`/`Collapse` ref-carrying retrofit, the
per-read reconciliation hook, and the unbounded index — as the concrete deliverables;
the `open`-time materialization survives only as the eager fast-path that prefetches
what the read would otherwise do lazily (§2.2), never as the guarantee.

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

> A sub-op must be an **`EagerMutation`** (§3.3) **iff its forward effect is
> durable state that a different process or a different repo handle can read, and
> the correctness of *that other reader* depends on seeing the effect before the
> outer transaction commits.** Everything else is a `DeferredMutation`.

Operationally, a sub-op is eager (implements `EagerMutation`) iff **both**:
1. its effect lands in a **cross-process-visible** store (a file other processes
   stat/read under a shared lock — the dedup store
   `operation_dedup.rs:216`/`acquire_file_lock`, a ref another process
   resolves), **and**
2. some external actor's behavior **branches on observing it** mid-transaction
   (the racing process backing off; a child process the op spawns and waits on —
   `operation_id.rs:145-162`).

A savepoint op's effects are only read by *this* transaction until commit
(staged object, staged FS, a ref temp file no other process resolves until phase 5
publishes it), so deferring is safe.

**How an eager sub-op participates in outer rollback.** It commits eagerly inside
`EagerMutation::commit_eager` — NOT inside `apply` (e.g. `store.reserve` returns
`Reserved`) — and `commit_eager` *returns* the compensator, which `enroll_eager`
**registers** with the outer `Tx` (`RewindEntry::Compensator`). Tying the eager
effect and the compensator into one method's body+return value is what makes a
missing rollback entry unrepresentable (§3.3). If the outer
transaction later fails, `rewind_all` runs that compensator — for the reserve,
the compensator is a no-op (`operation_dedup.rs:201`), because a handed-out
op-id reservation must remain durable even when an unrelated outer transaction
aborts. Releasing it would reopen the #251 cross-process reuse race. The
compensator is saga semantics for that one leg: the effect was really visible
for a while, and the compensator defines the correct *net* outcome. Eager legs
commit in apply-order; their compensators run in reverse with everything else.

### 3.3 — The compile-time enforcement (a type-level split, not a runtime const)

An eager op without a real compensator is a silent disaster (a leaked
reservation). The enforcement must be a **compile error**, not a
`debug_assert!` — a `debug_assert!(M::COMMIT_KIND == Eager)` vanishes in release
builds, so an op whose effect is genuinely eager could be enrolled through the
savepoint path with no compensator wired, in exactly the production builds that
matter. The earlier `COMMIT_KIND` associated-const sketch had this hole: a single
`enroll<M: AtomicMutation>` accepted *every* mutation, and the const-vs-kind
agreement was checked only at runtime.

Make the wrong combination unrepresentable by splitting the commit discipline at
the **type** level — two distinct sub-traits, each gating its own enroll path:

```rust
/// Opt-in marker for a deferred-commit op: when enrolled it defers its commit
/// marker to the outermost transaction and is unwound by the shared rewind
/// ledger if that transaction fails (§3.1). It makes NO invisibility claim —
/// real consumers (undo/redo) do immediately-visible writes and rely on the
/// ledger only for failure-atomicity. There is deliberately NO blanket
/// `impl<M: AtomicMutation> DeferredMutation for M` — an op opts in by
/// implementing this explicitly, so an op that is ONLY `EagerMutation` does
/// NOT satisfy the `enroll` bound and cannot be enrolled as a deferred child.
pub trait DeferredMutation: AtomicMutation {}

/// An eager op: its forward effect is cross-process-visible the instant it
/// runs (§3.2), so it must commit eagerly AND hand back a compensator. The
/// eager effect lives HERE, never in `apply` — `commit_eager` performs it and
/// *returns* the `Compensator`, so "perform the eager effect" and "produce the
/// compensator" are the same call. The method is required, so an eager op with
/// no compensator cannot implement the trait at all.
pub trait EagerMutation: AtomicMutation {
    /// Runs the eager, cross-process-visible effect (e.g. `store.reserve`) and
    /// returns the compensator the outer `Tx` stores. Separate from `rewind`
    /// because an eager leg's undo is a *forward* compensating action
    /// (cancel/release), not a staged-state rollback.
    fn commit_eager(&mut self, tx: &mut Tx<'_>) -> Result<Compensator>;
}

impl<'a> Tx<'a> {
    /// Savepoint enroll — bounded to `DeferredMutation`. Runs only `apply`
    /// (staged, reversible). An `EagerMutation`-only op fails this bound.
    pub fn enroll<M: DeferredMutation>(&mut self, m: M) -> Result<StagedCommit<M::Output>> { … }

    /// Eager enroll — bounded to `EagerMutation`. Stages via `apply`, then runs
    /// `commit_eager` and registers the returned `Compensator` into the ledger
    /// atomically. The compensator is guaranteed to exist because the bound
    /// requires the method that produces it.
    pub fn enroll_eager<M: EagerMutation>(&mut self, m: M) -> Result<M::Output> { … }
}
```

Why this closes the hole the `COMMIT_KIND` sketch left open:

- **`enroll` is bounded to `DeferredMutation`.** Passing an op that implements
  only `EagerMutation` is a hard **compile error** (`the trait bound
  ReserveOpId: DeferredMutation is not satisfied`) — not a release-eliding
  assert. There is no blanket `DeferredMutation` impl, so eager ops do not
  silently acquire savepoint-enrollability.
- **`enroll_eager` is bounded to `EagerMutation`, whose sole method *returns*
  the `Compensator`.** An op that declares itself eager but supplies no
  compensator cannot implement `EagerMutation` (the method is required) and so
  cannot be passed to `enroll_eager` — again a compile error.
- **The eager effect lives only in `commit_eager`, never in `apply`.** This is
  the load-bearing structural rule: even if an op were (wrongly) given *both*
  marker impls, enrolling it via `enroll` runs only `apply`, which by contract
  performs no eager, reader-visible effect — so the reservation is never made
  eagerly and there is nothing to leak. The compensator can only fail to be
  registered if the eager effect was never performed.

The result: **you cannot enroll an eager sub-op without handing the executor a
compensator, and you cannot do it in a release build that a `debug_assert!`
would have skipped — it simply does not compile.** No `COMMIT_KIND` const, no
runtime kind-check.

> Note on mutual exclusivity. Stable Rust has no negative bounds, so the type
> system cannot *forbid* an op from implementing both `DeferredMutation` and
> `EagerMutation`. The structural rule above ("eager effect only in
> `commit_eager`") makes a double-impl harmless rather than dangerous; sealing
> the two traits behind a single `CommitDiscipline` associated type to make them
> mutually exclusive is a belt-and-suspenders option carried to the impl epic
> (§6 O6) — it is not required for the compile-error guarantee, which the bound
> split already delivers.

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
        // savepoint sub-op: stage the recovery ref (temp file; its committed
        // `UndoRecoveryUpdate` record (§2.2, r9) is the commit point, the ref is
        // published post-commit in phase 5, rewind = unlink the temp). Undo-recovery
        // is a LOCAL (per-checkout) ref — sibling of this worktree's HEAD
        // (refs_storage.rs:89-93) — so it reconciles within THIS op_scope (r10),
        // never lifting a sibling checkout's recovery pointer.
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
        Ok(StagedCommit { output: …, oplog: vec![/* ThreadCreate */] })
    }
}
```
A failure at any leg unwinds the prior legs in reverse — no half-started thread,
and the user's pre-existing `--path` directory survives. Nests: yes. Eager: no.

### 5.3 — `capture` / `snapshot`  (#198-adjacent) — **third priority**

**Today** (`repo/src/repository_snapshot.rs:52`): object → **ref publish** →
oplog (§2.2), with `abort_snapshot_write_batch` (`:314`) covering only the object
batch and a fault checkpoint at `:233`. The ref is renamed onto its canonical
path (`refs_transactions.rs:230`) *before* the oplog append (`:252`), so a crash
in between leaves a reader-visible ref with no undo record.

**As an `AtomicMutation` (leaf, no nest, no eager leg):** the migration's
*behavioral* change is to make the oplog append the **sole commit** and move the
ref publish to **after** it as a post-commit materialization (§2.2 phases 4→5):

1. object-store leg uses the cheap no-op rewind (orphan + gc, §1.3);
2. ref leg *stages* the new value into a temp file (`write_string_temp`,
   `refs_transactions.rs:219-224`) under phase 3 — CAS-validated against the
   on-disk value but **not** renamed; its rewind is "unlink the temp file," since
   no canonical ref was published pre-commit;
3. the commit is the oplog append deduplicated by the **unbounded indexed
   `transaction_id` lookup** (§2.2 "Idempotency of the commit") — *not* the
   window-bounded `record_batch_scoped_if_no_transaction` (`oplog_core.rs:281`),
   so a delayed crash-retry stays exact-once;
4. only then (phase 5) does the executor rename the ref temp onto the canonical
   path + `sync_directory`, publishing it to lock-free readers.

A crash before the append publishes nothing (canonical ref untouched, temp file
swept). A crash after the append but before the rename is repaired by **per-read
reconciliation** (§2.2 "Reader model"): the next read of that ref — through the
single `reconciled_load` primitive every `RefManager` read funnels through (the
chokepoint below all ~124 direct `repo.refs()` readers), on *any* path and *any*
handle age, including
the daemon's long-held `Arc<Repository>` (`local_daemon.rs:330`,
`transaction.rs:143-152`) that never re-opens — sees the oplog generation has
advanced, folds the committed `OpRecord::Snapshot { new_state, thread }`
(`oplog_types.rs:18-22`) from the tail, and resolves the committed target (lazily
re-publishing the ref). A capture batch touches **both** ref classes — the
**thread** ref (shared, reconciled across all lanes) and `HEAD` (local, reconciled
within this `op_scope`) — so it advances **both** watermarks (§2.2 r10); a sibling
worktree's read of the now-committed shared thread ref folds capture's `Snapshot`
record even though capture ran under a different lane, while each worktree's `HEAD`
stays per-lane. The `Repository::open` (`repository.rs:594`) eager pass is
an optimization on top, not the guarantee — even a daemonless `heddle
capture`/`status` is correct without it, and a long-lived daemon stays correct
*because* the guarantee is in the read, not the open. This **closes** the
ref-moved-but-not-recorded window for every reader, handle, and timing rather than
merely shrinking it (or covering only freshly-opened readers) — there is no longer
any ordering in which a reader trusts a published-but-unrecorded or
committed-but-lagging ref. This is the cleanest demonstration that the
primitive *strengthens* an existing contract rather than just refactoring it.
Nests: no. Eager: no.

### 5.4 — op-id reserve  (#251) — **the eager-commit exemplar, fourth priority**

**Today** (`cli/src/operation_id.rs:62` `run_local_idempotency_if_requested`):
`store.reserve(op_id, command_name, request_hash)` (`:115`,
`operation_dedup.rs:362`) returns `Reserved` / `Replay` / `InFlight` / `Conflict`
(`operation_dedup.rs:104`); on `Reserved` it spawns the child, then `store.record`
(`:162`) or `store.cancel` on spawn failure (`:152`).

**As an `EagerMutation` sub-op:** when an op-id-bearing command is itself wrapped
in a transaction, the reserve is the canonical eager leg — enrolled via
`enroll_eager` (bounded `M: EagerMutation`, §3.3), never `enroll`:
```rust
impl EagerMutation for ReserveOpId {
    fn commit_eager(&mut self, _tx: &mut Tx) -> Result<Compensator> {
        let outcome = self.store.reserve(self.op_id, self.verb, self.hash)?;
        self.claim.set(outcome);
        Ok(Compensator::new(|| Ok(())))   // outer rollback intentionally keeps the handed-out id
    }
}
```
The reservation is visible to other processes the instant `reserve` returns
(cross-process file lock, `operation_dedup.rs:362`, `:370`) — it **cannot** defer to the
outer commit (§3.2 rule, both conditions met: cross-process store + racing
process branches on it). On outer rollback the compensator is intentionally a
no-op: once an op-id has been handed out, an unrelated outer abort must not free
it for reuse. Releasing that reservation would reintroduce the #251
cross-process reuse race; durability-on-reserve is the whole point of using
`EagerMutation` here.
Eager: **yes** — this is the whole reason `EagerMutation` + the `enroll_eager`
path exist.

### 5.5 — ref-write paths (already in-domain-atomic)

`update_refs(&[RefUpdate])` (`refs_manager.rs:319`) is already atomic + reverse-
rollback **within the refs domain** (§1.1). These do not need migration on their
own; they become the executor's "stage refs" leg (§2.2 phase 3). The win is only
realized when a ref write is *combined* with an oplog append or an FS effect in
one mutation (capture, undo) — which §5.1–5.3 cover.

**Exception (closed in r9): the direct-write setters that bypass `update_refs`.**
`set_remote_thread` (`refs_manager.rs:261`), `delete_remote_thread` (`:284`), and
`set_undo_recovery` (`:242`) do **not** go through `update_refs`/`RefUpdate` — they
take `lock_refs()` and `write_string`/`remove_file` the ref directly, with **no**
oplog append (cid 3328869364). They were the last ref-write paths that published a
committed-looking ref with no backing `OpRecord`, leaving `get_remote_thread`/
`list_remote_threads`/`list_remotes`/`get_undo_recovery` reconciling against an empty
tail. r9 folds them into the commit model: each appends its new `OpRecord`
(`RemoteThreadUpdate`/`RemoteThreadDelete`/`UndoRecoveryUpdate`, §2.2) as the phase-4
commit point, then publishes the ref as a phase-5 materialization. After r9 there
are **zero** ref-write paths that record *nothing*.

**The write chokepoint generalizes r9 to ALL writers (r11, cid 3328926767).** r9
fixed the two setters that appended *no* record; but two more writers recorded the
*wrong order* and a *ref-blind* shape. `cmd_fork` (`fork.rs:74-92`) publishes the
thread+HEAD (`update_refs` `:85` / `write_head` `:88`) **before** `record_fork`
(`:94`), and `cmd_collapse` (`collapse.rs:99-108`) publishes (`set_thread` `:101` /
`write_head` `:104`) **before** `record_collapse` (`:112`) — phase-5-before-phase-4 —
and neither `OpRecord::Fork { from, new_state }` (`oplog_types.rs:38`) nor `Collapse {
sources, result }` (`:40`) names the ref it published — and `cmd_fork` additionally
passed `record_fork`'s args reversed (`oplog_records.rs:113` vs `fork.rs:94-95`, r15).
Rather than patch these two call sites (the drip pattern at the writer level), r11
routes **all** ref publication through a single **write chokepoint** — one
commit-then-publish primitive `commit_and_publish(op_records: &[OpRecord], ref_updates:
&[RefUpdate])` that appends the **caller-supplied** ref-carrying record **batch** (phase
4) before publishing the atomic ref batch (phase 5) without splitting it. It takes a
**batch** rather than a single record (r16) so a multi-record atomic op — thread-rename's
new-thread+old-thread-delete (± HEAD) ref vector (`thread.rs:3074-3100`) backed by a
two-record `ThreadCreate`+`ThreadDelete` batch (`oplog_records.rs:96-110`) — **three
records when HEAD was attached to the renamed thread and the vector therefore carries a
`RefUpdate::Head` (`thread.rs:3090-3099`), the HEAD-move record added per
full-batch-record-coverage (r17, cid 3329019021)** — commits
all its records then publishes its atomic ref batch as one unit, never dropping a record
or splitting the batch (and never leaving a published ref without a backing record). It
*takes* the caller's `OpRecord`s rather than synthesizing them (r15): the generic write
methods see only ref identity + target, not the `sources`/`from` the `Collapse`/`Fork`
variants need, so the semantic callers build their full record and plain ref edits pass
a generic one — but **all** publication funnels through the one primitive, so the
**46** direct ref-write call sites (§2.2 "The write chokepoint") inherit correct
ordering + atomicity by construction, and the published thread name + HEAD are carried
by **extending the existing `Fork`/`Collapse` variants in place** (extra fields on the
existing variant bodies, discriminant indices unchanged) — `cmd_fork` rebuilding
`Fork { from: source_state, new_state, thread, head }` with the corrected arg order.
Pre-1.0 with no users and no production oplogs,
changing those payloads is a clean format break — any old dev-only logs are discardable,
so no migration, no compat shim, no versioned `…V2` variants. After
r11 there are **zero** ref-write paths that publish without a *preceding, ref-carrying*
record — the write-side mirror of r7's read chokepoint. (The 46 sites are not migrated
to `AtomicMutation` here; they simply route their existing publishes through the
chokepoint, exactly as readers were not migrated to `execute` but route through
`reconciled_load`.)

**The chokepoint is per backend (r18, cid 3329052679).** All of the above closes the *file*
backend's publish (`RefManager` → `update_refs_with_lock` → temp→rename). The hosted backend
`PgRefBackend` (`pg_refs.rs:35`) is a second `RefBackend` impl (`ref_backend.rs:15`) whose
`update_refs` (`pg_refs.rs:324`) publishes refs via its own SQL transaction (`:328`), untouched
by a file-only closure. r18 makes `commit_and_publish` a **`RefBackend`-trait method** both
backends implement, each with a private raw publish: the file backend keeps
oplog-append-then-publish + reconciliation (two subsystems, crash window), while
`PgRefBackend::commit_and_publish` inserts the ref-carrying record(s) and the ref/head updates
in **one** `pool.begin()…tx.commit()` (`PgOpLogBackend` shares the pool, `pg_oplog.rs:40`,`:259`)
— native ACID atomicity, no reconciliation or watermark. So "zero ref-write paths without a
preceding ref-carrying record" holds for the Postgres backend too, by its own mechanism.

### 5.6 — Inventory summary

| Site | File:line | Nests? | Eager leg? | Priority | What the primitive fixes |
|---|---|---|---|---|---|
| undo/redo | `undo.rs:93` | yes | no | 1 | mid-apply leaves repo half-rewound |
| thread start | `thread.rs` `cmd_start` (`:1709`+) | yes | no | 2 | half-started thread; precise dir rewind (#302 r4) |
| capture | `repository_snapshot.rs:52` | no | no | 3 | ref-moved-but-not-recorded window; ref publish becomes a post-commit materialized view |
| op-id reserve | `operation_id.rs:115` | as sub-op | **yes** | 4 | eager-commit exemplar; durable handed-out id survives outer rollback |
| ref writes | `refs_manager.rs:319` | n/a | no | — | already in-domain atomic; becomes the "stage refs" leg |
| remote/undo refs | `refs_manager.rs:242`,`:261`,`:284` | n/a | no | — | were direct-write, no `OpRecord` (cid 3328869364); r9 routes through oplog-commit + new variants so reconciliation is non-vacuous |
| fork | `fork.rs:74-92` | no | no | — | published thread+HEAD *before* `record_fork` (`:94`), passed `record_fork`'s args **reversed** (`oplog_records.rs:113` vs `:94-95`, r15), and `OpRecord::Fork` was ref-blind (cid 3328926767); fix: `cmd_fork` builds `Fork { from: source_state, new_state, thread, head }` (corrected order + published ref) and calls `commit_and_publish(&[op_record], ref_updates)` (one-element batch; r11 chokepoint, records-first; existing variant extended in place, pre-1.0 clean format break, no shim) |
| collapse | `collapse.rs:99-108` | no | no | — | published thread/HEAD *before* `record_collapse` (`:112`) and `OpRecord::Collapse` was ref-blind (cid 3328926767); fix: `cmd_collapse` builds `Collapse { sources, result, <published-ref> }` and calls `commit_and_publish(&[op_record], ref_updates)` (one-element batch; r11 chokepoint, records-first; existing variant extended in place, pre-1.0 clean format break, no shim) |
| thread-rename | `thread.rs:3061-3116` | no | no | — | publishes an **atomic ref batch** — create-new + delete-old (± HEAD move) in one `update_refs` vector (`:3074-3100`) — backed by a **two-record** batch (`ThreadCreate`+`ThreadDelete`, `oplog_records.rs:96-110`); the **multi-record motivator for the batch chokepoint (r16, cid 3329003333)**: a single-`OpRecord` primitive would drop a backing record or split the atomic batch. Fix: `cmd_thread_rename` calls `commit_and_publish(&[create, delete], ref_updates)` — full record batch + atomic ref batch, published together |
| pg ref backend | `pg_refs.rs:35`,`:324`,`:328` | n/a | no | — | second `RefBackend` impl (`ref_backend.rs:15`); `update_refs` published thread/marker/HEAD via its **own** SQL `pool.begin()…tx.commit()`, bypassing the file temp→rename seam (cid 3329052679); fix (r18): `commit_and_publish` is a **`RefBackend`-trait method** — `PgRefBackend` inserts the ref-carrying record(s) + ref/head rows in **one** SQL tx (`PgOpLogBackend` shares the pool, `pg_oplog.rs:40`,`:259`), native ACID atomicity, no reconciliation/watermark; its raw SQL publish made private behind the seam |

---

## §6 — Open questions / risks (carry into the impl epic)

- **O1 — Reordering capture + adding per-read reconciliation is a behavior
  change.** Making the oplog append the sole commit and moving the ref publish
  *after* it (§2.2 phases 4→5) changes the crash window the R7 test
  (`fault_injection.rs:157`) pins. The impl must (a) add a new fault checkpoint
  `*_after_oplog_before_ref_publish`, (b) test that a crash there leaves the ref
  at its OLD value with the oplog entry present, and (c) test that the **next read
  reconciles** to the committed target — i.e. the `committed ⇒ read resolves the
  committed value` half — **on three reader shapes**: a direct-CLI invocation with
  **no daemon running**, a **freshly-opened** handle, and crucially a
  **long-held `Arc<Repository>`** handle that opened *before* the crash and reads
  *after* it (the daemon shape, `local_daemon.rs:330` / `transaction.rs:143-152`)
  — the last is the cell an open-time-only pass would miss (cid 3328112197). The
  pre-commit half (crash before the append publishes nothing; temp file swept) is
  the cheaper test. Strictly strengthens the contract, but the reconciling
  `RefManager` read methods (plus the injected `RefReconciler`) are real new code,
  not just a reorder.
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
- **O6 — Sealing `DeferredMutation` ⊻ `EagerMutation` (belt-and-suspenders).**
  §3.3's compile-error guarantee does not require the two markers to be mutually
  exclusive — the "eager effect only in `commit_eager`" structural rule makes a
  double-impl harmless. If the impl wants the type system to also *forbid* a
  double-impl, seal both behind one `CommitDiscipline` associated type (an op
  declares exactly one discipline; the markers become blanket impls keyed on it).
  Stable Rust has no negative bounds, so this is the only way to make "both" a
  compile error. Decide in impl; not needed for the no-leaked-compensator
  guarantee.
- **O7 — The two close-the-class mechanisms are net-new code, with real cost
  trade-offs (§2.2, §2.4).** Neither exists today; both must be built and both
  carry a decision:
  - **Per-read reconciliation via the single load primitive (the guarantee) +
    open-time eager pass (optimization).** Correctness lives in the read: every ref
    resolve reconciles against the committed oplog tail (§2.2 "Reader model"). The
    reconciliation lands **inside one internal `reconciled_load` primitive** that is
    the sole path for **logical reads** in `RefManager` to touch raw ref storage; all
    **ten** public read methods (point, list, remote, undo-recovery, `resolve` —
    `refs_manager.rs:114`–`:327`) obtain ref data only through it, and the raw
    loaders (`read_change_id_at`, `read_head_state`, `try_read_ref_summary_index`,
    `*_from_storage`, `PackedRefs::load`) are reachable from a logical read **only**
    from inside it (enforced by a one-line conformance check whose target is
    *logical-read* call-sites, with an explicit allowlist for the **maintenance**
    path `pack_refs` — `refs_manager.rs:337-380`, run by `heddle gc` `gc.rs:104` —
    which touches four of those loaders directly to compact storage but serves no
    logical value, cid 3328894984; the impl must encode this exemption rather than a
    naive "exactly one caller" assert, which `pack_refs` would trip). This is
    **not** at the `Repository`
    accessors, because an accessor-layer hook is bypassed by the ~124 direct
    `repo.refs()` readers (`cli/src/cli/commands/status.rs:1795`,
    `cli/src/cli/commands/collapse.rs:99`, `client/src/grpc_remote/sync.rs:588`,
    `:1001`, `thread.rs:507`, `marker.rs:64`, …); and it is **not** a hand-picked
    subset of read methods (r6's three-method hook left the four `list_*`, both
    remote, undo-recovery, and `resolve` readers bypassing — cid 3328832780). Any
    *future* `RefManager` read method is covered automatically, since it has no raw
    storage to read except through the primitive. `RefManager` lives in the `refs`
    crate, which does not depend on `oplog` (`crates/refs/Cargo.toml`); the seam
    clears that via **dependency inversion** — a `RefReconciler` trait defined *in*
    `refs` (over `Head`/`ChangeId`/ref-name/`head_id`), held as
    `Option<Arc<dyn RefReconciler>>` via a `with_reconciler(..)` builder (cf.
    `with_local_head` `refs_manager.rs:50`), with the concrete oplog-backed impl
    injected from the `repo`/`oplog` layer at `Repository` construction
    (`crates/repo/Cargo.toml:22` oplog, `:24` refs) and invoked inside
    `reconciled_load`. Impl work: implement the trait + injection + the in-primitive
    reconcile call — **no per-call-site routing**: precisely *because* the primitive
    is below all ~124 readers, every reader (and every `Repository` accessor, and
    the daemon handler `transaction.rs:143-152`) inherits reconciliation unchanged.
    **Cost:** the hot path must be near-free or it taxes every read
    (daemon RPC and `heddle log`/`status` alike) — hence the O(1) generation check
    on the oplog `head_id` (`packed_oplog.rs:26`, the file's leading field `:55`),
    so a read that finds the **class-selected** watermark unchanged vs `head_id`
    returns immediately with no tail scan and no write; full reconcile (and lazy
    re-publish) only on the rare advanced-generation lag. The cached generation is a
    **watermark of fully-materialized batches, split by ref class** (r10, cid
    3328894983): **two** cells — a per-`op_scope` watermark guarding local-ref reads
    (HEAD, undo-recovery) and a **global** watermark guarding shared-ref reads
    (thread, marker, remote-thread) — each compared against the same `head_id`. A
    lagged read materializes **every** ref *of its class* in every lagged committed
    batch *before* advancing that class's watermark (batch-atomic, cid 3328853451),
    so the gate reads "current" iff no ref of its class in any batch ≤ it is stale —
    a partial single-ref reconcile must never advance it, else a batch sibling (point
    *or* list read) would observe stale storage behind a "current" gate. A committed
    batch advances whichever watermark(s) match the ref classes it touched (a
    `capture`, touching HEAD + a thread ref, advances both). The local-ref scan keeps
    the `Some(&op_scope())` filter (r5, scoped down to local refs); the shared-ref
    scan is **unfiltered/global** — filtering a shared ref to a lane would miss a
    co-tenant's committed-but-unpublished shared write (the correctness bug cid
    3328894983 fixes). This needs a cheap
    `OpLog::head_id()`/`tip()` header accessor (net-new). The `Repository::open` (`repository.rs:594`) eager materialization
    is kept as an *optional* prefetch — it repairs lag once at open so subsequent
    reads on that handle skip even the reconcile — but is **not** load-bearing and
    may be dropped; it must not itself need the recovery it provides (bootstrap
    ordering inside `open`). A per-ref committed index (vs the two class-split
    `head_id` watermarks) is an optional refinement to avoid reconciling a read when
    a *different* ref advanced — but it must stay batch-atomic **and** preserve the
    per-class scope (advance the per-ref watermark for **every** ref a reconcile
    materialized, not just the requested one, and keep local refs `op_scope`-scoped
    while shared refs stay global), or it
    reintroduces the sibling gap cid 3328853451.
  - **Unbounded indexed `transaction_id` map.** The exact-once commit needs a
    `transaction_id → committed-batch-id` index maintained under the oplog write
    lock and persisted atomically with the log (so it can never disagree). This
    replaces the window-bounded `record_batch_scoped_if_no_transaction`
    (`oplog_core.rs:281`) *as the linearization point* — that helper stays for the
    bounded rebase path. Open sub-questions: index persistence format (sidecar vs.
    derived-on-load from a full scan), and whether to GC the index for very long
    histories (it grows with distinct transaction ids). The full-tail-scan
    fallback is the zero-new-state correctness floor if a sidecar is undesirable.
- **O8 — New oplog record variants for remote-thread + undo-recovery (r9, cid
  3328869364), and the additive format bump they imply.** Bringing
  `set_remote_thread`/`delete_remote_thread`/`set_undo_recovery`
  (`refs_manager.rs:261`/`:284`/`:242`) into the oplog-commit model (§2.2 "Remote-thread
  and undo-recovery writes are oplog-committed too") adds three `OpRecord` variants —
  `RemoteThreadUpdate`, `RemoteThreadDelete`, `UndoRecoveryUpdate` — appended at the
  enum tail after `GitCheckpoint` (`oplog_types.rs:222-228`), per the
  discriminant-stability rule (`:12-14`: rmp-serde encodes variants by index, so new
  variants append at the tail and never reorder). The impl must: (a) add the three
  variants + their `description()` arms (`oplog_types.rs:231`+); (b) route the three
  setters through the phase-4 oplog-commit point + phase-5 publish, supplying the
  oplog the same way the `RefReconciler` is injected (the `refs` crate has no `oplog`
  dep, so the append is driven from the `repo`/`oplog` layer, not from inside
  `RefManager`); (c) add replay/reconcile handling for the new variants in
  `reconciled_load` and crash-replay (delete-record ⇒ drop from the reconciled list
  set; update-record ⇒ set the point value). **Format implication:** this changes the
  persisted oplog record set, so the oplog format is extended. Per the pre-1.0
  no-backcompat stance this is a **straight additive** change — new tail variants,
  **no migration shim** — so the impl must NOT over-engineer a versioned migration;
  old on-disk logs simply never contain the new variants and continue to read.
  This is a **format-stability-sensitive** change (new persisted record types) and
  should be reviewed as such.
- **O9 — The write chokepoint + the `Fork`/`Collapse` ref-carrying retrofit (r11,
  cid 3328926767), and the additive format change they imply.** The read-side
  guarantee (O7) is only sound if every published ref *has* a preceding ref-carrying
  record; r11 makes that structural on the write side. Impl work: **(a)** route the
  canonical-ref publish (temp→rename, `refs_transactions.rs:230`) so it is reachable
  **only** through one commit-then-publish primitive
  `commit_and_publish(op_records: &[OpRecord], ref_updates: &[RefUpdate])` that appends
  **all** of the **caller-supplied** record batch `op_records` (phase 4) before publishing
  the atomic `ref_updates` batch (phase 5) **without splitting it** — the bare publish
  becomes a private sub-step, mirroring how r7 made the raw loaders private below
  `reconciled_load`. **The signature takes a record *batch*, not a single `OpRecord`
  (r16, cid 3329003333)**, because some atomic ref batches back multiple records:
  thread-rename publishes create-new + delete-old (± HEAD) in one atomic `update_refs`
  vector (`thread.rs:3074-3100`) while `record_thread_rename` records a two-record batch
  (`ThreadCreate`+`ThreadDelete`, `oplog_records.rs:96-110`) — which, per r17 (cid
  3329019021), must become a *three*-record batch in the attached-HEAD case so the
  published `RefUpdate::Head` (`thread.rs:3090-3099`) has a backing record; see (e). A
  single-`OpRecord`
  signature would force such a writer to drop a backing record or split the atomic batch
  into multiple publishes (breaking atomicity); the batch signature commits every record
  then publishes the whole atomic ref batch together. Single-record ops pass a one-element
  batch. The primitive **takes** the records rather than
  synthesizing them (r15, cid 3328979498), because the generic write methods
  (`set_thread`/`write_head`/`update_refs`) see only ref identity + target, not the
  `sources`/`from`/thread context the `Collapse`/`Fork` variants need — synthesizing
  would lose those semantics or emit a duplicate generic record; the thin
  `set_thread`/`write_head`/`set_marker` wrappers stay for plain ref edits and build a
  one-element generic ref-update record before calling the primitive. Because `op_records`
  carry an
  `oplog` type carrying data `refs` does not own (`Fork`/`Collapse` `sources`/`from`),
  `commit_and_publish` lives at the **`repo`/`oplog` layer** (which depends on both,
  `crates/repo/Cargo.toml:22`,`:24`); `refs` exposes only a private publish sub-step it
  drives, so `refs` still gains no `oplog` dep — the write-side asymmetry with the
  `RefReconciler` (which *can* live in `refs` since its outputs are `refs` types).
  **(b)** Add a one-line conformance check (the write-side analog of O7's read-side
  check) asserting the raw publish has no caller but the primitive — so the 46 sites
  and any future writer cannot publish around it. **(c)** **Extend the existing `Fork` (`:38`) / `Collapse` (`:40`) variants
  in place** — add fields to the existing variant bodies, discriminant indices unchanged:
  `Fork { from, new_state, thread: Option<String>, head }` (published thread name, `None`
  for detached, + HEAD) and `Collapse { sources, result, <published-ref discriminant> }`
  (thread name or detached-HEAD marker), modelled on the field shape of existing variants
  like `ThreadUpdate { name, state }`. **heddle is pre-1.0 with no users and no production
  oplogs**, so changing these payloads under their existing discriminant index is a clean
  format break — any old dev-only logs no longer deserialize those records, which is fine
  (nothing to migrate, discardable). **No migration shim, no compat shim, no versioned
  `…V2` variants** — that compatibility-preserving escape is the opposite of the
  no-backcompat stance and is not taken. Update the existing `Fork`/`Collapse` replay/
  reconcile arms to read the new fields (materialize the named thread/HEAD from the record).
  **(d)** Fix `cmd_fork` (`fork.rs:74-92`) and `cmd_collapse` (`collapse.rs:99-108`) to
  build their full `OpRecord` (populating the new published-ref fields) and publish via
  `commit_and_publish`, eliminating the current publish-before-record order. `cmd_fork`
  must **also correct the reversed `record_fork` arguments** (r15, cid 3328979497):
  `record_fork(from, new_state)` (`oplog_records.rs:113`) takes the source as `from` and
  the result as `new_state`, but today's call passes
  `(&new_state.change_id, &source_state.change_id)` (`fork.rs:94-95`) — source and
  result swapped — so the rebuilt `OpRecord::Fork { from, new_state, thread, head }`
  must set `from = source_state`, `new_state = new_state`. Once the variant carries the
  published thread/HEAD, **those published-ref fields — not the from/new_state
  positional pair — are the authoritative replay/materialization target**:
  reconciliation re-publishes the ref named by the explicit fields, so a future
  positional slip cannot mis-materialize the wrong ref. **Format implication:**
  extending `Fork`/`Collapse` is a
  **clean in-place format break**, pre-1.0, **no migration shim** (old dev logs are
  discardable; there are no production oplogs to preserve). It remains
  **format-stability-sensitive** and is reviewed as such (cf. O8) — not because old logs
  must survive, but because the new field shapes must land coherently across every
  reader/writer of these variants. **(e)** Route `cmd_thread_rename`
  (`thread.rs:3061-3116`) through the chokepoint with its **full record batch** — its
  existing `record_thread_rename` records (`ThreadCreate`+`ThreadDelete`,
  `oplog_records.rs:96-110`) handed in as `&[create, delete]` alongside the atomic
  `update_refs` vector it already builds (`thread.rs:3074-3100`). This is the concrete
  case that drives the batch signature in (a): the create + delete ref batch and its two
  backing records commit-then-publish as one atomic unit — no dropped record, no split
  publish. **And it must satisfy full-batch-record-coverage (r17, cid 3329019021): when
  HEAD was attached to the renamed thread the published vector gains a third
  `RefUpdate::Head` (`thread.rs:3090-3099`), so `record_thread_rename` must, for that
  case, emit a *third* HEAD-move record (or extend its record shape to carry the
  attached-HEAD move) — handed in as `&[create, delete, head_move]` — else that published
  HEAD ref has no replay record.** **(f)** Make the raw publish private so
  `commit_and_publish` is its **sole** caller (r17, cid 3329019023): the public writers —
  including the CAS wrappers `set_thread_cas`/`set_marker_cas` (`thread.rs:1647-1650`,
  `grpc_remote/mod.rs:334-338`), `create_marker`/`delete_marker`, and the `delete_*_cas`
  siblings — already converge on `update_refs` (`refs_manager.rs:319`) whose
  `update_refs_with_lock` temp→rename half (`refs_transactions.rs:228-256`, already
  `pub(super)`) is that one publish, so routing them through `commit_and_publish` is the
  structural closure, not a per-writer enumeration; the one-line write-side conformance
  check in (b) enforces it. The ref-write sites are **not**
  migrated to `AtomicMutation` here — they keep their existing publishes and merely
  route through the chokepoint; full `execute` migration is the §7 epic's job.
- **O10 — The write chokepoint is a `RefBackend`-trait seam, closed per backend (r18, cid
  3329052679).** O9 privatized one raw publish — the file path's temp→rename
  (`refs_transactions.rs:228-256`). But `RefBackend` (`ref_backend.rs:15`) has a second
  production impl, `PgRefBackend` (`pg_refs.rs:35`), whose `update_refs` (`pg_refs.rs:324`)
  publishes thread/marker/HEAD via its own `pool.begin()…tx.commit()` (`:328`) and never touches
  the file seam — so a file-only closure leaves a second public raw publish with no record-first
  guarantee. Impl work: **(a)** make `commit_and_publish(op_records, ref_updates)` a method on
  the **`RefBackend` trait** (not a free function over `RefManager`), so every backend implements
  it and the single-sole-writer invariant is stated **per backend**; the trait method names a
  **`refs`-defined committer trait** (the write-side dual of `RefReconciler`), not `OpRecord`, so
  `refs` keeps no `oplog` dep while the seam lives on the trait (this revises O9's "must sit at
  the repo/oplog layer" placement, which was what left the Pg publish uncovered). **(b)** File
  backend: `commit_and_publish` calls the injected committer to append to the file oplog, then
  runs its private temp→rename — two durable subsystems, so the crash window is bridged by the O7
  per-read reconciliation/watermark (unchanged). **(c)** Postgres backend:
  `PgRefBackend::commit_and_publish` does the ref-carrying record `INSERT` **and** the ref/head
  updates in **one** `pool.begin()…tx.commit()` — `PgOpLogBackend` (`pg_oplog.rs:39`) appends via
  `INSERT INTO oplog (…)` (`:259`) over the same `Arc<PgPool>` (`:40`) `PgRefBackend` holds
  (`pg_refs.rs:36`), so the record row and the ref/head rows commit atomically; "committed ⇔ the
  record row exists" holds **natively**, and the Postgres path needs **no** temp→rename, **no**
  oplog-replay reconciliation, **no** watermark. Route the existing `PgRefBackend::update_refs`
  (the SQL-tx-of-just-refs) into the seam as the **private** raw publish, with
  `commit_and_publish` adding the record `INSERT` to the same tx; the per-backend write-side
  conformance check asserts each backend's bare publish has only its own `commit_and_publish` as
  caller. **(d)** The impl must confirm the server constructs `PgRefBackend` and `PgOpLogBackend`
  over a **shared** pool so the record `INSERT` can join the refs transaction (both take
  `Arc<PgPool>` + `repo_id` today, `pg_refs.rs:46` / `pg_oplog.rs:50`); if a future deployment
  splits them across databases the Postgres path loses native single-tx atomicity and must fall
  back to the file backend's append-then-reconcile model. The test-only `MemRefBackend`
  (`backend.rs:127`) is non-durable and outside the invariant. Like O8/O9, the Postgres-side
  change touches a persisted format (the oplog row now co-committed with the refs) and is
  reviewed as **format-stability-sensitive**.

---

## §7 — Recommendation + follow-up impl epic

**Recommendation: build the primitive, `dyn`-free, and migrate in the priority
order above.** The bug class is real, recurring, and structurally closable; the
executor-enforces-once shape fits heddle's existing type-state idioms; and most
primitives it composes (CAS batch + reverse rollback, a *window-bounded*
idempotent oplog append, orphan-tolerant object store, atomic FS rename) already
exist — the work is sequencing them under one ledger, not inventing durability.
The genuinely net-new pieces are the close-the-class mechanisms (O7, O9), which now
span **both** sides of the commit. On the **write** side, a single **write chokepoint**
(O9, cid 3328926767; op-descriptor refinement r15, cid 3328979498; record-batch
refinement r16, cid 3329003333): every ref
publication routes through one commit-then-publish primitive
`commit_and_publish(op_records: &[OpRecord], ref_updates: &[RefUpdate])`, which appends
**all** of the **caller-supplied** ref-carrying record batch (phase 4) *before*
publishing the atomic canonical-ref batch (phase 5) without splitting it. It takes a
**batch** (r16) so multi-record atomic ops — thread-rename's create-new+delete-old
(± HEAD) ref vector backed by a two-record `ThreadCreate`+`ThreadDelete` batch
(`thread.rs:3074-3100`, `oplog_records.rs:96-110`), or **three records** when HEAD was
attached to the renamed thread so the published `RefUpdate::Head` (`thread.rs:3090-3099`)
gets a backing HEAD-move record (full-batch-record-coverage, r17, cid 3329019021) — are
faithful: every published ref in the batch has a backing record committed in the same
atomic publish; single-record ops pass a one-element batch. The
primitive **takes** the caller's operation descriptors rather than having the generic
write methods synthesize them (they see only ref identity + target, not the
`sources`/`from` the `Collapse`/`Fork` variants need): semantic callers (fork/collapse)
build their full record, thread-rename hands in its two-record batch, plain ref edits
pass a one-element generic ref-update record, and **all**
publications funnel through the one primitive — so the **46** direct ref-write call
sites — including `cmd_fork` (`fork.rs:74-92`) and `cmd_collapse`
(`collapse.rs:99-108`), which today publish *before* recording (and whose `record_fork`
call also passes its arguments reversed against the `record_fork(from, new_state)`
definition, `oplog_records.rs:113` vs `fork.rs:94-95`, r15) — inherit correct ordering
+ atomicity by construction, and the two ref-blind variants
(`OpRecord::Fork`/`Collapse`, `oplog_types.rs:38`,`:40`) are **extended in place** to
carry the published thread name + HEAD, which become the authoritative replay target —
pre-1.0 with no users and no production oplogs, a clean format break with no migration
shim, no compat shim, and no versioned `…V2` variant (old dev-only logs are
discardable).
The chokepoint is a **`RefBackend`-trait seam closed per backend (r18, cid 3329052679)**:
r17's single-sole-writer closure privatized only the *file* backend's temp→rename, leaving the
hosted `PgRefBackend`'s separate SQL publish (`pg_refs.rs:324`/`:328`) uncovered, so
`commit_and_publish` becomes a trait method every backend implements — the file backend earning
atomicity via oplog-append-then-publish + reconciliation (two subsystems, crash window) and the
Postgres backend via ONE SQL transaction inserting the ref-carrying record + ref/head rows
together (native ACID, no reconciliation/watermark); the record crosses the trait boundary
through a `refs`-defined committer trait (the write-side dual of `RefReconciler`), so `refs`
still names no `oplog` type.
On the **read** side,
**per-read reconciliation inside _one internal `reconciled_load` primitive_** —
the sole path for **logical reads** to touch raw ref storage (the maintenance path
`pack_refs` touches the loaders directly to compact storage but serves no logical
value and is an explicit, reasoned exemption, cid 3328894984), which all **ten**
public `RefManager` read methods (point, list, remote, undo-recovery, `resolve`)
funnel through, so reconciliation cannot be bypassed by any present reader, the ~124
direct `repo.refs()` readers, **or any read method added later** (raw storage is
unreachable from a logical read except through the primitive) — reached via a
`refs`-crate `RefReconciler` trait injected from `repo` (dependency inversion, so
`refs` keeps no `oplog` dep), and **scoped by ref class** (cid 3328894983): local
refs — `HEAD`, undo-recovery (beside the per-worktree HEAD, `refs_storage.rs:69-93`)
— reconcile within the current `op_scope`; shared refs — thread, marker,
remote-thread (under the shared ref root, `refs_storage.rs:50-67`) — reconcile
**globally** across all lanes (filtering a shared ref to a lane is a correctness bug
— it would miss a co-tenant's committed-but-unpublished shared write), so the
`committed ⇔ a ref-carrying oplog record exists` invariant holds with the recovery
domain set by ref class for every reader path, *handle age*, crash timing, and oplog
topology — including the daemon's long-held `Arc<Repository>` an open-time-only pass
cannot reach, and shared-oplog worktrees (local reads stay per-lane, shared reads span
lanes) — and an **unbounded indexed `transaction_id`
commit dedup** (so exact-once holds at any retry timing, not just within a 64-batch
window). The write chokepoint and the read primitive are duals — record-before-publish
on the write, reconcile-before-trust on the read — and together close the class on both
sides: no writer publishes a ref without a preceding ref-carrying record, and no reader
trusts a published ref without confirming it. The `Repository::open` eager
materialization is kept only as an optimization on top of the read-side guarantee.

### Proposed impl epic shape (blocked by this spike — confirm before filing)

> **Epic: atomic-mutation primitive — land `AtomicMutation` + migrate the
> recurring multi-step mutations.** Blocked by #330.

1. **#330-impl-a — land the primitive (no migrations).** `AtomicMutation`,
   `EagerMutation`, `Tx`, `execute`, the rewind ledger, `Drop` backstop, the
   **unbounded indexed `transaction_id` commit dedup** (O7 — *not* a reuse of the
   window-bounded `record_batch_scoped_if_no_transaction`), and **per-read
   reconciliation implemented *inside* one internal `reconciled_load` primitive**
   that all ten public `RefManager` read methods funnel through (point, list,
   remote, undo-recovery, `resolve`), via a `refs`-crate `RefReconciler` trait
   injected from `repo` (O7 — the read-side guarantee; the primitive is the sole
   path for **logical reads** to raw ref storage, below all ~124 `repo.refs()`
   readers, so **no per-call-site edits** and any future read method is covered
   automatically; reconciliation is **scoped by ref class** — local refs HEAD +
   undo-recovery within `op_scope`, shared refs thread/marker/remote-thread globally
   (cid 3328894983) — backed by **two** watermarks (per-`op_scope` + global); add the
   one-line conformance check that the raw loaders have no *logical-read* caller but
   the primitive, with an explicit allowlist for the `pack_refs` maintenance path,
   which legitimately touches the loaders to compact storage and serves no logical
   value (cid 3328894984); the `Repository::open` eager pass is an optional
   optimization on top).
   It also lands the **write chokepoint** (O9): the canonical-ref publish becomes
   reachable only through one commit-then-publish primitive
   `commit_and_publish(op_records: &[OpRecord], ref_updates: &[RefUpdate])` that appends
   **all** of the **caller-supplied** record batch before publishing the atomic ref batch
   (without splitting it) — a **batch**, not a single record (r16), so multi-record atomic
   ops (thread-rename's create+delete±HEAD, `thread.rs:3074-3100` /
   `oplog_records.rs:96-110`) commit every backing record then publish their atomic ref
   batch as one unit; single-record ops pass a one-element batch. The generic write
   methods cannot synthesize the
   record (they lack the `sources`/`from` the `Collapse`/`Fork` variants need, r15), so
   semantic callers pass their full record(s) and the thin `set_thread`/`write_head`/
   `set_marker` wrappers build a one-element generic one for plain edits — with a one-line
   write-side conformance check (the raw publish has no caller but the primitive, so
   all **46** ref-write sites and any future writer record-first by construction). The
   primitive lives at the `repo`/`oplog` layer (it names `OpRecord`, an `oplog` type),
   with `refs` exposing only a private publish sub-step, so `refs` keeps no `oplog`
   dep. It
   lands the three r9 `OpRecord` variants
   (`RemoteThreadUpdate`/`RemoteThreadDelete`/`UndoRecoveryUpdate`, O8) **and** the
   `Fork`/`Collapse` ref-carrying retrofit (O9 — the existing `Fork`/`Collapse` variants
   extended in place to carry the published thread name + HEAD / published-ref
   discriminant; pre-1.0 clean format break, old dev logs discardable), and routes
   `set_remote_thread`/`delete_remote_thread`/`set_undo_recovery`
   (`refs_manager.rs:261`/`:284`/`:242`) plus `cmd_fork` (`fork.rs:74-92`),
   `cmd_collapse` (`collapse.rs:99-108`), and `cmd_thread_rename` (`thread.rs:3061-3116`,
   passing its full record batch — two records, **or three when HEAD was attached to the
   renamed thread** so the published `RefUpdate::Head` (`thread.rs:3090-3099`) gets a
   backing HEAD-move record, full-batch-record-coverage r17) through `commit_and_publish`
   (oplog-commit point + phase-5 publish), with `cmd_fork` also rebuilding its record with
   the corrected, non-reversed `record_fork` arg order (`from = source_state`, r15). The
   raw publish (`update_refs_with_lock`'s temp→rename half, `refs_transactions.rs:228-256`)
   is made private so `commit_and_publish` is its **sole** caller (r17), and the public
   CAS/create wrappers (`set_thread_cas`/`set_marker_cas`/`create_marker`) — which already
   converge on `update_refs`, `refs_manager.rs:319` — delegate through it, so no
   ref class and no writer is a direct-write/wrong-order/reversed-arg/uncovered exception and
   the ten-reader reconciliation is non-vacuous (the oplog-format change — new tail
   variants for the r9 remote/undo classes + in-place extension of `Fork`/`Collapse`,
   no migration shim — is part of this issue).
   The write chokepoint is landed as a **`RefBackend`-trait method** (`ref_backend.rs:15`) so
   **both** backends are closed (r18, cid 3329052679, O10): the file backend keeps temp→rename +
   reconciliation, and `PgRefBackend` (`pg_refs.rs:35`) implements `commit_and_publish` as one
   `pool.begin()…tx.commit()` inserting the ref-carrying record (via `PgOpLogBackend`'s shared
   pool, `pg_oplog.rs:40`,`:259`) + the ref/head rows — native ACID atomicity, no
   reconciliation/watermark — with its existing `update_refs` (`pg_refs.rs:324`) made the private
   raw publish behind it. The record crosses the `RefBackend` boundary via a `refs`-defined
   committer trait (the write-side dual of `RefReconciler`), so `refs` gains no `oplog` dep.
   Unit tests mirroring `refs_transactions.rs:341-377` (reverse-order
   rewind) + a panic-unwind test + a delayed-retry exact-once test (retry past the
   old window) + reconciliation tests on **all ten read methods** (a conformance
   test asserting each routes through the primitive) + a **write-chokepoint conformance
   test** asserting no publish bypasses the record-first primitive and a crash-replay
   test that a fork/collapse interrupted after phase-4 but before phase-5 materializes
   the correct published thread/HEAD from the (now ref-carrying) record — on all three
   reader shapes — daemonless CLI, freshly-opened handle, and a **long-held
   `Arc<Repository>`** that opened before the crash and reads after it (§2.4 proof; the
   cell cid 3328112197 exposed).
   Effort: **xhigh** (intricate state machine + locks + panic-safety + a
   format-stability-sensitive oplog change). No `AtomicMutation` call-site migrations
   yet — but the 46 ref-write sites are routed through the write chokepoint (publish
   path only, not full `execute` migration).
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
- [x] **(2) Commit-point + ordering** — §2.2: the oplog append is the **sole**
  commit; refs are a post-commit materialized view; correctness rests on
  **per-read reconciliation** — every ref read reconciles against the oplog at
  read time, hooked **inside one internal `reconciled_load` primitive** that is the
  sole path for **logical reads** (reads that serve a ref value to a caller) in
  `RefManager` to touch raw ref storage and through which all **ten** public read
  methods funnel — point (`read_head` `:114`, `get_thread` `:129`, `get_marker`
  `:185`, `get_undo_recovery` `:252`, `get_remote_thread` `:256`), list
  (`list_threads` `:178`, `list_markers` `:230`, `list_remotes` `:305`,
  `list_remote_threads` `:312`), and `resolve` `:327` (all `refs_manager.rs`). The
  raw loaders
  (`read_change_id_at`/`read_head_state`/`try_read_ref_summary_index`/`*_from_storage`/`PackedRefs::load`)
  are reachable from a logical read only from inside the primitive (one-line
  conformance check targeting logical-read call-sites, with an explicit allowlist for
  the `pack_refs` maintenance path — `refs_manager.rs:337-380`, run by `heddle gc`
  `gc.rs:104` — which touches four of those loaders directly to **compact storage**,
  serves no logical value, and is therefore a reasoned exemption, **cid 3328894984**;
  it operates on a committed, already-materialized snapshot under `lock_refs()` and
  must not drop an oplog-committed-but-unmaterialized ref). The closure is therefore
  **structural, not an enumeration**: r6's three-method hook
  (`read_head`/`get_thread`/`get_marker`) left the four `list_*`, both remote,
  undo-recovery, and `resolve` readers bypassing (cid 3328832780), whereas every
  present *reader* **and any future read method** now reconciles because raw storage
  has no other logical-read entry. An accessor-layer hook would be bypassed by the
  **~124** direct `repo.refs()` readers (e.g. `cli/src/cli/commands/status.rs:1795`,
  `cli/src/cli/commands/collapse.rs:99`, `client/src/grpc_remote/sync.rs:588`,
  `:1001`, `thread.rs:507`, `marker.rs:64`). The
  `refs` crate has no `oplog` dep (`crates/refs/Cargo.toml`); the seam clears that
  by **dependency inversion** — a `RefReconciler` trait defined in `refs`, injected
  from the `repo`/`oplog` layer (`crates/repo/Cargo.toml:22`,`:24`) at `Repository`
  construction and invoked inside the primitive — so reconciliation reaches every
  reader (and the daemon handler `transaction.rs:143-152`) with no call-site
  changes. **Reconciliation scope is a property of the ref class (cid 3328894983):**
  local refs — `HEAD`, undo-recovery, both beside the *per-worktree* HEAD pointer
  (`refs_storage.rs:69-93`; `Repository::open` builds
  `RefManager::new(&shared_galeed_dir).with_local_head(...)`, `repository.rs:659`) —
  reconcile within this `op_scope`; shared refs — thread, marker, remote-thread,
  under the *shared* ref root (`refs_storage.rs:50-67`) — reconcile **globally**
  across all lanes, since one file serves every worktree (applying the `op_scope`
  filter to a shared ref is a correctness bug: it would miss a co-tenant's
  committed-but-unpublished shared write; r5's filter is scoped **down** to the local
  classes). The O(1) `head_id` generation gate (`packed_oplog.rs:26`,`:55`) runs
  inside the primitive as **two watermarks, split by ref class** — a per-`op_scope`
  cell (local) + a global cell (shared) — each a **watermark of *fully-materialized*
  batches**, advanced only after a lagged read materializes a crashed batch's *every*
  ref of that class (batch-atomic, cid 3328853451), never on a partial single-ref
  reconcile, so it cannot short-circuit a batch sibling for any read shape (point,
  list, remote); a batch advances whichever watermark(s) match the classes it
  touched. The **local**-ref scan reuses the `Some(&op_scope())` exact-match filter
  undo/redo apply (`undo.rs:108-109`, `:131-132`; `Repository::op_scope()`
  `repository.rs:1636`), so each lane resolves its own HEAD/recovery; the
  **shared**-ref scan is unfiltered so it folds every lane's committed thread/marker/
  remote update. This holds for **every reader path, handle age, crash timing, and
  oplog topology** — including the daemon's long-held `Arc<Repository>`
  (`local_daemon.rs:330`) that never re-passes `Repository::open`
  (`repository.rs:594`), the cell cid 3328112197 exposed and an open-time pass
  cannot reach, and shared-oplog worktrees (cid 3328776063 — local reads stay
  per-lane, shared reads span lanes). "Recover at open" is demoted to an **eager
  optimization**. Commit dedup is an **unbounded indexed `transaction_id` lookup**,
  *not* the window-bounded `record_batch_scoped_if_no_transaction`
  (`oplog_core.rs:281`, the rebase caller's 64-batch window,
  `rebase_ops.rs:192-202`). §2.4 collapses the per-cell matrix into a **single
  universal proof** — write chokepoint (every publish preceded by a ref-carrying
  record) + all reads reconcile at their class's scope (read side) + unbounded index
  (exact-once write side) ⇒ `committed ⇔ a ref-carrying oplog record exists` (recovery
  domain set by ref class: per-`op_scope` for local, global for shared) across the
  whole {writer × path × handle age × timing × topology} space — against lock-free
  readers (`refs_head.rs:22-41`, `refs_manager.rs:129-135`) + temp→rename apply
  (`refs_transactions.rs:230`). §2.3 idempotency.
- [x] **(2a) Every ref class has committed records (r9, cid 3328869364)** — §2.2
  "Remote-thread and undo-recovery writes are oplog-committed too": the last
  direct-write setters (`set_remote_thread` `:261`, `delete_remote_thread` `:284`,
  `set_undo_recovery` `:242`) gain committed `OpRecord` variants
  (`RemoteThreadUpdate`/`RemoteThreadDelete`/`UndoRecoveryUpdate`, appended at the
  `OpRecord` tail after `GitCheckpoint`, `oplog_types.rs:16`,`:222-228`) and route
  through the oplog-as-sole-commit path, so the all-ten reconciliation proof (§2.4)
  is **non-vacuous** for the remote/undo classes too — no reader reconciles against
  an empty tail, and replay materializes these refs from their records like any
  thread/marker write. Additive oplog-format change, pre-1.0 no migration shim,
  format-stability-sensitive (§6 O8).
- [x] **(2b) The write side is a structural chokepoint too (r11, cid 3328926767; the
  op-descriptor refinement + fork arg-order fix are r15, cid 3328979498 / 3328979497; the
  record-batch refinement is r16, cid 3329003333; the full-batch-record-coverage invariant
  + the single-sole-writer structural closure are r17, cid 3329019021 / 3329019023; the
  per-backend `RefBackend`-trait seam is r18, cid 3329052679)** —
  §2.2 "The write chokepoint": r9 closed the writers that recorded *nothing*; r11
  closes the writers that recorded the *wrong order* or a *ref-blind* shape. `cmd_fork`
  (`fork.rs:74-92`) and `cmd_collapse` (`collapse.rs:99-108`) published the thread/HEAD
  ref **before** appending the oplog record (`record_fork` `:94` / `record_collapse`
  `:112`); `cmd_fork` *also* passed `record_fork`'s args **reversed**
  (`record_fork(from, new_state)` `oplog_records.rs:113` vs the call
  `record_fork(&new_state.change_id, &source_state.change_id)` `:94-95`, persisting the
  source as the result, r15); and `OpRecord::Fork { from, new_state }`
  (`oplog_types.rs:38`) / `Collapse { sources, result }` (`:40`) did **not** name the
  ref they published — unreplayable for ref purposes. The fix is the write-side mirror
  of r7's read chokepoint: the canonical publish (temp→rename,
  `refs_transactions.rs:230`) is reachable **only** through one commit-then-publish
  primitive `commit_and_publish(op_records: &[OpRecord], ref_updates: &[RefUpdate])` that
  appends **all** of the **caller-supplied** record batch (phase 4) *before* publishing
  the atomic `ref_updates` batch (phase 5) **without splitting it**. It takes a **batch**,
  not a single record (r16), because some atomic ref batches back multiple records:
  `cmd_thread_rename` (`thread.rs:3061-3116`) publishes create-new + delete-old (± HEAD)
  in **one** atomic `update_refs` vector (`:3074-3100`) backed by a **two-record** batch
  (`ThreadCreate`+`ThreadDelete` via `record_thread_rename`, `oplog_records.rs:96-110`)
  — a single-`OpRecord` chokepoint would drop a backing record or split the atomic batch;
  the batch signature commits every record then publishes the whole ref batch together
  (single-record ops pass a one-element batch), so **every published ref in a batch has a
  backing record in the same atomic publish** — including the attached-HEAD thread-rename,
  whose third `RefUpdate::Head` (`thread.rs:3090-3099`) requires `record_thread_rename`
  (which today emits only `ThreadCreate`+`ThreadDelete`, `oplog_records.rs:96-110`) to
  add a third HEAD-move record, so the count of refs backed by a record equals the count
  of refs published (full-batch-record-coverage, r17, cid 3329019021).
  The primitive **takes** the caller's records rather than synthesizing them (r15): the
  generic write methods see only ref identity + target, not the `sources`/`from` the
  `Collapse`/`Fork` variants need, so semantic callers (fork/collapse) build their full
  record, thread-rename hands in its two-record batch, and plain ref edits pass a
  one-element generic ref-update record — all funneling through
  the one primitive. The **46** direct
  ref-write call sites (verified 2026-05-30 — the same enumeration r7 did for the ten
  readers; `fork.rs`, `collapse.rs`, `thread.rs`/`thread_cmd.rs`, `clone.rs`,
  `actor_cmd.rs`, `attempt.rs`, `remote/*`, `git_core.rs`/`git_sync.rs`,
  `grpc_remote/{hydration,sync,mod}.rs`, `repository_thread_materialize.rs`, …) inherit
  correct ordering + atomicity **by construction** — but the *proof* is not that count
  (r17, cid 3329019023): the raw publish (`update_refs_with_lock`'s temp→rename half,
  `refs_transactions.rs:228-256`) is made **private**, with `commit_and_publish` its
  **sole** caller, and every public writer family delegates through it — the plain edits,
  the CAS wrappers `set_thread_cas`/`set_marker_cas` (`thread.rs:1647-1650`,
  `grpc_remote/mod.rs:334-338`), `create_marker`/`delete_marker` (`grpc_remote/mod.rs:339`),
  the `delete_*_cas` siblings, and the r9 remote/undo setters — all already converging on
  `update_refs` (`refs_manager.rs:319`). So any future writer is covered too: there is no
  raw publish to call around the primitive (a one-line write-side conformance check, the
  analog of the read-side one, enforces the sole-caller boundary). This is the write-side
  mirror of r7's single-`reconciled_load` read closure — a single-sole-writer invariant,
  not a writer count. `cmd_fork` rebuilds
  `OpRecord::Fork { from: source_state, new_state, thread, head }` with the corrected
  arg order, and the published thread name + HEAD are carried by
  **extending the existing `Fork`/`Collapse` variants in place** — extra fields on the
  existing variant bodies, discriminant indices unchanged, modelled on existing variants
  like `ThreadUpdate { name, state }` — those published-ref fields being the
  authoritative replay target over the from/new_state positions. Pre-1.0 with no users
  and no production oplogs, this
  is a clean format break: the changed payloads mean old dev-only logs no longer
  deserialize those records, which is fine — no migration shim, no compat shim, no
  versioned `…V2` variant (that compatibility-preserving escape is the opposite of the
  no-backcompat stance; every other publishing variant already carries its ref). Because
  `op_records` are `oplog` types carrying data `refs` does not own, `commit_and_publish`
  lives at the `repo`/`oplog` layer with `refs` exposing only a private publish sub-step,
  so `refs` keeps no `oplog` dep (the write-side asymmetry with the `refs`-defined
  `RefReconciler`). After r11+r15+r16, "zero ref-write paths
  without a *preceding, ref-carrying* committed record" is **structural on both sides**,
  for single- and multi-record atomic batches alike.
  **r18 (cid 3329052679) closes the *second backend*:** `commit_and_publish` is lifted onto the
  `RefBackend` trait (`ref_backend.rs:15`), so the hosted `PgRefBackend` (`pg_refs.rs:35`) —
  whose `update_refs` (`pg_refs.rs:324`/`:328`) published refs via its own SQL tx, bypassing the
  file temp→rename seam — is closed too: it implements the seam as ONE `pool.begin()…tx.commit()`
  inserting the ref-carrying record + ref/head rows (native ACID, no reconciliation/watermark, via
  `PgOpLogBackend`'s shared pool, `pg_oplog.rs:40`,`:259`), with its raw SQL publish made private
  behind it. The single-sole-writer invariant is stated **per backend** (file: temp→rename + oplog
  + reconciliation; Postgres: one SQL tx), and the record crosses the trait boundary via a
  `refs`-defined committer trait (the write-side dual of `RefReconciler`) so `refs` keeps no
  `oplog` dep — revising r17's "the write primitive must sit one layer up," the very placement
  that left the Pg publish uncovered.
  Format-stability-sensitive (§6 O9, O10).
- [x] **(3) Nesting** — §3: enroll-into-outermost (savepoint) default, eager-
  commit exception **rule pinned** (§3.2), **type-level** compensator
  enforcement (§3.3: `DeferredMutation`/`EagerMutation` bound split on
  `enroll`/`enroll_eager` — a compile error, no `COMMIT_KIND` const or
  `debug_assert!`), `Tx` context + depth/reverse-order tracking (§3.1), op_scope
  tie-in + sentinel bridge (§3.4).
- [x] **(4) Panic-safety** — §4: explicit `Result` primary, `Drop` abort-only
  backstop, op_scope interaction.
- [x] **(5) Retrofit inventory** — §5: undo, thread/hydrate (with #302 r4
  precision), capture, op-id reserve (eager exemplar), ref-write, and the
  fork/collapse writers routed through the r11 write chokepoint (§5.5–5.6); each
  sketched.
- [x] **Recommendation + follow-up impl epic** — §7.
- [x] Real primitives cited by `path:line` throughout; no production code
  changed (this doc only).
