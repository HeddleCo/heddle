# Pending API contracts — decision doc (heddle#206)

**Status:** Spike. Decision locked (Strategy C, full type-state via the witness-type
idiom); impl tracked separately (see §6).
**Scope:** `crates/mount/src/pending.rs` (encapsulation landed by PR #182 r10) plus
every callsite in `crates/mount/src/core.rs` that drives the lifecycle FSM. The
storage model and FSM itself are owned by [`docs/design/mount-posix-semantics.md`](./mount-posix-semantics.md)
and not re-derived here.
**Inputs grounded against:** PR #182 head (`task/180-mount-fuse-implement-write-side-ops-crea`)
— `crates/mount/src/pending.rs` (766 lines) and `crates/mount/src/core.rs`
(~4,020 lines, 44 `pending.<method>(` callsites enumerated in §4).

---

## 1. Premise + locked decision

### 1.1 Review history

PR #182 (heddle#180 — FUSE write-side ops) has been through 11 Codex rounds. The
shape of those rounds has shifted, and the shift is the whole reason this spike
exists:

* **r6 → r9 (six findings, one bug class).** Code paths in `core.rs` mutated the
  pending overlay's maps directly (`pending.state.clear()`, `pending.warm.remove(id)`,
  `pending.hot.insert(...)`) and bypassed the FSM. r10's structural fix made every
  field of `Pending` module-private and routed mutation through state-aware
  methods (`transition_to_orphan`, `release_open_handle`, `kernel_forget_inode`,
  `drain_for_capture`, …). The `scripts/check-no-direct-pending-mutation.sh`
  static check belts-and-braces the discipline. **Direct-field-mutation bugs are
  unrepresentable.** 17 fixture tests verify the encapsulation.

* **r11 (four new findings, one new bug class).** With direct mutation gone,
  Codex found the next layer: the *methods themselves* had off-by-a-condition
  bugs on lifecycle preconditions. The methods accept any NodeId in any state
  and silently do the wrong thing for the wrong state. Four findings:

  | #  | Severity | Method                              | Bug                                                                                                          |
  | -- | -------- | ----------------------------------- | ------------------------------------------------------------------------------------------------------------ |
  | 1  | P2       | `Pending::transition_to_orphan`     | Records `Orphan { open_count: 0 }` for nodes with no live fds → dead lifecycle records that never get reaped |
  | 2  | P1       | `Pending::drain_for_capture`        | Drops `Live` entries with `open_count > 0` → breaks POSIX "last-close-wins" across a capture                 |
  | 3  | P1       | `Pending::kernel_forget_inode`      | Removes `hot[id]` before checking lifecycle state → data loss if forget races open handles                   |
  | 4  | P2       | `rename_entry_with_options` caller  | Calls `transition_to_orphan` for symlinks/dirs (no open/release lifecycle) → stale state accumulates         |

  #1–#3 are bugs **in** the API. #4 is a bug in a **caller** that picked the
  wrong method for a state the method didn't reject.

The pattern is the same as r6–r9 one level up: **an abstraction's contract is
implicit, callers and the impl can miss it, Codex catches it, we fix, repeat.**
The pattern stops when the contract is enforced by the type system rather than
documented in a doc-comment.

### 1.2 Locked decision

**Strategy C — full type-state for `Pending`.** Misuse cannot compile.

The user evaluated four strategies (A: `debug_assert!` preconditions on every
method; B: refinement types for the immediately-bug-shaped parameters; C: full
type-state for the FSM; D: TLA+/proptest model checker against the FSM) and
chose C. Rationale:

* A panics in debug, strips in release, and does not prevent the bug at design
  time. Loud, but not gone.
* B (e.g. `OpenCount: NonZero|Zero` newtype) catches #1 directly but not #2 or
  #3 — those are "method does the wrong thing internally on the wrong state",
  not "caller passes the wrong scalar".
* C makes every transition a typed method on a state-witnessed type. Every r11
  finding becomes "won't typecheck". §3 walks each finding through the type
  system to verify this is not just rhetoric.
* D (property tests against the §1 FSM in `mount-posix-semantics.md`) remains
  valuable as a *complement* — it catches FSM-divergence bugs the types can't
  see across the FUSE-callback boundary with the kernel. It does not replace
  static enforcement; it backs it up. See §4 and impl sub-issue 5 in §6.

