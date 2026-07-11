# L8 — Pack install two-phase journal (design)

**Status:** **Designed / not implemented** — optional harden for GAP_MAP **L8**.  
**Product priority:** **P2** unless unpaired packs become a measured disk-leak
or support incident. Correctness today does **not** require this journal:
`reload_packs` ignores unpaired `.pack` files.

**Not a claim:** this document does not ship a journal; Wave 5 tip correctness
remains green without it.

Cross-check: `GAP_MAP.md` L7/L8, `crates/objects/src/store/fs/fs_pack.rs`
(`install_pack_files_streaming`), `publish_file_durable` in `fs_atomic.rs`.

---

## Problem statement

`install_pack_files_streaming` publishes pack then index as **two** durable
renames:

1. `publish_file_durable(src_pack → packs/<hash>.pack)`
2. `publish_file_durable(src_idx  → packs/<hash>.idx)`

Each publish fsyncs file data and parent dirent. A crash **between** the two
leaves:

- a content-addressed `<hash>.pack` on disk, and
- **no** matching `<hash>.idx`.

`reload_packs` only loads packs that have both files → **no silent corruption**.
The residual is **disk leak / cleanup**, not wrong reads.

Symmetric risk if order were reversed (index without pack) would also be
ignored, but current order is pack-then-index.

---

## Goals

| Goal | Notes |
|------|--------|
| G1 Crash between pack/index publish does not leave permanent orphan packs without recovery path | Cleanup or complete |
| G2 No weakening of publish durability (still fsync + atomic rename + parent fsync) | Must not skip L6/L7 guarantees |
| G3 Constant-memory streaming install remains | No full pack in RAM |
| G4 Simple recovery: restart/gc can finish or delete | Idempotent |
| G5 Optional: never leave unpaired pack visible longer than one recovery pass | Best-effort |

## Non-goals

- Distributed multi-node pack install.
- Replacing content-addressed pack naming.
- Making unpaired packs loadable (that would be wrong).
- Rewriting `PackBuilder` in-memory install path beyond the same journal API.

---

## Design options

### Option A — Intent journal (recommended)

Write a small durable intent file **before** either publish, then complete or
abort.

**Layout** (under store root, e.g. `.heddle/objects/` or `packs/`):

```text
packs/.install-intent/<install_id>.json   # or msgpack
```

**Intent record (v1):**

```json
{
  "version": 1,
  "install_id": "<ulid-or-uuid>",
  "pack_name": "<blake3-hex>",
  "src_pack": "<optional abs path or empty if already staged>",
  "src_index": "<optional>",
  "dst_pack": "packs/<name>.pack",
  "dst_index": "packs/<name>.idx",
  "phase": "prepared | pack_published | completed | aborted",
  "created_unix": 0
}
```

**Protocol:**

1. Stream-hash pack → `pack_name` (unchanged).
2. `create_dir_all_durable(intent_dir)`.
3. Write intent with `phase=prepared` via `write_file_atomic` (or
   `publish_file_durable` into intent path).
4. `publish_file_durable` pack → `phase=pack_published` (update intent atomically).
5. `publish_file_durable` index → `phase=completed`, then delete intent
   (best-effort unlink + parent fsync).
6. `reload_packs` as today.

**Crash recovery** (`recover_pack_install_intents` on store open or GC):

| Intent phase | Disk state | Action |
|--------------|------------|--------|
| `prepared` | neither or partial | Delete intent; delete any half-published dst if present |
| `pack_published` | pack present, index missing | Prefer **delete pack + intent** (safe leak cleanup) **or** re-copy index if `src_index` still exists and hashes match |
| `completed` | both present | Delete stale intent only |
| missing intent, unpaired pack | pack without idx | Same as today: ignore for load; optional GC deletes orphan packs older than N |

**Why delete unpaired pack rather than complete index from src?**  
After process death, `src_index` may be gone (temp spool). Completing requires
durable staging of both sources until commit. If we keep sources until
`completed`, recovery can finish install; that is Option A+.

### Option A+ — Hold sources until completed

Staging directory owned by install:

```text
packs/.staging/<install_id>/{pack,idx}
```

1. Publish/move sources into staging (durable).
2. Intent `prepared` with staging paths.
3. Publish pack from staging → `pack_published`.
4. Publish index from staging → `completed`.
5. Remove staging + intent.

Recovery can always finish or abort with full information. Slightly more I/O
and disk.

