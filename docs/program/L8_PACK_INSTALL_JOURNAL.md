# L8 — Pack install two-phase journal (A+)

**Status:** **Shipped on program tip (A+ journal + Option D backstop)**  
**Module:** `crates/objects/src/store/fs/pack_install_journal.rs`  
**Call site:** `FsStore::install_pack_files_streaming` → `install_pack_files_journaled`  
**Recovery:** `FsStore::reload_packs` runs `recover_pack_install_intents` then
`prune_unpaired_pack_files`.

**Not a claim:** byte-array `install_pack_files` still dual-writes without journal
(smaller in-memory path); can adopt the same API later.

Cross-check: `GAP_MAP.md` L7/L8, `fs_pack.rs`, `publish_file_durable`.

---

## On-disk layout (under `.heddle/packs/`)

```text
packs/
  <blake3-hex>.pack          # final content-addressed pack
  <blake3-hex>.idx           # final index
  .staging/<install_id>/
    pack                     # staged pack bytes
    idx                      # staged index bytes
  .install-intent/
    <install_id>.json        # durable intent (atomic write)
```

`install_id` = `{unix_secs:016x}-{rand_u64:016x}` (unique under concurrency).

---

## Intent schema (v1)

```json
{
  "version": 1,
  "install_id": "…",
  "pack_name": "<blake3 hex>",
  "staging_pack": "/abs/…/packs/.staging/<id>/pack",
  "staging_idx": "/abs/…/packs/.staging/<id>/idx",
  "dst_pack": "/abs/…/packs/<name>.pack",
  "dst_idx": "/abs/…/packs/<name>.idx",
  "phase": "prepared | pack_published | completed",
  "created_unix": 0
}
```

Phases: `PackInstallPhase::{Prepared, PackPublished, Completed}`.

---

## Install protocol (happy path)

1. Stream-hash source pack → `pack_name`.
2. If final pack+idx both exist → drop sources, return (idempotent).
3. If orphan final pack without idx → delete orphan.
4. `create_dir_all_durable(.staging/<id>)`.
5. `publish_file_durable(src_pack → staging/pack)`.
6. `publish_file_durable(src_idx → staging/idx)`.
7. Write intent `phase=prepared` (`write_file_atomic`).
8. `publish_file_durable(staging/pack → final.pack)`.
9. Intent `phase=pack_published`.
10. `publish_file_durable(staging/idx → final.idx)`.
11. Intent `phase=completed`.
12. Remove staging dir + intent file.
13. Caller clears caches + `reload_packs` (which also recovers peers).

Every publish uses existing L6/L7 durability (`publish_file_durable`).

---

## Recovery table

| Intent phase | Disk | Action |
|--------------|------|--------|
| `prepared`, no finals | staging only | **Abort** — drop staging + intent |
| `prepared`, pack final + staged idx | crash after pack publish before phase flip | **Complete** index publish |
| `prepared`, both finals | rare | **Cleanup** staging + intent |
| `pack_published`, staged idx present | pack final | **Complete** index publish |
| `pack_published`, staged idx missing | pack final only | **Abort** — delete unpaired pack + intent |
| `completed` leftover | both finals | **Cleanup** staging + intent |
| corrupt / unknown version | — | drop intent file; count error |
| no intent, unpaired `.pack` | legacy L8 | **Option D** `prune_unpaired_pack_files` on reload |

---

## Public API

| Item | Role |
|------|------|
| `install_pack_files_journaled(packs_dir, src_pack, src_idx, pack_name)` | Journaled install |
| `recover_pack_install_intents(packs_dir) -> PackInstallRecoverReport` | Crash recovery |
| `PackInstallIntent` / `PackInstallPhase` / `PackInstallRecoverReport` | Types |
| `FsStore::prune_unpaired_packs` | Option D GC |
| `FsStore::reload_packs` | recover + prune + load |

---

## Test matrix (unit)

| Test | Covers |
|------|--------|
| `journaled_install_produces_pair_and_cleans_intent` | Happy path + cleanup |
| `recover_pack_published_completes_from_staging` | Complete after pack publish |
| `recover_prepared_aborts_without_finals` | Abort clean staging |
| `recover_pack_published_without_staging_idx_aborts_orphan_pack` | Abort when cannot complete |
| `recover_prepared_with_pack_and_staging_idx_completes` | Phase-flip crash window |
| `journaled_install_idempotent_when_pair_exists` | CAS idempotency |
| unpaired pack list/prune tests | Option D backstop |

---

## Remaining / follow-ups

- [ ] Wire prune/recover summary lines into `heddle maintenance` human output (optional UX)
- [ ] Journal the in-memory `install_pack_files` dual-write path (same API)
- [ ] Hosted metrics counters for recover completed/aborted (when observability lands)
- [ ] Intent TTL sweeper for abandoned staging under abuse (quota companion)

---

## Decision log

| Date | Decision |
|------|----------|
| 2026-07-11 | A+ chosen for long-term multi-user scale |
| 2026-07-11 | **Implemented** journaled streaming install + recover on reload + Option D prune |
| 2026-07-11 | Prepared-phase recovery completes when final pack + staged idx present |