The unresolved choice is the **Rust idiom** for type-state on a long-lived
storage struct (`Pending` is held by `MountInner` across the entire mount
lifetime, not constructed-and-consumed). Two idioms exist; we pick the
witness-type idiom and justify it in §2.

---

## 2. Type-state design

### 2.1 Lifecycle states

These are the four states a NodeId can be in. They are the same four states
the §1 FSM in `mount-posix-semantics.md` already names; the type-level
encoding makes them inhabit different types.

```rust
/// The Pending FSM's per-NodeId lifecycle. Sealed; only the four
/// types below implement it.
pub(crate) trait Lifecycle: sealed::Sealed {}

pub(crate) struct LiveNonZero;  // Live { open_count >= 1 }
pub(crate) struct LiveZero;     // Live { open_count == 0 }
pub(crate) struct Orphan;       // Orphan { open_count >= 0 } — bytes outlive directory entry
pub(crate) struct Released;     // not in `state` map; entry retired

impl Lifecycle for LiveNonZero {}
impl Lifecycle for LiveZero {}
impl Lifecycle for Orphan {}
impl Lifecycle for Released {}
```

Why split `Live` into `LiveNonZero` / `LiveZero` (the §1 FSM treats `Live`
as one state with a refcount): because finding #1 is specifically about
the legality of transitioning to Orphan, and that's only valid from
`LiveNonZero`. Splitting the refcount=0 case off as its own state lets a
method signature say "Live with at least one open fd" at the type level.

Transitions (compile-checked):

```text
                       record_open
                     ┌─────────────┐
                     │             ▼
       record_open  ┌─┴─────┐  ┌───────────┐  release_open_handle (count → 0)
   None ──────────► │ LiveZ │  │ LiveNonZ  │ ─────────────────────────────────┐
                    └───┬───┘  └─────┬─────┘                                  │
                        │            │ transition_to_orphan                   │
                        │            ▼                                        │
                        │       ┌────────┐  release_open_handle (count → 0)   │
                        │       │ Orphan │ ─────────────────────────────► Released
                        │       └────────┘
                        │ release_live (count == 0)
                        └─────────────────────────────────────────────► Released
```

Methods are state-gated:

* `LiveNonZero::transition_to_orphan(self) -> Orphan` — fixes finding #1.
* `LiveZero::release_live(self) -> Released` — there is no method called
  `transition_to_orphan` on `LiveZero`; the bug doesn't compile.
* `Orphan::release_one(self) -> Either<Orphan, Released>` — open-count
  decrement; transitions to `Released` only when reaching zero.
* `LiveNonZero | LiveZero | Orphan::record_open(self) -> LiveNonZero`
  (the destination is always `LiveNonZero` after a successful open).
* `kernel_forget_inode` is **not** a method on any of these; it's a
  method on a separate `Hot` typed handle (see §2.3). Fixes finding #3.

### 2.2 The storage shape problem

The simplest type-state idiom is "transition-by-move":

```rust
let p: Pending<LiveNonZero> = …;
let p: Pending<Orphan> = p.transition_to_orphan();
```

This does not fit `Pending` as it exists. `MountInner` holds **one**
`Pending` over its entire lifetime, covering many NodeIds simultaneously
in different states. Parameterising `Pending` over a single
`Lifecycle` type is meaningless — at any instant the map holds a mix.

Two Rust idioms work around this. We pick the first; the second is
documented for completeness so future readers don't re-litigate.

#### 2.2.1 Witness-type idiom (chosen)

`Pending` itself stays non-parameterized. The storage shape is unchanged
from r10. Methods that depend on the per-NodeId state take a **witness**
parameter that can only be constructed by an FSM-aware lookup function:

