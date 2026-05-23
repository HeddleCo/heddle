# mount/ POSIX-semantics model

Spec for the inode-lifecycle / cache-coherency contract that `crates/mount/` must preserve. Future write-side work is audited against this document, not reconstructed from grep.

Line citations below reference the world under audit: PR #182 (branch `task/180-mount-fuse-implement-write-side-ops-crea`, file `crates/mount/src/core.rs` and `crates/mount/src/fuse.rs`). `main` predates the orphan machinery — citing it would be misleading. The "PR #182 continuation plan" section below describes how the locked decisions land back in that PR.

## Locked decisions (do not relitigate in this doc)

| Decision | Summary |
|---|---|
| **A** | Cache layer model: `Pending` collapses to NodeId-keyed throughout. `warm` becomes `BTreeMap<NodeId, PendingEntry>`. `orphan_warm` is deleted (bytes stay in `warm[node_id]` across the Live → Orphan transition). `hot_by_path` stays as the path → NodeId reverse-index. |
| **B** | PR #182 strategy: refactor in-place. New commits on `task/180-...` collapse the dual representation and rewrite the r8-era `orphan_warm` migration tests against the unified shape before resuming review. |
| **C** | Lifecycle representation: a `NodeState` enum (`Live`, `Orphan { open_count: u32 }`). Replaces `orphans: BTreeSet<NodeId>` + `open_handles: BTreeMap<NodeId, u32>`. The type system makes "orphaned with no open count" and "open count without orphan flag" unrepresentable. |

The agent owns determining whether `Live` itself needs a refcount (see §2.4). Best read of the current code: `open_handles` is consulted exclusively to drive the `Orphan(N)` → `Released` decision in `release_node` (core.rs:2535, 2562-2571). No Live-state code path consults the count. Therefore: `Live` does not need to carry one; `open_handles` entries for Live nodes are vestigial and the enum form drops them. If a future LRU / memory-pressure eviction policy needs per-Live refcounts, extend the enum then — the migration is mechanical.

---

## 1. Inode lifecycle FSM

### 1.1 States

```rust
enum NodeState {
    Live,
    Orphan { open_count: u32 },
    // Released: entry absent from the `state` map.
}
```

A NodeId is **Live** iff it owns the current binding for some path (i.e. appears in `inodes.by_path` as a value). It transitions to **Orphan** when its directory entry is removed (`unlink`) or replaced (`rename`-over) while open file descriptors still reference it. It transitions to **Released** (entry removed from the state map) when the last fd closes — for an Orphan, on the final `release`; for a Live node with no open fds at the time its directory entry is removed, immediately.

### 1.2 Transitions