### Option B — Single directory rename (publish pack+idx as unit)

Build into `packs/.tmp/<name>/{name.pack,name.idx}`, fsync both + dir, then
rename the **directory** into place. Many filesystems make directory rename
atomic w.r.t. readers under the same parent.

**Pros:** no journal file.  
**Cons:** current layout is flat `packs/*.pack` + `packs/*.idx`; readers and
pack manager assume flat names. Would need layout migration or hardlink dance.
Higher blast radius.

### Option C — Index first, then pack

Does not remove the window; only swaps which orphan type appears. **Rejected**
as a fix (still L8-shaped).

### Option D — Accept leak + periodic GC (status quo + cleaner)

Document unpaired pack GC: any `*.pack` without `*.idx` older than threshold
deleted during `heddle maintenance` / `gc`.

**Pros:** tiny change, no journal.  
**Cons:** does not shrink the crash window; only bounds leak lifetime.

**Recommendation:** ship **Option D** as immediate low-risk cleanup when product
cares about disk; implement **Option A or A+** if install interruptions become
common (large imports, flaky disks).

---

## Recommended implementation plan (when elevated)

### Phase 0 — Docs + metrics (this document)

- [x] Problem, non-goals, options, recovery table
- [ ] Optional: counter/log when `reload_packs` skips unpaired pack (observability)

### Phase 1 — Orphan pack GC (Option D)

- [x] `list_unpaired_pack_files(packs_dir) -> Vec<PathBuf>` (crate helper)
- [x] `prune_unpaired_pack_files` + `FsStore::prune_unpaired_packs` (library API)
- [x] Unit tests: pack without idx deleted; pack+idx kept; missing dir ok
- [ ] Wire to `heddle maintenance` / `gc` CLI human output (optional product surface)
- [x] GAP_MAP L8 notes foundation helpers

### Phase 2 — Intent journal (Option A)

- [ ] Intent schema + atomic write helpers
- [ ] Integrate into `install_pack_files_streaming` only (streaming path first)
- [ ] `recover_pack_install_intents` on `ObjectStore` open or first pack reload
- [ ] Fault-injection tests: crash after pack publish, after intent prepare
- [ ] In-memory `install_pack_files` (bytes) either shares journal or stays
      dual-write with same ordering docs

### Phase 3 — Optional A+ staging

- [ ] Only if Phase 2 recovery often cannot complete for lack of sources

---

## API sketch (Phase 2)

```rust
// crates/objects — illustrative, not shipped
pub struct PackInstallIntent { /* version, pack_name, paths, phase */ }

pub fn prepare_pack_install_intent(...) -> Result<PackInstallIntent>;
pub fn install_pack_files_streaming_journaled(...) -> Result<()>;
pub fn recover_pack_install_intents(store_root: &Path) -> Result<RecoverReport>;
```

CLI: no new user command required if recovery runs on open/gc. Optional
`heddle maintenance` line: `pruned N unpaired packs`.

---

## Testing strategy

| Layer | Cases |
|-------|--------|
| Unit | Intent parse/roundtrip; phase transitions; unpaired listing |
| Fault | Simulate crash by stopping after pack publish (temp hook / stop flag in tests) |
| Property | After any prefix of protocol steps + recovery, store either has both files or neither for that name |
| Perf | Journal adds ≤2 small atomic writes; measure import path only if Phase 2 lands |

---

## Interaction with L6/L7

| Layer | Role |
|-------|------|
| L6 | Dir creates for intent/staging use `create_dir_all_durable` |
| L7 | Staged builder output already fsynced before publish |
| L8 journal | Orders **two publishes** and recovers orphans |

Do not drop parent fsync on publish to “optimize” journaled install.

---

## Decision log

| Date | Decision |
|------|----------|
| 2026-07-11 | L8 remains **acceptable residual** for tip correctness |
| 2026-07-11 | Design recorded; default product path stays pack-then-index without journal |
| 2026-07-11 | Prefer Option D (GC) before Option A (journal) when implementing |
| 2026-07-11 | Option B flat-layout directory rename deferred (migration cost) |

---

## Closure criteria

L8 may be marked **mitigated** (not necessarily “eliminated”) when:

1. Phase 1 GC is shipped **or** Phase 2 journal is shipped, and  
2. GAP_MAP L8 text updated honestly, and  
3. Tests cover recovery/GC for unpaired packs.

Until then: status remains **Designed / optional residual**.