```rust
/// Proof that the NodeId is currently in state `S`. Constructed only
/// via `Pending::witness::<S>(id)`. Owning one is the type-level
/// promise that the lookup returned `Some(state matching S)` at the
/// moment of construction. The witness is `!Send` and lifetime-bound
/// to a `&mut Pending` borrow so the FSM cannot change underneath it
/// (the borrow checker enforces this — no other code can mutate
/// `Pending` while the witness exists).
pub(crate) struct Witness<'p, S: Lifecycle> {
    id: u64,
    _state: PhantomData<S>,
    _borrow: PhantomData<&'p mut ()>,
}

impl Pending {
    /// Construct a witness if-and-only-if `id` is in state `S`. The
    /// returned witness is the *only* way to call the state-gated
    /// methods below.
    pub(crate) fn witness_live_nonzero(&mut self, id: u64)
        -> Option<Witness<'_, LiveNonZero>> { … }

    pub(crate) fn witness_live_zero(&mut self, id: u64)
        -> Option<Witness<'_, LiveZero>> { … }

    pub(crate) fn witness_orphan(&mut self, id: u64)
        -> Option<Witness<'_, Orphan>> { … }
}

impl Pending {
    /// Transition the NodeId held by `w` from LiveNonZero → Orphan.
    /// Consumes the witness so it cannot be reused after the FSM moved.
    pub(crate) fn transition_to_orphan(&mut self, w: Witness<'_, LiveNonZero>)
        -> Witness<'_, Orphan> { … }

    /// Final release of an Orphan with open_count == 0 is reachable
    /// only via `release_one`'s `Released` branch.
    pub(crate) fn release_one(&mut self, w: Witness<'_, Orphan>)
        -> Either<Witness<'_, Orphan>, ()> { … }
}
```

Properties:

1. **Storage shape unchanged.** `MountInner.pending: Pending` stays
   non-generic. No retrofit to `MountInner`'s definition, no GATs, no
   enum-dispatch refactor on the storage side.
2. **Calls are gated at the type level.** `Pending::transition_to_orphan`
   only accepts `Witness<LiveNonZero>`. There is no way to call it on an
   Orphan or on a Live-with-zero-fds node; the compiler refuses.
3. **Witnesses are scoped to a `&mut Pending` borrow.** The borrow
   checker prevents holding a stale witness across a mutation.
4. **No runtime cost vs. r10.** The witness is one `u64` + zero-sized
   `PhantomData`; the FSM check happens once at witness construction
   and is the same `match` the r10 methods already do internally.

#### 2.2.2 Enum-dispatch / GAT idiom (rejected)

Alternative: turn `Pending` into a parameterized type with a discriminating
GAT, or expose `&PendingLiveNonZero` etc. via `match`. Caller does:

```rust
match pending.lookup(id) {
    LookupView::LiveNonZero(p) => p.transition_to_orphan(…),
    LookupView::LiveZero(p)    => …,
    LookupView::Orphan(p)      => …,
    LookupView::Released       => …,
}
```

Rejected because:

* `MountInner`'s storage type changes (the map's value type becomes an
  enum; `MountInner` may need a GAT or a typestate-erasing wrapper).
* Compile-time gain over witnesses is nil — both encodings reject the
  same bug at the same compile-time boundary; enum dispatch additionally
  forces the caller to write an exhaustive `match` it usually doesn't
  need.
* Higher churn at the storage layer; less likely to land in one PR.

The choice is locked: **witness-type idiom**. Implementation detail —
the witness struct must carry the lifetime `'p` of the `&mut Pending`
borrow that produced it, so the borrow checker invalidates the witness
the moment the borrow ends; that's what makes "stale witness across a
push-back into `Pending`" unrepresentable.

### 2.3 Typed handles for the path-side maps

Finding #3 (kernel_forget removes `hot[id]` before checking lifecycle)
is not a state-of-`state[id]` bug — it's about *which submaps the method
is allowed to touch*. The fix is a separate typed handle:

```rust
/// Proof that the caller has done the lifecycle check that says it's
/// safe to drop the hot-tier buffer for `id`. Constructed only via
/// `Pending::witness_kernel_forget(id)`, which itself returns Some
/// iff `state[id]` is None or matches a discharge-safe pattern.
pub(crate) struct KernelForgetWitness<'p> {
    id: u64,
    _borrow: PhantomData<&'p mut ()>,
}
```

`kernel_forget_inode` takes a `KernelForgetWitness`. The
construction logic is the FSM check; the method body cannot run on a
state that hasn't been checked.

---

## 3. How each r11 bug becomes impossible by construction