| # | From | Trigger | To | Side effects |
|---|---|---|---|---|
| T1 | `Live` | `unlink(path)` where N ≥ 1 fds open on the resolved NodeId | `Orphan { open_count: N }` | `tombstones += path`; `hot_by_path -= path`; `inodes.by_path -= path`; `hot[node_id]`, `warm[node_id]`, `symlinks[path]?` are LEFT IN PLACE (bytes survive). |
| T2 | `Live` | `unlink(path)` where N = 0 fds open | (entry removed → Released) | Same path-side bookkeeping as T1, plus drop `hot[node_id]`, `warm[node_id]`. |
| T3 | `Live` (= the displaced destination of a rename-over) | `rename(src, dst)` with N ≥ 1 fds open on `dst`'s current NodeId | `Orphan { open_count: N }` | `tombstones += dst` is NOT set (the source claims `dst` immediately); `hot_by_path[dst]` is rebound to the source's NodeId; `inodes.by_path[dst]` rebinds to the source NodeId. Displaced NodeId keeps `hot[id]` / `warm[id]`. |
| T4 | `Live` (= the displaced destination of a rename-over) | `rename(src, dst)` with N = 0 fds open on `dst` | (entry removed → Released) | Same path rebind as T3, plus drop `hot[id]` and `warm[id]` for the displaced NodeId. |
| T5 | `Orphan { N }` | FUSE `release(node)` with N > 1 | `Orphan { N - 1 }` | None at the per-NodeId-state level (an inner `flush_node` may still promote the hot buffer through the *non-orphan* code path; orphan branches are no-ops). |
| T6 | `Orphan { 1 }` | FUSE `release(node)` (final) | (entry removed → Released) | Drop `hot[node_id]`, `warm[node_id]`. Do NOT promote to warm (an orphan's bytes are unreachable — no path resolves to this NodeId). |
| T7 | Released | (none — terminal) | — | — |

Notes:

* **FUSE `flush` is NOT a transition trigger.** `flush` fires per-descriptor close (once per `dup`, once per `fork`-inherit close); only `release` fires on the per-inode final close. Confusing the two was the r8 finding (Codex Thread [3293235165](https://github.com/HeddleCo/heddle/pull/182#discussion_r3293235165)) — see Bug Class B in §4.
* **`forget`** retires the `by_id` entry; it does NOT mutate `NodeState`. A Live node forgotten is still Live (rare in practice — `forget` typically follows `unlink` + final close). An Orphan node's `forget` arrives after T6, so the `state` entry is already gone.
* **`create_file`** on a path that was just unlinked mints a *fresh* NodeId (because T1 cleared `inodes.by_path[path]`). The fresh NodeId starts Live; the orphaned NodeId from T1 stays Orphan until its own `release`. POSIX: open-unlinked temp files must not alias a replacement at the same path. Current code enforces this at core.rs:1367.

### 1.3 ASCII diagram

```
                        unlink (N=0)
                   ┌───────────────────────────────────┐
                   │                                   │
                   │      rename-over (N=0, displaced) │
                   │   ┌───────────────────────────────┤
                   │   │                               ▼
        ╭──────────┴───┴─╮  unlink (N≥1)       ╭──────────────╮
   ─►   │     Live       │ ───────────────►    │  Orphan { N }│
        │                │  rename-over (N≥1   │              │
        ╰──────────┬─────╯  on displaced)      ╰──────┬───────╯
                   │                                  │
                   │                                  │ release (N→N-1)
                   │                                  │ (self-loop while N>1)
                   │                                  │
                   │                                  ▼
                   │                          ╭──────────────╮
                   └────────────────────────► │   Released   │
                          (final release)     │ (entry gone) │
                                              ╰──────────────╯
```

### 1.4 Invariants

* `state[node_id] == Orphan { N }` ⇔ N kernel fds still hold `node_id` AND no path in `inodes.by_path` resolves to `node_id`.
* `state[node_id] == Live` ⇒ exactly one path in `inodes.by_path` resolves to `node_id` (file/symlink case) or `node_id` is a directory/root.
* Released ⇒ `state` has no entry; `hot`, `warm`, `hot_by_path` likewise have no entry for `node_id`.
* Bytes stored under `hot[node_id]` / `warm[node_id]` outlive transitions T1 and T3 — the orphan path keeps reading its own data through the surviving fd.

---

## 2. Cache layer model

### 2.1 Target shape (post-Decision A)

```rust
#[derive(Default)]
struct Pending {
    // Hot tier: per-NodeId open-file buffers.
    hot: BTreeMap<NodeId, HotBuffer>,

    // Warm tier: per-NodeId promoted bytes.
    // CHANGED from path-keyed (`BTreeMap<PathBuf, PendingEntry>`).
    // Bytes survive unlink / rename-over without a migration step.
    warm: BTreeMap<NodeId, PendingEntry>,

    // Path → NodeId reverse-index for hot. Stays path-keyed because
    // FUSE `lookup` and rename arrive with paths, not NodeIds.
    hot_by_path: BTreeMap<PathBuf, NodeId>,

    // Directory-entry-level concepts — stay path-keyed.
    tombstones: BTreeSet<PathBuf>,
    dir_tombstones: BTreeSet<PathBuf>,
    explicit_dirs: BTreeSet<PathBuf>,

    // Symlink target bytes — created as a directory-entry, never
    // opened, so path-keyed is the natural shape. Removed on unlink.
    symlinks: BTreeMap<PathBuf, Vec<u8>>,

    // Lifecycle state — Decision C. Replaces `orphans` + `open_handles`.
    state: BTreeMap<NodeId, NodeState>,

    // REMOVED: `orphan_warm` (Decision A — bytes stay in `warm[node_id]`).
    // REMOVED: `orphans: BTreeSet<NodeId>` (Decision C — folded into state).
    // REMOVED: `open_handles: BTreeMap<NodeId, u32>` (Decision C — folded into state).
}
```

`HotBuffer` keeps its current shape (core.rs:347-361). `PendingEntry` keeps its current shape (core.rs:365-370).

### 2.2 Per-field contract

| Field | Read by | Written by | Behavior on Live → Orphan |
|---|---|---|---|
| `hot: BTreeMap<NodeId, HotBuffer>` | `read`, `write`, `apply_truncate`, `attrs`, `flush_node`, `release_node`, capture | `write`, `create_file`, `apply_truncate`, `flush_node` (drains) | Unchanged. Bytes remain reachable by the open fd via NodeId. |
| `warm: BTreeMap<NodeId, PendingEntry>` | `read` (orphan + non-orphan branches collapse), `attrs`, `pending_lookup`, capture | `flush_node` (on promotion), `apply_pending_to_tree` | Unchanged. **No migration step.** This is the whole point of Decision A. |
| `hot_by_path: BTreeMap<PathBuf, NodeId>` | `pending_lookup`, `rename`, `create_file`, `apply_truncate` (for path → owner check) | `create_file`, `apply_truncate` (Live only), `rename`, `unlink_entry` | `remove(path)` — the path no longer resolves to this NodeId. |
| `tombstones: BTreeSet<PathBuf>` | `read`/`attrs`/`lookup` (suppress captured-tree entries), capture (plant deletions) | `unlink_entry`, `create_file` (clear), `apply_truncate` (clear), `rename` | `insert(path)` on T1; left untouched on T3 because `rename` rebinds. |
| `dir_tombstones: BTreeSet<PathBuf>` | capture-walk, `lookup`, `enumerate` | `rmdir_entry`, `make_dir` (clear), `create_file` (clear) | N/A — directories don't go through Live/Orphan/Released; an open dir handle does not survive `rmdir`. |
| `explicit_dirs: BTreeSet<PathBuf>` | `pending_dir_exists`, `enumerate` | `make_dir`, `rmdir_entry` (remove) | N/A. |
| `symlinks: BTreeMap<PathBuf, Vec<u8>>` | `read_link`, `attrs`, capture | `create_symlink`, `unlink_entry` (remove), `rename` (move) | `remove(path)`. Symlinks are not openable for IO; no orphan story applies. |
| `state: BTreeMap<NodeId, NodeState>` | every callback that branches on Live vs Orphan | `unlink_entry`, `rename_entry` (T1/T3), `on_open` (bump count if already Orphan — see §2.3), `release_node` | `insert(node_id, Orphan { N })` where N is taken from `state` if Live-with-count is later introduced; for now, N is taken from a side count captured at unlink time (see §2.3). |

### 2.3 What "open count" tracks under the new shape

Decision C states `Live` carries no count. The open count is born at the transition T1/T3 and lives only inside `Orphan { open_count }`. Mechanics:

* `on_open(node_id)`: if `state[node_id] == Orphan { N }`, set `Orphan { N + 1 }`. If absent (Live), record nothing — no count needed.
* `release_node(node_id)`: if `state[node_id] == Orphan { N }` then either decrement (N > 1) or remove the entry + drop bytes (N == 1). If absent, treat as Live release: flush_node only.
* `unlink_entry(node_id)`: to compute the initial N at T1, count open handles *at this instant*. Two options:
  1. Keep a `BTreeMap<NodeId, u32> open_count_live` (functionally identical to today's `open_handles`, scoped to Live).
  2. Extend the enum to `Live { open_count: u32 }` and rely on `on_open` / `release_node` to maintain it.

Option 2 is structurally cleaner but adds a refcount on every Live node — which today, no Live-state code path consults. **Recommendation: Option 1**, with a TODO comment to revisit if a Live-side consumer appears. The refactor PR can ship either; the spec is silent on the choice because both satisfy the FSM in §1.

### 2.4 Migration steps that no longer exist

Pre-spike (PR #182 r8 state), every directory operation that orphaned an inode had to migrate bytes between two parallel cache representations:

| Operation | Pre-spike migration (current code at core.rs line) | Post-spike (collapsed shape) |
|---|---|---|
| `unlink_entry` of warm-tier path with open fd | `pending.warm.remove(path)` → `pending.orphan_warm.insert(node_id, entry)` (core.rs:1354-1356) | Nothing. `warm[node_id]` already keyed correctly. |
| `rename`-over of warm-tier dest with open fd | `pending.warm.remove(new_path)` → `pending.orphan_warm.insert(displaced_id, entry)` (core.rs:1683-1685, 1733-1735) | Nothing. Displaced NodeId's `warm[id]` stays put. |
| `read` / `attrs` / `write` / `apply_truncate` on an orphaned NodeId | Branch on `pending.orphans.contains(&id)`; if yes, consult `orphan_warm[id]` instead of `warm[path]` | Branch on `state[id] == Orphan {..}` for *correctness gates* (don't rebind `hot_by_path`, don't clear tombstones). Byte lookup goes through `warm[id]` either way. |
| `release_node` final-release of orphan | `pending.orphans.remove(&id)` + `pending.hot.remove(&id)` + `pending.orphan_warm.remove(&id)` (core.rs:2562-2571) | Match on `state[id]`; if `Orphan { 1 }`, remove `state[id]`, `hot[id]`, `warm[id]`. |

The class of bug "code remembered to update one keyed-view, not the parallel one" (the r6 → r7 → r8 → r9 pattern) is **structurally absent** post-spike: there is no parallel view to forget.

---

## 3. FUSE callback contract

Callbacks listed are the ones implemented on the PR #182 branch's `FuseShell` (fuse.rs:330-820) plus `forget` (fuser default — exposed through `Inodes::forget` at core.rs:305-333). "Lock pos." references the lock hierarchy documented at core.rs:459-490: `write_mu` ⊐ `state` ⊐ `pending` ⊐ `inodes`. Structural-mutation methods take `write_mu` first; pure-read methods skip it.

Legend:
* **NodeState support**: which states the callback must handle correctly. `*` = both Live and Orphan have meaningful paths.
* **Reads / Writes**: which `Pending` fields the callback touches (post-spike shape — see §2.1).
* **POSIX guarantee**: the property the callback must preserve.
* **Lock pos.**: which lock(s) the callback acquires and in what order.

| Callback | NodeState support | Reads | Writes | POSIX guarantee | Lock pos. |
|---|---|---|---|---|---|
| `lookup` (fuse.rs:402) | Live only (Orphan has no path) | `hot_by_path`, `warm` (via inodes.by_path → NodeId), `tombstones`, `dir_tombstones`, `symlinks`, `explicit_dirs` | — | ENOENT for tombstoned paths; consistent NodeId for the same path across calls (inode coalescing at core.rs:262-287). | `state` (read), `pending`, `inodes` |
| `getattr` / `attrs` (fuse.rs:426, core.rs:3210) | * | `state`, `hot[id]`, `warm[id]`, `symlinks`, captured tree | — | Size / mode reflect the latest write through any fd, including open-unlinked fds. | `state` (read), `pending`, `inodes` |
| `setattr` / `set_attrs` (fuse.rs:732, core.rs:1900) | * | `state` | `hot[id]` (size on truncate), `warm[id]` (mode), **NOT** `hot_by_path` when Orphan | A chmod / ftruncate on an unlinked-open fd must not republish the path. Recreated-path inode must not inherit the orphan's mode (Codex Thread [3293164731](https://github.com/HeddleCo/heddle/pull/182#discussion_r3293164731)). | `write_mu`, `state`, `pending`, `inodes` |
| `read` (fuse.rs:433, core.rs:2750) | * | `state`, `hot[id]`, `warm[id]`, captured blob | — | Reads through an open-unlinked fd see the inode's bytes, not a replacement at the same path. | `state` (read), `pending`, `inodes` |
| `write` (fuse.rs:502, core.rs:2879) | * | `state` | `hot[id]`; if Live: also `hot_by_path[path]`, clears `tombstones[path]`; if Orphan: only `hot[id]` | Writes via an open-unlinked fd must not resurrect the path or clear its tombstone (Codex Thread [3276025807](https://github.com/HeddleCo/heddle/pull/182#discussion_r3276025807) + r6/r7 sweeps). | `write_mu`, `state`, `pending`, `inodes` |
| `flush` (fuse.rs:520, core.rs:2476) | * | `state` | `warm[id]` (promote from hot, Live only); `hot[id]` (drain). Orphan branch: no-op. | Per-descriptor close. Must NOT clear orphan state — flush fires multiple times per inode (Codex Thread [3293235165](https://github.com/HeddleCo/heddle/pull/182#discussion_r3293235165)). | `state`, `pending` |
| `release` / `release_node` (fuse.rs:536, core.rs:1138, 2528) | * | `state` | `state[id]` (decrement / remove); on final + Orphan: drop `hot[id]`, `warm[id]`. On final + Live: same as flush. | Per-inode final close. The "inode lives until the last close" promise ends here for orphans. | `state`, `pending` |
| `open` / `on_open` (fuse.rs:356, core.rs:1146) | * (but Orphan-open is rare — only via still-cached fh) | `state` | `state[id]` (bump count if Orphan; no-op if Live, per §2.3 Option 1) | Refcount accuracy — every open must be paired with a release. | `state`, `pending` |
| `opendir` | Live only (directories don't orphan) | — | — | — | (none) |
| `releasedir` | Live only | — | — | — | (none) |
| `readdir` (fuse.rs:456, core.rs:`enumerate`) | Live only | `hot_by_path`, `warm`, `tombstones`, `dir_tombstones`, `explicit_dirs`, `symlinks`, captured tree | — | Tombstoned paths suppressed; pending-only files/dirs visible. | `state` (read), `pending`, `inodes` |
| `forget` (core.rs:305) | * | — | `inodes.by_id`, `inodes.by_path`, `inodes.by_hash` | After `forget`, the NodeId may be reused. Released entries cleaned up here. | `inodes` |
| `lookup` of tombstoned path | — | `tombstones` | — | ENOENT — captured-tree entry is masked. | (read-only lock) |
| `mkdir` (fuse.rs:608, core.rs:`make_dir`:1278) | Live only | `dir_tombstones`, captured tree | `explicit_dirs`, clears `dir_tombstones[path]`, clears `tombstones[path]` (file → dir replacement) | EEXIST if a Live file/dir already lives at the target. | `write_mu`, `state` (read), `pending`, `inodes` |
| `rmdir` (fuse.rs:671, core.rs:1374) | Live only | `enumerate` (must be empty) | `explicit_dirs` (remove), `dir_tombstones` (insert) | ENOTEMPTY when overlay children exist. No orphan story for dirs. | `write_mu`, `state` (read), `pending`, `inodes` |
| `create` (fuse.rs:555, core.rs:`create_file`:1209) | mints fresh Live NodeId | `tombstones`, `inodes.by_path` | `inodes.by_path[path] = new_id`, `hot[new_id]`, `hot_by_path[path] = new_id`, clears `tombstones[path]` | After `open(p) → unlink(p) → create(p)`, the new file must be a fresh NodeId (core.rs:1367 enforces). | `write_mu`, `state`, `pending`, `inodes` |
| `unlink` (fuse.rs:662, core.rs:1316) | Live → Orphan or Released (T1/T2) | `hot_by_path`, `inodes.by_path` | `state[id] = Orphan{N}` or remove; `tombstones += path`; `hot_by_path -= path`; `inodes.by_path -= path`; `symlinks -= path`. **DOES NOT** touch `hot[id]` or `warm[id]` for T1. | The inode survives behind open fds (POSIX "open unlinked file"). The directory entry doesn't. | `write_mu`, `state`, `pending`, `inodes` |
| `rename` (fuse.rs:680, core.rs:1407, 1429) | Source Live; destination Live → Orphan or Released (T3/T4) | `hot_by_path`, `inodes.by_path`, `state`, `tombstones`, `dir_tombstones` | Source: `inodes.by_path[src] = ..` rebinds to `dst`; `tombstones += src`. Destination: `state[dst_id] = Orphan{N}` or remove. Both NodeIds keep their `hot[id]` / `warm[id]`. | RENAME_NOREPLACE atomicity: the existence check and the mutation must land under one `write_mu` acquisition (Codex Thread [3293235163](https://github.com/HeddleCo/heddle/pull/182#discussion_r3293235163)). | `write_mu`, `state`, `pending`, `inodes` |
| `link` (fuse.rs:809) | Not implemented — returns EOPNOTSUPP | — | — | — | — |
| `symlink` (fuse.rs:773, core.rs:2082) | mints Live NodeId, kind=Symlink | `tombstones`, `dir_tombstones` | `inodes.by_path[path] = id`, `symlinks[path] = target`, clears `tombstones[path]` | EEXIST if path is currently bound. Symlink target bytes are opaque (kernel-provided). | `write_mu`, `state`, `pending`, `inodes` |
| `readlink` (fuse.rs:793, core.rs:2121) | Live only | `symlinks`, captured tree | — | Returns the raw target bytes. | `state` (read), `pending` |
| `truncate` (delivered as `setattr(size=N)`) | * | `state` | `hot[id]` (resize); if Live: also rebinds `hot_by_path`, clears tombstone. If Orphan: only `hot[id]` (Codex Thread [3293164722](https://github.com/HeddleCo/heddle/pull/182#discussion_r3293164722)). | Truncate via an open-unlinked fd does not resurrect the path. | `write_mu`, `state`, `pending`, `inodes` |

---

## 4. Bug-class taxonomy

Every Codex finding on PR #182 r6 — r9 fits one of the four classes below. Each class gets:
* a **predicate** — the syntactic check a reviewer (human or static) can apply,
* a **status post-spike** — whether Decision A + C eliminates the class structurally, or whether it remains a discipline concern.

### Class A — Cache-layer asymmetry

**Description.** Two parallel cache representations (`hot_by_path` path-keyed vs `hot` NodeId-keyed; `warm` path-keyed vs `orphan_warm` NodeId-keyed). Code updates one view, forgets the parallel one. Every directory op that orphans an inode must migrate bytes between maps; every read-side op that hits an orphaned NodeId must branch to a different lookup map.

**Findings of this class.**

| Round | Codex thread | Surface |
|---|---|---|
| r6 | [3276025807](https://github.com/HeddleCo/heddle/pull/182#discussion_r3276025807) | unlink left `hot_by_path[path] → old_id` stale; recreate aliased the unlinked inode |
| r7 | [3293164722](https://github.com/HeddleCo/heddle/pull/182#discussion_r3293164722), [3293164729](https://github.com/HeddleCo/heddle/pull/182#discussion_r3293164729), [3293164731](https://github.com/HeddleCo/heddle/pull/182#discussion_r3293164731) | truncate / setattr / rename-over re-published `hot_by_path` for orphans, or applied mode changes through a path that now bound a different inode |
| r8 | [3293235162](https://github.com/HeddleCo/heddle/pull/182#discussion_r3293235162), [3293235164](https://github.com/HeddleCo/heddle/pull/182#discussion_r3293235164) | rename-over / unlink dropped `warm[path]` for paths whose inode was orphaning — bytes were the only copy a surviving fd had. Fix added `orphan_warm` (the second parallel view this spike is collapsing). |
| r9 | [3293307302](https://github.com/HeddleCo/heddle/pull/182#discussion_r3293307302) | unlink-of-open dropped `hot[node_id]` immediately — the pre-flush hot buffer was the only copy of those bytes. |

**Predicate (pre-spike, manual review).** Any write to a `pending.X[node_id]` or `pending.X[path]` map must be audited for orphan-state implications: does the NodeId's *other* keyed-view need a parallel update? Does the directory-entry-level view need to be cleared/preserved separately from the byte-level view?

**Predicate (post-spike).** **N/A — this class is structurally gone.** Decision A eliminates the dual representation. There is exactly one byte storage location per (NodeId, tier): `hot[id]` and `warm[id]`. Path-keyed structures (`hot_by_path`, `tombstones`, `dir_tombstones`, `explicit_dirs`, `symlinks`) are directory-entry-level concepts whose mutation is unrelated to byte storage. The "remember the parallel view" mental load disappears.

### Class B — Lifetime-event confusion

**Description.** FUSE distinguishes `flush` (per-descriptor close — fires multiple times per inode for `dup`, `fork`, etc.) from `release` (per-inode final close — fires exactly once after the last fd referencing the inode is closed). Code that clears orphan state in `flush` clears it too early; code that promotes hot-tier bytes only in `release` may miss a flush-driven `fsync`.

**Finding of this class.** r8, Codex Thread [3293235165](https://github.com/HeddleCo/heddle/pull/182#discussion_r3293235165): orphan marker cleared on `flush`, causing the next write through a surviving fd to take the *non*-orphan branch and republish the path.

**Predicate.** Any orphan-state clear or final-release-only side effect must live in the `release` path, not `flush`. The `flush` path may promote hot → warm (Live only) and otherwise must be a no-op for orphan-state lifecycle. Reviewers reading any code in `core.rs::flush_node` should ask: "does this depend on this being the *final* close?" If yes, it's in the wrong callback.

**Post-spike status.** Class remains a discipline concern — the FUSE distinction is part of the kernel ABI, not something a type can encode. The `NodeState` enum partially mitigates by making "Orphan → Live" a non-transition (no way to "un-orphan"), so accidentally promoting an Orphan back to Live in `flush` is now unrepresentable. The remaining failure mode — accidentally calling `state.remove(id)` from `flush` — is catchable by lint or a single audit on the `flush_node` body.

### Class C — TOCTOU on atomicity flags

**Description.** A flag whose semantics depend on atomicity (RENAME_NOREPLACE: "fail if destination exists"; O_EXCL: "fail if path exists") is checked in one critical section and acted on in another. Another writer can slip between the check and the action.

**Finding of this class.** r8, Codex Thread [3293235163](https://github.com/HeddleCo/heddle/pull/182#discussion_r3293235163): RENAME_NOREPLACE check (a lookup) ran before `rename_entry`'s lock acquisition; a racing writer could create the destination between the two. Fix: introduce `write_mu` at core.rs:507 as a coarse serialization point for all write-side mutations and hold it across check + mutation.

**Predicate.** Any flag that affects mutation atomicity (RENAME_NOREPLACE, O_EXCL, O_CREAT, etc.) must be checked *inside* the same lock acquisition as the mutation it gates. The `write_mu` mutex (core.rs:499-507) is the discipline: `let _g = write_mu.lock(); /* check */ /* mutate */ /* g drops */`. Reviewers should flag any `lookup` followed by `rename_entry` / `create_file` / `mkdir_entry` in separate lock scopes.

**Post-spike status.** Class remains. Decisions A and C don't address lock discipline. The mitigation is convention + the `write_mu` lock ordering documented at core.rs:459-490; codifying the rule as a clippy lint ("every reference to RENAME_NOREPLACE / O_EXCL in the same function must be in the same `write_mu` scope") is a possible follow-up — see §5.

### Class D — Orphan-state ignorance

**Description.** Code takes a NodeId without inspecting its lifecycle state and applies side effects (rebinding `hot_by_path`, clearing `tombstones`) that are only valid for the Live state. The compiler offers no help — `BTreeSet::contains` is just a method call, easy to forget.

**Findings of this class.** Most r6/r7 findings overlap with Class A; the underlying error is "the code assumed Live." Examples: r7 [3293164722](https://github.com/HeddleCo/heddle/pull/182#discussion_r3293164722) (truncate); r7 [3293164731](https://github.com/HeddleCo/heddle/pull/182#discussion_r3293164731) (setattr chmod).

**Predicate (pre-spike).** Any function that mutates `Pending` and takes a `NodeId` must call `pending.orphans.contains(&id.0)` somewhere on the path between the lock acquisition and the first mutation; if it doesn't, the function is implicitly assuming Live.

**Predicate (post-spike).** **Greatly reduced — but not zero.** Decision C makes `NodeState` an enum, so any function that branches on lifecycle must `match` (or destructure with `if let`). Exhaustiveness checking turns "forgot to handle Orphan" into a compile error *inside that match*. The residual failure mode is functions that don't branch on state at all — they implicitly assume Live by virtue of not consulting `state`. These are harder to catch: the type system sees an unused `state: &BTreeMap<NodeId, NodeState>` parameter, not a missing branch.

The API surface bridging the gap: code paths that take a `NodeId` from FUSE (via `INodeNo`) and operate on `Pending` without consulting `state` should be the audit target. A possible mitigation (follow-up issue, optional) is a newtype `LiveNodeId(NodeId)` that's only constructible from `state.get(id) == Some(Live)` — making "Live-only" a type-level claim. Defer that decision to the impl PR; the spec accepts the residual discipline cost.

---

## 5. Impl sub-issues to file post-spike

### 5.1 In-flight: refactor PR #182 to NodeId-keyed `Pending` + `NodeState` enum

This is **not** a separate issue. It is the continuation plan for `heddle#180` / PR #182 itself (Decision B). The work, as a sequence of commits on `task/180-mount-fuse-implement-write-side-ops-crea`:

1. Introduce `NodeState` enum + `state: BTreeMap<NodeId, NodeState>`. Migrate `orphans` and `open_handles` callers in lockstep. Drop the old fields.
2. Re-key `warm` from `PathBuf` to `NodeId`. Drop `orphan_warm`. Update `flush_node` promotion, `apply_pending_to_tree`, `read`/`attrs`/`write`/`apply_truncate` orphan-branches, and the unlink/rename-over orphan-warm migrations.
3. Address the r9 finding ([3293307302](https://github.com/HeddleCo/heddle/pull/182#discussion_r3293307302)) at the directory-op site: don't drop `hot[node_id]` on unlink-of-open. Under the new shape this is a single-line change (remove the `pending.hot.remove(&node_id);` call at core.rs:1335 from the orphaning branch; keep it for the non-orphaning Live→Released path T2).
4. Rewrite the r8-era tests that asserted the `orphan_warm` migration (they tested behavior that no longer exists — under the new shape there's no migration step to verify). Replace with tests of the new invariant: "bytes in `warm[node_id]` survive the Live → Orphan transition unchanged."
5. Update doc comments on `Pending` fields (core.rs:374-436) to match the new shape; the existing comments thoroughly document the dual representation and would mislead readers post-collapse.

Post-merge, this doc (`docs/design/mount-posix-semantics.md`) is the surviving reference; the inline `Pending` comments can stay terse.

### 5.2 Possible follow-up: static-check for pending-write sites (defer; optional)

**Premise.** Under Decision A + C, the bug-class predicates simplify: Class A is gone, Class D is enforced by `match`-exhaustiveness for any code that branches on `state`. The remaining risk surface is code that mutates `hot_by_path[path]` / `warm[id]` / `hot[id]` *without* consulting `state` — implicitly assuming Live.

**Proposal.** A repo-grep script (or clippy lint) that flags direct `pending.hot_by_path.insert/remove` / `pending.warm.insert/remove` / `pending.hot.insert/remove` outside a small set of canonical mutation sites in `core.rs`. The set is enumerated in the lint config.

**Status.** Defer. The type-system gains from the refactor in §5.1 may make this redundant. File as a follow-up only if a class-D-shaped bug recurs after #182 lands.

### 5.3 Possible follow-up: property-test for FUSE callback sequencing (defer; xhigh effort)

**Premise.** The FSM in §1 is small enough to model in `proptest`. A property test that generates arbitrary sequences of FUSE callbacks (lookup / open / write / unlink / rename / release / forget), applies them to a mount, and asserts that the resulting `state` is reachable from `Live` via documented transitions would catch FSM-divergence bugs that example-based tests miss.

**Status.** Defer to follow-up. The effort is xhigh (test infrastructure for arbitrary FUSE-callback streams is non-trivial); the leverage is real but secondary to the #182 refactor landing first.

---

## References

* HeddleCo/heddle#180 — the impl issue under audit.
* HeddleCo/heddle#199 — this spike's tracking issue.
* HeddleCo/heddle PR #182 — `task/180-mount-fuse-implement-write-side-ops-crea`. Line citations in this document reference the head commit of that branch unless otherwise noted.
* `crates/mount/src/core.rs` — the file under audit.
* `crates/mount/src/shell.rs` — `PlatformShell` trait, including the `flush` / `release` lifetime distinction at lines 141-154.
* `crates/mount/src/fuse.rs` — `FuseShell` callbacks on the PR #182 branch.
* Codex review threads on PR #182, linked inline in §4.
