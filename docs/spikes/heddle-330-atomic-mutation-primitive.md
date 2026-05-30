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
  the oplog at read time** (hooked **inside `RefManager::{read_head, get_thread,
  get_marker}`** — `refs_manager.rs:114`/`:129`/`:185`, the sole read chokepoint
  below all **91** direct `repo.refs()` readers — reached via a `RefReconciler`
  trait defined in `refs` and injected from `repo`, so `refs` keeps no `oplog`
  dep; placing it at the `Repository` accessors instead would be bypassed by those
  91 readers). This holds for **every reader path, every handle age,
  every crash timing** — crucially the daemon's **long-held `Arc<Repository>`**
  (`local_daemon.rs:330`) that **never re-passes `Repository::open`**
  (`repository.rs:594`), the case an open-time pass structurally cannot reach
  (cid 3328112197). "Recover at open" is kept only as an **eager optimization**,
  not the guarantee; the hot path stays cheap via an O(1) oplog-generation
  (`head_id`, `packed_oplog.rs:26`,`:55`) check, full reconcile only on the rare
  lag. **And the commit is deduplicated by an *unbounded, indexed*
  `transaction_id` lookup, not the window-bounded
  `record_batch_scoped_if_no_transaction` (which only scans a caller-supplied
  window — the rebase caller passes `64` and documents that aging past it
  duplicates the batch, `rebase_ops.rs:192-202`)** — so a crash-retry at *any*
  later time is exactly-once. Per-read reconciliation (read side) + the unbounded
  index (write side) make "committed" ⇔ "oplog entry exists" hold universally —
  across reader path, handle age, and retry timing. See §2.2 + the §2.4
  crash/retry-coverage proof — the single most load-bearing correction in the
  spike.