| # | r11 finding                                                           | Why it can't compile                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| - | --------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 1 | `transition_to_orphan` accepts `open_count == 0`                      | `Pending::transition_to_orphan(&mut self, w: Witness<'_, LiveNonZero>)`. `Witness<LiveNonZero>` is constructed only by `witness_live_nonzero(id)`, which returns `Some` iff `state[id] == Live { open_count >= 1 }`. There is no other constructor. Calling on a `LiveZero` node returns `None` at the witness-construction call site, the early-`?` propagates upward, and the transition never fires.                                                                                                                              |
| 2 | `drain_for_capture` drops `Live` with `open_count > 0`                | `drain_for_capture`'s contract is "drop `LiveZero`; preserve `LiveNonZero` (their fds will close mid-mount and finalise the Live exit path); preserve `Orphan` (open-unlinked POSIX)". `Released` is the absence-of-entry state — values loaded from the `state` map are one of the three resident variants, so the inner `match` is exhaustive over those three. A future refactor that adds a fourth resident state forces every match to be revisited; the "drop `LiveNonZero`" line cannot be written without naming `LiveNonZero` explicitly.        |
| 3 | `kernel_forget_inode` removes `hot[id]` before state check            | `kernel_forget_inode` takes a `KernelForgetWitness`. The witness is constructed by an FSM-aware function whose body IS the lifecycle check. The method body cannot reach the `self.hot.remove(&id)` line on a state for which that check did not pass — the witness wouldn't exist. The race window between "check passed" and "method body runs" is closed by the `&mut Pending` borrow held by the witness's lifetime.                                                                                                              |
| 4 | `rename_entry_with_options` calls `transition_to_orphan` on a symlink | This is a *caller* bug, but the same gating applies one level up. The caller has to construct a `Witness<LiveNonZero>` to call `transition_to_orphan`. A symlink or directory NodeId is never registered in `state` with a `LiveNonZero` discriminant (symlinks have no open/release lifecycle; `record_open` is never called for them, so the state never goes Live-with-handles). `witness_live_nonzero(symlink_id)` returns `None`. The caller's existing code becomes a no-op for symlinks instead of polluting the `state` map. |

The general shape: the four r11 findings collapse into one pattern (method
acts without checking state), and one fix (state check is the witness
constructor, not the method body).

---

## 4. Retrofit cost + plan

### 4.1 Callsite count (grounded against PR #182 head)

`pending.<method>(` callsites in `crates/mount/src/core.rs` on PR #182's head:
**44**. By method:

| Method                              | Callsites | Retrofit class            |
| ----------------------------------- | --------- | ------------------------- |
| `record_open`                       | 1         | state-gated; needs witness |
| `release_open_handle`               | (via flush path; called inside core's release flow — count covered by `take_hot_for_flush` + `record_warm_promotion`)  | state-gated |
| `transition_to_orphan`              | 2         | state-gated (finding #1)   |
| `drain_for_capture`                 | 1         | state-gated (finding #2)   |
| `kernel_forget_inode`               | 1         | typed handle (finding #3)  |
| `detach_path_hot_binding`           | 4         | path-side, no state gating |
| `mark_file_tombstone`               | 3         | path-side, no state gating |
| `clear_file_tombstone`              | 1         | path-side, no state gating |
| `mark_dir_removed`                  | 1         | path-side, no state gating |
| `clear_symlink`                     | 4         | path-side, no state gating |
| `install_symlink`                   | 2         | path-side, no state gating |
| `unlink_path_dropping_all`          | 1         | already-typed (drops all)  |
| `prepare_for_create_file`           | 1         | path-side, no state gating |
| `prepare_for_make_dir`              | 1         | path-side, no state gating |
| `seed_empty_hot`                    | 1         | hot tier; no state gating  |
| `rebase_hot_path`                   | 3         | hot tier; no state gating  |
| `coalesce_hot_to_node`              | 2         | hot tier; needs state hint |
| `republish_path`                    | 2         | path-side, no state gating |
| `install_hot_buffer`                | 2         | hot tier; needs state hint |
| `install_synthetic_warm`            | 1         | warm tier; no state gating |
| `hot_buffer_mut`                    | 4         | hot tier; no state gating  |
| `warm_entry_mut`                    | 1         | warm tier; no state gating |
| `hot_entry_or_insert_with`          | 1         | hot tier; needs state hint |
| `take_hot_for_flush`                | 1         | hot tier; needs state hint |
| `record_warm_promotion`             | 1         | warm tier; state-gated     |
| `take_path_keyed_for_rename`        | 1         | already-typed              |
| `reinstall_path_keyed_maps`         | 1         | already-typed              |

(Counts derived from `grep -nE 'pending\.<method>\(' crates/mount/src/core.rs`
on the PR #182 head; 44 lines total.)

**Of the 44, only ~5 actually need state-gating in the new API** — the
ones that drive lifecycle transitions:

* `record_open` × 1
* `transition_to_orphan` × 2
* `drain_for_capture` × 1
* `kernel_forget_inode` × 1

Plus the release-path callsites that fire inside `flush_node` / `release_node`
and currently go through `release_open_handle` indirectly. Call them ~3
additional sites that need witness threading.

The remaining ~36 callsites are path-side / hot-tier / warm-tier helpers that
don't gate on FSM state. They keep their current signatures.

**Retrofit headline: ~8 callsites need witness threading; ~36 callsites
unchanged.** The "every method gets type-state" version of this proposal is
not what we're doing; we're applying type-state to exactly the four lifecycle
transitions Codex flagged and stopping there.

### 4.2 Phased plan

Three phases, each its own impl sub-issue (§6):

1. **Substrate (sub-issue 1).** Introduce `Lifecycle`, `LiveNonZero`,
   `LiveZero`, `Orphan`, `Released`, `Witness<'p, S>`, `KernelForgetWitness<'p>`,
   and the four `witness_*` constructors on `Pending`. No retrofitting of
   callsites yet. Substrate-only; type-checks; no behaviour change.
2. **Transition retrofits (sub-issues 2, 3, and 4).** Change
   `transition_to_orphan`, `drain_for_capture`, `kernel_forget_inode`
   signatures; thread witnesses at the ~8 callsites in `core.rs`. Findings
   #1, #2, #3, #4 all close in this phase.
3. **Property-test harness (sub-issue 5).** Activate `heddle#199 §5.3`.
   Generate random FUSE-callback sequences; assert reachable `state` matches
   the FSM. Catches the bugs the types still can't see — specifically,
   FSM-divergence bugs at the kernel boundary (e.g., kernel sending a
   `release` for an fh `record_open` never saw).

Phase 3 must follow phase 2, not precede it: property tests against the
old API would lock in the bug-prone surface.

---

## 5. PR #182 outcome

**Recommendation: PR #182 is superseded by a fresh PR against the new API.**

Reasoning:

* PR #182 has 11 cumulative rounds of diff (5,723 additions / 185 deletions,
  12 files). It already represents 11 reviewer cycles' worth of context.
* The type-state retrofit changes the signatures of three methods
  (`transition_to_orphan`, `drain_for_capture`, `kernel_forget_inode`) and
  introduces a witness substrate. Every r6-r10 commit that touched
  `core.rs`'s call sites for those methods now has merge conflicts against
  the new shape. The branch survives in principle, but reviewing the result
  is reviewing two layered refactors at once.
* The r6-r10 *tests* are the part worth preserving. The 17 fixture tests
  for r10's encapsulation are FSM-shape tests; they should land on `main`
  as part of phase 1 substrate work and survive the rewrite. The impl
  sub-issues' ACs must include "the r6-r10 fixture tests still apply and
  still pass" as an explicit gate.

**Action:**

1. Cherry-pick the r10 `pending.rs` encapsulation (the module split itself)
   onto main as a separate small PR. This locks in the wins from PR #182
   without the in-flight r11 fixes.
2. Open phase-1 substrate PR against main (§6 sub-issue 1) on top of that.
3. Close PR #182 with a comment pointing to the cherry-picked encapsulation
   PR + the substrate PR. Retain the branch (`task/180-mount-fuse-implement-write-side-ops-crea`)
   in case any r6-r10 commits are worth pulling commit-by-commit when the
   phase-2 retrofits land.

**Alternative considered: PR #182 continues with the type-state rewrite
landing on the same branch.** Rejected — at that point the rewrite is
substantially-the-whole-PR; the prior 11 rounds become churn in the
reviewer's mental model rather than incremental progress.

---

## 6. Impl sub-issues (sketches — not yet filed)

Five issues. Each is independently shippable in dependency order.

### Sub-issue 1 — `heddle: type-state substrate for Pending`

```markdown
## Premise
heddle#206 locked Strategy C (full type-state via the witness-type idiom).
This issue lands the substrate: types, traits, witness constructors. No
callsite retrofitting yet.

## Acceptance criteria
- [ ] `Lifecycle` sealed trait + `LiveNonZero` / `LiveZero` / `Orphan` /
      `Released` ZSTs in `crates/mount/src/pending.rs`.
- [ ] `Witness<'p, S: Lifecycle>` struct + the four `Pending::witness_*`
      constructors.
- [ ] `KernelForgetWitness<'p>` + its constructor.
- [ ] Substrate is `pub(crate)`-visible from `core.rs` but unused there.
- [ ] r10's 17 fixture tests pass unchanged.
- [ ] `bash scripts/check-no-silent-default-tree-load.sh`,
      `cargo test -p heddle-mount`,
      `cargo clippy --workspace --all-targets -- -D warnings` all green.

## Blocked by
- HeddleCo/heddle#206 (decision doc)

## File scope
- `crates/mount/src/pending.rs`
```

### Sub-issue 2 — `heddle: retrofit transition_to_orphan to witness-gated API (fixes r11 #1, #4)`

```markdown
## Premise
With sub-issue 1's substrate in place, change `Pending::transition_to_orphan`
to take a `Witness<'_, LiveNonZero>` and return a `Witness<'_, Orphan>`.
Thread the witness at the 2 call sites in `core.rs`
(`unlink_entry`, `rename_entry_with_options`). Symlink / dir branches
of the rename caller short-circuit when `witness_live_nonzero` returns
`None` — that's the fix for r11 finding #4.

## Acceptance criteria
- [ ] `Pending::transition_to_orphan` signature changed; old signature
      removed.
- [ ] Both `core.rs` call sites updated.
- [ ] Symlink/dir branch of rename no longer mutates `state[id]` for
      non-Live nodes.
- [ ] r10 fixture tests still pass; add a regression test that a
      symlink rename leaves `state` unchanged.
- [ ] Local CI parity gates green.

## Blocked by
- HeddleCo/heddle#<sub-issue 1 number>

## File scope
- `crates/mount/src/pending.rs`
- `crates/mount/src/core.rs`
```

### Sub-issue 3 — `heddle: retrofit drain_for_capture to typed Live/Orphan split (fixes r11 #2)`

```markdown
## Premise
`Pending::drain_for_capture` currently uses a runtime `match` on
`NodeState`. Convert the implementation to exhaustively match the
state types and use the types to encode "drop `LiveZero`; preserve
`LiveNonZero`; preserve `Orphan`". The signature gains no parameters
(capture-drain is whole-map by construction), but the inner loop is
rewritten to surface the `LiveNonZero` / `LiveZero` distinction so
the "drop Live with `open_count > 0`" bug (r11 #2) becomes unwritable.

## Acceptance criteria
- [ ] Inner `match` is exhaustive over the three *resident* `Lifecycle`
      types — `LiveNonZero`, `LiveZero`, `Orphan`. `Released` is the
      absence-of-entry state and is never stored in the map, so the
      value loaded via `map.get(...)` cannot be `Released`; the impl
      should document this with a code comment so future readers don't
      add a `Released` arm.
- [ ] `LiveNonZero` entries are preserved across capture (POSIX
      last-close-wins regression test).
- [ ] r10 fixture tests still pass.
- [ ] New proptest fixture: random FSM trace ending in a capture
      preserves all open-fd refcounts.
- [ ] Local CI parity gates green.

## Blocked by
- HeddleCo/heddle#<sub-issue 1 number>

## File scope
- `crates/mount/src/pending.rs`
```

### Sub-issue 4 — `heddle: retrofit kernel_forget_inode to KernelForgetWitness (fixes r11 #3)`

```markdown
## Premise
`Pending::kernel_forget_inode` currently removes `hot[id]` before
performing the lifecycle check. Change it to take a
`KernelForgetWitness<'_>` whose constructor IS the lifecycle check.
Thread at the one call site in `core.rs` (`MountInner::invalidate`).

## Acceptance criteria
- [ ] `kernel_forget_inode` signature changed; old signature removed.
- [ ] `MountInner::invalidate` call site updated to construct the
      witness; the `None` branch of the constructor short-circuits
      the entire forget path (matches the prior intent).
- [ ] Regression test: a kernel forget racing an open Orphan fd
      does not drop the bytes (the test that motivated this finding).
- [ ] r10 fixture tests still pass.
- [ ] Local CI parity gates green.

## Blocked by
- HeddleCo/heddle#<sub-issue 1 number>

## File scope
- `crates/mount/src/pending.rs`
- `crates/mount/src/core.rs`
```

### Sub-issue 5 — `heddle: property-test harness for Pending FSM (activates heddle#199 §5.3)`

```markdown
## Premise
The type-state retrofit (sub-issues 1-4) encodes the FSM at the API
boundary inside `Pending`. The FUSE-callback boundary with the kernel
is still untyped — the kernel can send a `release(fh)` for an fh
`record_open` never saw, or a `forget(node)` for a NodeId never
opened. Catch divergence with a proptest harness that generates
arbitrary FUSE-callback sequences and asserts the final `state` is
reachable from `Live` via documented transitions.

## Acceptance criteria
- [ ] `proptest` strategy generating sequences of {lookup, open,
      write, release, forget, unlink, rename} against a fresh mount.
- [ ] Property: every reachable state matches the §1 FSM in
      `docs/design/mount-posix-semantics.md`.
- [ ] Property: open-count is always non-negative (refcount sanity).
- [ ] At least one shrunk counterexample for a deliberately-broken
      build, as evidence the harness catches divergence.
- [ ] Local CI parity gates green.

## Blocked by
- HeddleCo/heddle#<sub-issue 2 number>
- HeddleCo/heddle#<sub-issue 3 number>
- HeddleCo/heddle#<sub-issue 4 number>

## File scope
- `crates/mount/src/pending.rs` (test module only)
- `crates/mount/tests/pending_fsm_proptest.rs` (new file)
```

---

## 7. Risks worth flagging

1. **Type-state rewrites are invasive.** Even with the scoped retrofit
   (§4: ~8 callsites need witness threading, not 44), the substrate PR
   plus three retrofit PRs is four PRs of churn against a hot file
   (`core.rs`). Sequencing matters: every retrofit PR depends on the
   substrate PR. If the substrate PR sits in review, every other piece
   blocks behind it.

2. **`MountInner` storage shape is preserved by the witness-type
   idiom**, but this property is load-bearing on the *idiom* choice. If
   future work tries to convert to enum-dispatch (§2.2.2), the storage
   type changes and `MountInner` will need a coordinated update. The
   doc-comment on `Pending` should call this out so a drive-by
   refactor doesn't pick the other idiom.

3. **PR #182's r6-r10 test coverage is valuable and must survive the
   rewrite.** The retrofit sub-issues each list "r10 fixture tests
   still pass" as an explicit AC. If a retrofit forces a fixture-test
   rewrite, that's a signal the API change is wider than intended;
   the sub-issue should stop and post a SCOPE comment rather than
   silently revising the tests.

4. **Sub-issue 5 (proptest) discovers FSM bugs the type system can't
   see.** Two outcomes worth flagging in advance: (a) the harness
   reveals a divergence in production behaviour against the §1 FSM
   doc, in which case one of them is wrong (probably the doc; we
   trust the kernel's transition reality more than the spike's model);
   (b) the harness reveals a FSM bug in `Pending` itself that the
   types don't catch — the most likely candidate is "kernel sends
   release for unopened fh", which is technically a Pending-side bug
   even though the witness-type idiom can't reject it at compile time.
   If (b) happens, file a follow-up impl issue rather than expanding
   the proptest sub-issue.

5. **`Released` is the absence-of-entry state, not a concrete
   variant.** The Rust encoding treats `Released` as a ZST you can
   own but never store in the map (the map's value type is one of
   the other three). This is consistent but easy to misread when
   skimming. The substrate PR's doc-comments on `Lifecycle` must
   call this out.

---

## References

* [`docs/design/mount-posix-semantics.md`](./mount-posix-semantics.md) — heddle#199 spike; owns the FSM and storage model.
* `crates/mount/src/pending.rs` — heddle#180 r10 encapsulation. 766 lines on PR #182 head.
* `crates/mount/src/core.rs` — 44 `pending.<method>(` callsites enumerated in §4.1.
* PR #182 Codex review threads r6 → r10 (history) + r11 findings `3293575534`,
  `3293575537`, `3293575538`, `3293575541` (this spike's input).
* `scripts/check-no-direct-pending-mutation.sh` — r10 belt-and-braces static check; survives the retrofit unchanged.