- **Nesting = enroll-into-outermost (savepoint) by default; eager-commit only
  when an effect must be visible to another process before the outer commit**
  (the #251 reserve). This is a **type-level split**, not a runtime const:
  savepoint ops implement `SavepointMutation` (opt-in, no blanket impl), eager
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
// vs. eager is a *type-level* split (`SavepointMutation` vs `EagerMutation`,
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

> **Universal rule: a ref read reconciles against the oplog at read time, within
> the current worktree's `op_scope`.** A reader NEVER treats a canonical ref as
> authoritative-committed unless its committing oplog entry exists **in this
> repository's `op_scope`**; and if the committed oplog tail — *filtered to this
> `op_scope`* — names a newer target for that ref than the canonical value (a
> publication not yet materialized), the read resolves the **authoritative value
> from the oplog** (and MAY re-publish the canonical ref lazily). So the read
> never trusts a *lagging* cache (oplog ahead of ref) and never trusts a
> *committed-looking* ref with no backing entry (ref ahead of oplog —
> structurally impossible pre-commit anyway).
>
> The `op_scope` filter is load-bearing for **shared-oplog setups** (multiple
> worktrees sharing one oplog backend via `.heddle/objectstore`). The local HEAD
> pointer — and thus the canonical refs a read resolves — is *per worktree*, so
> the reconciliation must resolve each lane against its own committed entries.
> Without the filter, a long-lived checkout B reading next could reconcile its
> local HEAD/thread refs to a *different* checkout A's newest committed target,
> lazily publishing A's state into B's lane. The scope is exactly the worktree
> discriminator `Repository::op_scope()` already exists to provide
> (`repository.rs:1636-1654`: "unique per worktree even when several worktrees
> share one oplog backend … `undo`/`redo`/`--list` filter by exact-match
> scope"), reused unchanged — undo/redo already scope every oplog scan this way
> (`undo.rs:108-109`, `:131-132`: `recent_batches_scoped`/`undo_batches_scoped`
> with `Some(&scope)`; redo at `repository.rs:941-942`). Per-read reconciliation
> reuses that same `Some(&op_scope())` filter on its tail scan; it does not
> invent a new scoping mechanism.

This single rule holds the invariant across **all four axes at once** — reader
path (daemon RPC vs direct CLI), **handle age** (freshly opened vs a long-held
`Arc<Repository>`), crash timing (immediate vs delayed), and **oplog topology**
(a private oplog vs a shared backend fronting multiple worktrees) — precisely
because reconciliation happens *per read*, not *per open*, and *within the
current `op_scope`*: it re-reads the current oplog state from disk on every
resolve, filtered to this worktree's lane, so a handle opened once at process
start still reconciles on its ten-thousandth read and never crosses into another
lane's state. "Recover at open" structurally cannot do
this; per-read reconciliation is what makes the daemon-handle cell hold, and it
subsumes the daemon-vs-CLI and immediate-vs-delayed cells the prior rounds
enumerated one at a time.

**Where the rule hooks (grounded) — the single read chokepoint, `RefManager`.**
The seam must be the one place EVERY ref read funnels through, or a reader that
reaches around it observes a committed-looking canonical ref without the
`op_scope`-filtered oplog reconciliation, and the invariant has a hole. That
chokepoint is `RefManager`'s three lock-free read methods — `read_head`
(`refs_manager.rs:114`) → `read_head_state` (`refs_head.rs:22-41`), `get_thread`
(`refs_manager.rs:129`), and `get_marker` (`refs_manager.rs:185`). **The
reconciliation lives *inside* these methods**, below the `Repository` accessor
layer — not at the accessors.

*Why not the `Repository` read accessors — the bypass that placement leaves open.*
An accessor-layer hook (`repo.head()` `repository.rs:1737` plus a reconciling
`repo`-level `get_thread`/`get_marker` wrapper) is **bypassable, and the readers
that matter bypass it.** **91 production call-sites read
`repo.refs().{read_head,get_thread,get_marker}` directly on `RefManager`**, going
straight around any `Repository` accessor
(`rg "refs\(\)\.(get_thread|get_marker|read_head)"` → 91 non-test hits across 38
files, verified 2026-05-30). Three sit on critical resolve paths:
`repo.refs().get_thread(..)` in `heddle status`
(`cli/src/cli/commands/status.rs:1795`), `repo.refs().read_head()` in `collapse`
(`cli/src/cli/commands/collapse.rs:99`), and `repo.refs().get_thread(..)` in
hosted sync (`client/src/grpc_hosted/sync.rs:588`). A shared-oplog or post-crash
lagging-ref reader on ANY of these 91 would trust the canonical ref **without**
reconciliation — so accessor-layer placement leaves the "universal" invariant with
91 holes. Routing each reader onto a reconciling accessor only "fixes" the class
one call-site at a time (the exact drip §2.4 indicts), and a 92nd raw
`repo.refs()` read added later silently re-opens it. Enforcement at a layer
callers can skip is not enforcement.

*The crate-dependency objection, and how the chokepoint clears it.* The reason an
earlier framing reached one layer up is that reconciliation must consult the
committed oplog tail, yet `RefManager` lives in the `refs` crate, which **does not
depend on `oplog`** (`crates/refs/Cargo.toml` declares no oplog dep — verified
2026-05-30), so the seam appears to need the only layer that sees both crates
(`crates/repo/Cargo.toml:22` oplog, `:24` refs). The resolution is **dependency
inversion, not relocating the seam**: define a narrow `RefReconciler` trait *in
the `refs` crate*, over types `refs` already owns (`Head`, `ChangeId`, the ref
name, and the cached `head_id: u64` generation); have `RefManager` hold an
`Option<Arc<dyn RefReconciler>>` set by a `with_reconciler(..)` builder — the same
shape as the existing `with_local_head` builder (`refs_manager.rs:50`) — and
inject the concrete oplog-backed implementation from the `repo`/`oplog` layer at
`Repository` construction (the layer that sees both crates). The trait *is* the
seam; the `refs` crate gains **no** oplog dependency — it depends only on an
abstraction it defines, which the higher layer implements. `RefManager::{read_head,
get_thread, get_marker}` invoke `self.reconciler` (when present) before returning;
the reconciler runs the O(1) `head_id` generation gate (the cheap path — no tail
scan when the generation is unchanged; see "Keeping it cheap" below) and, only on
a generation advance, the `Some(&op_scope())`-filtered tail scan — so **both the
cheap generation gate (r4) and the `op_scope` scoping (r5) live inside the
`RefManager` read methods**. A bootstrap `RefManager::new` with no reconciler
keeps today's plain-cache behavior. Because the hook sits below the accessor
layer, **ALL 91 direct `repo.refs()` readers — plus every `Repository` accessor,
plus the daemon handler at `transaction.rs:143-152` — inherit reconciliation with
zero call-site changes**: there is no lower-level raw ref read a caller can reach
around `RefManager`, so no bypass exists.

Reconciliation re-derives the committed target with no extra bookkeeping: every
committed state `OpRecord` carries the ref identity + target — `Snapshot {
new_state, thread }` (`oplog_types.rs:18-22`), `ThreadCreate/ThreadUpdate { name,
… state }` (`:29`, `:33`), `Goto { target }` (`:24`) for HEAD — and the read
takes the newest committed target *within the current `op_scope`* in the tail
(newest-wins, so two committed txns on one ref resolve to the same value a
non-crashed run would produce). "Newest in the tail" always means newest among
*this worktree's* entries — the scan is the `Some(&op_scope())`-filtered one
undo/redo already run (`undo.rs:108-109`, `:131-132`), so in a shared-oplog
setup a read in checkout B resolves B's lane only and never lifts checkout A's
newest committed target.

**Keeping it cheap (the cost the decision accepts).** Reconciling on *every* read
must not become a full oplog scan per read. The hot-path check is a **generation
/ commit-index** comparison against the oplog's monotonic head id. The packed
oplog already carries `head_id: u64` (`packed_oplog.rs:26`) and writes it as the
**leading field** of the packed file (`packed_oplog.rs:55`), so the current value
is readable from the file header without parsing the log; every append advances it
(`packed_oplog.rs:206-209`; `start_id = packed.head_id + 1`, `oplog_core.rs:249`,
`:308`). A reader caches the `head_id` it last reconciled against, and on each
read reads the current `head_id` (an O(1) header read) and:

- if it is **unchanged**, no commit has landed since the last reconcile ⇒ the
  canonical ref is current ⇒ return it directly — **no tail scan, no write**;
- only if it has **advanced** does the reader scan the tail from `cached+1` for
  committed `TransactionCommit` entries **in this `op_scope`** naming this ref,
  resolve the newest target, and reconcile (lazily re-publishing the lagging
  ref). The scan applies the same `Some(&op_scope())` exact-match filter
  undo/redo use (`undo.rs:108-109`, `:131-132`), so a `head_id` advance driven
  purely by *another* worktree's commit in a shared oplog finds no entry for
  this lane and the reconcile is a no-op — the reader keeps returning its own
  canonical ref.

So the steady-state hot path is one small header read plus an integer compare;
the full reconcile runs only on the rare post-crash lag. (A per-ref committed
index tightens this further — reconcile only when *this ref's* newest committed
target advanced — but the single `head_id` generation gate is the simple floor.
Exposing a cheap `OpLog::head_id()`/`tip()` header accessor is net-new impl work,
§6 O7.)

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
re-open the class one verb at a time. `RefManager`'s read methods are the single
structural choke point every resolve already funnels through — including all 91
direct `repo.refs()` readers — so enforcing reconciliation there (not at the
bypassable accessor layer) is what closes the class.

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

The invariant — stated precisely, **`committed ⇔ oplog entry exists` *within each
`op_scope`*** (each worktree/lane is its own commit/recovery domain) — must hold
for *every* reader and *every* retry timing. The prior rounds tried to prove this **cell by cell** —
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

**The collapse.** Two orthogonal mechanisms — one on the *read* side, one on the
*write* side — make the entire product space hold, with no per-cell case analysis:

- **Read side — per-read reconciliation (§2.2 "Reader model").** Every ref read
  reconciles against the committed oplog tail at read time, hooked **inside
  `RefManager::{read_head, get_thread, get_marker}`** (`refs_manager.rs:114`/`:129`/`:185`)
  — the sole read chokepoint all 91 direct `repo.refs()` readers (and every
  `Repository` accessor, and the daemon handler `transaction.rs:143-152`) funnel
  through, reached via a `refs`-crate `RefReconciler` trait the `repo`/`oplog`
  layer injects (dependency inversion — no `refs`→`oplog` crate dep). The
  reconciliation scans **only this repository's `op_scope`** — the same `Some(&op_scope())` exact-match
  filter undo/redo apply to every oplog scan (`undo.rs:108-109`, `:131-132`;
  `Repository::op_scope()` `repository.rs:1636`), so each worktree/lane resolves
  against its own committed entries. Therefore a reader on
  **any path** (daemon RPC or direct CLI), holding a handle of **any age**
  (freshly opened or a long-held `Arc<Repository>`), observing a crash at **any
  time** (immediate or delayed), under **any oplog topology** (a private oplog or
  a shared backend fronting multiple worktrees):
    1. never treats a canonical ref as committed without its backing oplog entry
       *in its own `op_scope`* (the read confirms the entry), and
    2. never returns a stale canonical ref this lane's oplog has already
       superseded, and **never lifts another lane's target** (the read resolves
       the committed target within its own `op_scope`).
  Because the check is *in the read*, not *at the open*, and *scoped to the
  reader's own lane*, there is no reader, handle, timing, or co-tenant worktree
  that escapes it. The matrix collapses to **"all reads reconcile, in scope, ∎"**
  — there are no cells left to enumerate, because the third axis (handle age) and
  fourth axis (shared-oplog topology) the per-cell frame missed are closed by the
  same mechanism as the first two.

- **Write side — unbounded indexed exact-once commit (§2.2 "Idempotency of the
  commit," retained from r3).** The phase-4 linearization point deduplicates on an
  **unbounded, indexed `transaction_id` → committed-batch-id** lookup under the
  oplog write lock — *not* the window-bounded `record_batch_scoped_if_no_transaction`
  (`oplog_core.rs:281`). A crashed `execute` re-run at any timing — immediate, or
  delayed past *any* fixed window `N` — finds the prior `TransactionCommit` and
  refuses the second append. Window size stops being a correctness parameter.

These two are **independent**: reconciliation governs *reading* a commit, the
indexed dedup governs *writing* one. Together — every read reconciles, every
commit appends at most once — they hold `committed ⇔ oplog entry exists`
universally.

The one forbidden state — a NEW, committed-*looking* canonical ref with **no**
backing oplog entry — is structurally impossible regardless of reader/handle/timing,
because nothing publishes a canonical ref before its phase-4 oplog entry is durable;
and the only post-crash residue (a *lagging* OLD ref with the entry already present)
is resolved at the read by reconciliation, on every path and handle age, not merely
at the next `open` — and, because the reconcile is `op_scope`-filtered, a reader in
a shared-oplog setup resolves only its own lane and never observes another
worktree's uncommitted-to-its-own-refs state. That is the close-the-class result:
not a covered matrix, but a single invariant — `committed ⇔ oplog entry exists`,
*per `op_scope`* — enforced in the shared read path. The impl epic (§6 O1, O7) carries
the two mechanisms — the per-read reconciliation hook and the unbounded index — as
the concrete deliverables; the `open`-time materialization survives only as the
eager fast-path that prefetches what the read would otherwise do lazily (§2.2),
never as the guarantee.

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
> outer transaction commits.** Everything else is a `SavepointMutation`.

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
leaked reservation unrepresentable (§3.3). If the outer
transaction later fails, `rewind_all` runs that compensator — for the reserve,
the compensator is `store.cancel(op_id, verb)` (`operation_id.rs:152`,
`operation_dedup.rs:288`), which releases the reservation so a retry isn't
wedged on a stale `InFlight`. The compensator is saga semantics for that one
leg: the effect was really visible for a while, and the compensator makes the
*net* outcome correct. Eager legs commit in apply-order; their compensators run
in reverse with everything else.

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
/// Opt-in marker for a savepoint-enrollable op: its staged effects are
/// invisible to other readers until the outer commit publishes them, so it
/// may defer to the outermost commit (§3.1). There is deliberately NO
/// blanket `impl<M: AtomicMutation> SavepointMutation for M` — an op opts in
/// by implementing this explicitly, so an op that is ONLY `EagerMutation`
/// does NOT satisfy the `enroll` bound and cannot be enrolled as a savepoint.
pub trait SavepointMutation: AtomicMutation {}

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
    /// Savepoint enroll — bounded to `SavepointMutation`. Runs only `apply`
    /// (staged, reversible). An `EagerMutation`-only op fails this bound.
    pub fn enroll<M: SavepointMutation>(&mut self, m: M) -> Result<StagedCommit<M::Output>> { … }

    /// Eager enroll — bounded to `EagerMutation`. Stages via `apply`, then runs
    /// `commit_eager` and registers the returned `Compensator` into the ledger
    /// atomically. The compensator is guaranteed to exist because the bound
    /// requires the method that produces it.
    pub fn enroll_eager<M: EagerMutation>(&mut self, m: M) -> Result<M::Output> { … }
}
```

Why this closes the hole the `COMMIT_KIND` sketch left open:

- **`enroll` is bounded to `SavepointMutation`.** Passing an op that implements
  only `EagerMutation` is a hard **compile error** (`the trait bound
  ReserveOpId: SavepointMutation is not satisfied`) — not a release-eliding
  assert. There is no blanket `SavepointMutation` impl, so eager ops do not
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
> system cannot *forbid* an op from implementing both `SavepointMutation` and
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
        // savepoint sub-op: stage the recovery ref (temp file; published
        // post-commit in phase 5, rewind = unlink the temp)
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
reconciliation** (§2.2 "Reader model"): the next read of that ref — through
`RefManager`'s reconciling read methods (the chokepoint below all 91 direct
`repo.refs()` readers), on *any* path and *any* handle age, including
the daemon's long-held `Arc<Repository>` (`local_daemon.rs:330`,
`transaction.rs:143-152`) that never re-opens — sees the oplog generation has
advanced, folds the committed `OpRecord::Snapshot { new_state, thread }`
(`oplog_types.rs:18-22`) from the tail, and resolves the committed target (lazily
re-publishing the ref). The `Repository::open` (`repository.rs:594`) eager pass is
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
`operation_dedup.rs:216`) returns `Reserved` / `Replay` / `InFlight` / `Conflict`
(`operation_dedup.rs:104`); on `Reserved` it spawns the child, then `store.record`
(`:162`) or `store.cancel` on spawn failure (`:152`).

**As an `EagerMutation` sub-op:** when an op-id-bearing command is itself wrapped
in a transaction, the reserve is the canonical eager leg — enrolled via
`enroll_eager` (bounded `M: EagerMutation`, §3.3), never `enroll`:
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
Eager: **yes** — this is the whole reason `EagerMutation` + the `enroll_eager`
path exist.

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
| capture | `repository_snapshot.rs:52` | no | no | 3 | ref-moved-but-not-recorded window; ref publish becomes a post-commit materialized view |
| op-id reserve | `operation_id.rs:115` | as sub-op | **yes** | 4 | eager-commit exemplar; stale `InFlight` on rollback |
| ref writes | `refs_manager.rs:319` | n/a | no | — | already in-domain atomic; becomes the "stage refs" leg |

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
- **O6 — Sealing `SavepointMutation` ⊻ `EagerMutation` (belt-and-suspenders).**
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
  - **Per-read reconciliation hook (the guarantee) + open-time eager pass
    (optimization).** Correctness lives in the read: every ref resolve reconciles
    against the committed oplog tail (§2.2 "Reader model"). The hook lands
    **inside `RefManager::{read_head, get_thread, get_marker}`**
    (`refs_manager.rs:114`/`:129`/`:185`) — the sole chokepoint **below** the
    `Repository` accessor layer that all 91 direct `repo.refs()` readers funnel
    through — **not** at the `Repository` accessors, because an accessor-layer hook
    is bypassed by those 91 readers (`cli/src/cli/commands/status.rs:1795`,
    `cli/src/cli/commands/collapse.rs:99`, `client/src/grpc_hosted/sync.rs:588`,
    …). `RefManager` lives in the `refs` crate, which does not depend on `oplog`
    (`crates/refs/Cargo.toml`); the seam clears that via **dependency inversion** —
    a `RefReconciler` trait defined *in* `refs` (over `Head`/`ChangeId`/ref-name/
    `head_id`), held as `Option<Arc<dyn RefReconciler>>` via a `with_reconciler(..)`
    builder (cf. `with_local_head` `refs_manager.rs:50`), with the concrete
    oplog-backed impl injected from the `repo`/`oplog` layer at `Repository`
    construction (`crates/repo/Cargo.toml:22` oplog, `:24` refs). Impl work:
    implement the trait + injection + the in-method reconcile call —
    **no per-call-site routing**: precisely *because* the chokepoint is below all
    91 readers, every reader (and every `Repository` accessor, and the daemon
    handler `transaction.rs:143-152`) inherits reconciliation unchanged. **Cost:** the hot path must be near-free or it taxes every read
    (daemon RPC and `heddle log`/`status` alike) — hence the O(1) generation check
    on the oplog `head_id` (`packed_oplog.rs:26`, the file's leading field `:55`),
    so a read that finds `head_id` unchanged returns immediately with no tail scan
    and no write; full reconcile (and lazy re-publish) only on the rare advanced-
    generation lag. This needs a cheap `OpLog::head_id()`/`tip()` header accessor
    (net-new). The `Repository::open` (`repository.rs:594`) eager materialization
    is kept as an *optional* prefetch — it repairs lag once at open so subsequent
    reads on that handle skip even the reconcile — but is **not** load-bearing and
    may be dropped; it must not itself need the recovery it provides (bootstrap
    ordering inside `open`). A per-ref committed index (vs the single `head_id`
    gate) is an optional refinement to avoid reconciling a read when a *different*
    ref advanced.
  - **Unbounded indexed `transaction_id` map.** The exact-once commit needs a
    `transaction_id → committed-batch-id` index maintained under the oplog write
    lock and persisted atomically with the log (so it can never disagree). This
    replaces the window-bounded `record_batch_scoped_if_no_transaction`
    (`oplog_core.rs:281`) *as the linearization point* — that helper stays for the
    bounded rebase path. Open sub-questions: index persistence format (sidecar vs.
    derived-on-load from a full scan), and whether to GC the index for very long
    histories (it grows with distinct transaction ids). The full-tail-scan
    fallback is the zero-new-state correctness floor if a sidecar is undesirable.

---

## §7 — Recommendation + follow-up impl epic

**Recommendation: build the primitive, `dyn`-free, and migrate in the priority
order above.** The bug class is real, recurring, and structurally closable; the
executor-enforces-once shape fits heddle's existing type-state idioms; and most
primitives it composes (CAS batch + reverse rollback, a *window-bounded*
idempotent oplog append, orphan-tolerant object store, atomic FS rename) already
exist — the work is sequencing them under one ledger, not inventing durability.
The two genuinely net-new pieces are the close-the-class mechanisms (O7):
**per-read reconciliation hooked _inside_ `RefManager::{read_head, get_thread,
get_marker}`** — the sole read chokepoint below all 91 direct `repo.refs()`
readers, reached via a `refs`-crate `RefReconciler` trait injected from `repo`
(dependency inversion, so `refs` keeps no `oplog` dep), and **filtered to the
current `op_scope`** (so the `committed ⇔ oplog entry exists` invariant holds
per-lane for every reader path, *handle age*, crash timing, and oplog topology —
including the daemon's long-held `Arc<Repository>` an open-time-only pass cannot
reach, and shared-oplog worktrees that must not cross lanes) and an **unbounded indexed `transaction_id`
commit dedup** (so exact-once holds at any retry timing, not just within a 64-batch
window). The `Repository::open` eager materialization is kept only as an
optimization on top of the read-side guarantee.

### Proposed impl epic shape (blocked by this spike — confirm before filing)

> **Epic: atomic-mutation primitive — land `AtomicMutation` + migrate the
> recurring multi-step mutations.** Blocked by #330.

1. **#330-impl-a — land the primitive (no migrations).** `AtomicMutation`,
   `EagerMutation`, `Tx`, `execute`, the rewind ledger, `Drop` backstop, the
   **unbounded indexed `transaction_id` commit dedup** (O7 — *not* a reuse of the
   window-bounded `record_batch_scoped_if_no_transaction`), and **per-read
   reconciliation implemented *inside* `RefManager::{read_head, get_thread,
   get_marker}`** via a `refs`-crate `RefReconciler` trait injected from `repo`
   (O7 — the read-side guarantee; the chokepoint below all 91 `repo.refs()`
   readers, so **no per-call-site edits**; the `Repository::open` eager pass is an
   optional optimization on top). Unit tests mirroring `refs_transactions.rs:341-377` (reverse-order
   rewind) + a panic-unwind test + a delayed-retry exact-once test (retry past the
   old window) + reconciliation tests on all three reader shapes — daemonless CLI,
   freshly-opened handle, and a **long-held `Arc<Repository>`** that opened before
   the crash and reads after it (§2.4 proof; the cell cid 3328112197 exposed).
   Effort: **xhigh** (intricate state machine + locks + panic-safety). No call site
   changes yet.
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
  read time *within the current `op_scope`*, hooked **inside `RefManager::{read_head,
  get_thread, get_marker}`** (`refs_manager.rs:114`/`:129`/`:185`) — the sole read
  chokepoint **below** the `Repository` accessor layer, through which all **91**
  direct `repo.refs()` readers funnel (an accessor-layer hook would be bypassed by
  those 91 — e.g. `cli/src/cli/commands/status.rs:1795`,
  `cli/src/cli/commands/collapse.rs:99`, `client/src/grpc_hosted/sync.rs:588`). The
  `refs` crate has no `oplog` dep (`crates/refs/Cargo.toml`); the seam clears that
  by **dependency inversion** — a `RefReconciler` trait defined in `refs`, injected
  from the `repo`/`oplog` layer (`crates/repo/Cargo.toml:22`,`:24`) at `Repository`
  construction — so reconciliation reaches every reader (and the daemon handler
  `transaction.rs:143-152`) with no call-site changes. Both the O(1) `head_id`
  generation gate (`packed_oplog.rs:26`,`:55`) and the `op_scope` filter run inside
  those `RefManager` methods. The tail scan reuses the
  `Some(&op_scope())` exact-match filter undo/redo already apply (`undo.rs:108-109`,
  `:131-132`; `Repository::op_scope()` `repository.rs:1636`), so each worktree/lane
  resolves only its own committed entries. This holds for **every reader path,
  handle age, crash timing, and oplog topology** — including the daemon's long-held
  `Arc<Repository>` (`local_daemon.rs:330`) that never re-passes `Repository::open`
  (`repository.rs:594`), the cell cid 3328112197 exposed and an open-time pass
  cannot reach, and shared-oplog worktrees (cid 3328776063) that must resolve
  their own lane, never a co-tenant's target. "Recover at open" is demoted to an **eager optimization**. Commit
  dedup is an **unbounded indexed `transaction_id` lookup**, *not* the
  window-bounded `record_batch_scoped_if_no_transaction` (`oplog_core.rs:281`, the
  rebase caller's 64-batch window, `rebase_ops.rs:192-202`). §2.4 collapses the
  per-cell matrix into a **single universal proof** — all reads reconcile in scope
  (read side) + unbounded index (write side) ⇒ `committed ⇔ oplog entry exists`
  *per `op_scope`* across the whole {path × handle age × timing × topology} space —
  against lock-free readers
  (`refs_head.rs:22-41`, `refs_manager.rs:129-135`) + temp→rename apply
  (`refs_transactions.rs:230`). §2.3 idempotency.
- [x] **(3) Nesting** — §3: enroll-into-outermost (savepoint) default, eager-
  commit exception **rule pinned** (§3.2), **type-level** compensator
  enforcement (§3.3: `SavepointMutation`/`EagerMutation` bound split on
  `enroll`/`enroll_eager` — a compile error, no `COMMIT_KIND` const or
  `debug_assert!`), `Tx` context + depth/reverse-order tracking (§3.1), op_scope
  tie-in + sentinel bridge (§3.4).
- [x] **(4) Panic-safety** — §4: explicit `Result` primary, `Drop` abort-only
  backstop, op_scope interaction.
- [x] **(5) Retrofit inventory** — §5: undo, thread/hydrate (with #302 r4
  precision), capture, op-id reserve (eager exemplar), ref-write; each sketched.
- [x] **Recommendation + follow-up impl epic** — §7.
- [x] Real primitives cited by `path:line` throughout; no production code
  changed (this doc only).
