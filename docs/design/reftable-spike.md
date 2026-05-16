# Reftable spike — decision doc

**Status:** Spike complete. **Recommendation: defer past 0.3.**
**Issue:** [HeddleCo/heddle#21](https://github.com/HeddleCo/heddle/issues/21)
**Prototype:** `crates/refs/src/refs/reftable_model.rs`
**Bench:** `crates/refs/benches/reftable_vs_packed.rs`

## Question

Should heddle replace the current `packed-refs` text format with a
reftable-style binary format for the 0.3 release?

## TL;DR

A reftable-style binary format wins on every ref-count-scaling metric we
care about — most dramatically on cold single-ref lookup, where it is
**~97× faster than packed-refs at 100k refs** (514 µs vs 50 ms). It also
shrinks the on-disk payload by 34 %.

The wins are real but **invisible at heddle's current target scale**.
A 1 000-ref repo loses ~0.5 ms of cold-load cost; nobody notices.
Meanwhile, shipping the format requires an on-disk migration, a real
`RefBackend` integration (CAS, transactions, summary index, packing),
and committing to a binary format we'd then need to keep stable.

**Recommendation: defer.** Revisit if (a) a user reports
slowness on a >10 k-ref repo, or (b) we change the on-disk layout for
another reason and can bundle the migration.

## What's in the prototype

`crates/refs/src/refs/reftable_model.rs` defines `ReftableModel`,
parallel in shape to the existing `PackedRefsModel`
(`crates/refs/src/refs/packed_model.rs`). Both hold sorted
`(name, ChangeId)` records for threads and markers and serialize to a
single file.

The differences are entirely in the on-disk encoding:

| Aspect | `PackedRefsModel` | `ReftableModel` (prototype) |
|---|---|---|
| Bytes | Line-oriented UTF-8 text | Header + offset index + variable-length records + footer, little-endian |
| Record | `<base32 ChangeId> refs/<kind>/<name>\n` | `[name_len u16][name][id 16 bytes]` |
| ID encoding | base32 with `hd-` prefix (≈ 29 chars / ID) | Raw 16 bytes |
| Lookup cost (cold) | Full parse, then `HashMap` get | `O(log N)` binary search via offset index |
| Lookup cost (warm) | `HashMap` get — `O(1)` | Binary search over sorted `Vec` — `O(log N)` |
| File header | `# packed-refs with: peeled fully-peeled sorted` | 8-byte magic + thread/marker counts |
| Self-describing | Header comment only | Magic at head and footer |

The encoding lives in `to_bytes` / `from_bytes` and is documented inline
at the top of `reftable_model.rs`. Cold-lookup helpers
(`lookup_thread_in_bytes`, `lookup_marker_in_bytes`) binary-search
against a byte slice without materialising the rest of the model — the
"fetch HEAD ref name" path.

### What the prototype is NOT

This spike intentionally stops at the model + format layer. None of the
following is implemented:

- **No `RefBackend` impl.** The prototype is not wired through
  `RefManager` (`crates/refs/src/refs/refs_manager.rs`). The current
  `get_thread` hot path —
  `PackedRefs::load(&self.packed_refs_path())?.get_thread(name)`
  (`refs_manager.rs:116`) — is unchanged.
- **No file storage.** `to_bytes`/`from_bytes` only; no atomic write,
  no lock, no temp-file rename.
- **No CAS, no transactions, no summary-index integration.**
- **No append-only / multi-table stacking** as in Git's real reftable
  spec. We rewrite the whole file on every mutation, same as the
  current `PackedRefs::save`. This makes the bench an apples-to-apples
  comparison of the rewrite strategy, but it is not how a production
  reftable would behave.
- **No prefix compression, varint encoding, or restart points.** A
  full Git reftable would shave another ~30–50 % off file size with
  prefix compression. We chose simplicity over that for the spike.
- **No reflog records.**

Call it "reftable-lite" — enough format to bench the lookup-path wins
that motivate reftable, not the full spec.

## Bench setup

`cargo bench -p heddle-refs --bench reftable_vs_packed -- --quick --noplot`

- Sizes: 10 000, 50 000, 100 000 threads. Zero markers (per-section code
  paths are identical; a second axis would only obscure the comparison).
- Names mimic real branch shapes:
  `feature/branch-NNNNNN`, `topic/…`, `release/…`, `user/alice/…`,
  `user/bob/…`. See `name_for` in the bench file.
- IDs are deterministic per index — same names + IDs every run.
- Host: the local development host the prototype was written on
  (`uname -srm`: `Linux 6.8.0-71-generic x86_64`). Numbers are
  representative of a workstation, not CI.
- Mode: criterion `--quick` (3 sample windows × 10 iterations). Good
  enough to read the dynamic range; not statistically tight enough to
  call <10 % differences.

## Results

### File size

| Refs | packed-refs | reftable | reftable / packed |
|---:|---:|---:|---:|
| 10 000 | 654 KB | 434 KB | 0.66 |
| 50 000 | 3.27 MB | 2.17 MB | 0.66 |
| 100 000 | 6.54 MB | 4.34 MB | 0.66 |

Reftable is **34 % smaller**. The win comes entirely from encoding the
16-byte `ChangeId` raw rather than as a 26-character base32 string with
`hd-` prefix.

### Cold load (open file, parse to in-memory model)

| Refs | packed-refs | reftable | speedup |
|---:|---:|---:|---:|
| 10 000 | 4.23 ms | 699 µs | **6.0×** |
| 50 000 | 25.2 ms | 3.73 ms | **6.7×** |
| 100 000 | 57.3 ms | 6.82 ms | **8.4×** |

Reftable cold-load throughput holds ~600 MiB/s across sizes; packed-refs
falls from 148 → 109 MiB/s because the per-line UTF-8 / base32 parse
cost dominates at scale.

### Cold single-ref lookup (open file fresh, find one ref by name)

| Refs | packed-refs | reftable | speedup |
|---:|---:|---:|---:|
| 10 000 | 4.01 ms | 58 µs | **69×** |
| 50 000 | 21.3 ms | 245 µs | **87×** |
| 100 000 | 50.0 ms | 514 µs | **97×** |

This is reftable's headline number. The cold-lookup path is what a
short-lived CLI invocation pays — `heddle status` resolving HEAD,
sync/fetch checking a single thread. Reftable scales as `O(log N)`
seeks; packed-refs always pays the full parse + hashmap-build cost
just to answer one question.

### Warm lookup (model already loaded, 1000 random lookups)

| Refs | packed-refs | reftable | reftable / packed |
|---:|---:|---:|---:|
| 10 000 | 55.8 µs | 190 µs | 3.4× slower |
| 50 000 | 54.3 µs | 273 µs | 5.0× slower |
| 100 000 | 83.9 µs | 313 µs | 3.7× slower |

**Reftable loses warm lookup.** `HashMap` is `O(1)`; binary search over
a sorted `Vec` is `O(log N)`. This is the price for not building a
hashmap. At microsecond scale across 1 000 lookups, neither is a
user-visible bottleneck — but if a process needs to do tens of thousands
of resolutions in tight succession, packed-refs (once loaded) is the
faster shape.

### List all ref names

| Refs | packed-refs | reftable | speedup |
|---:|---:|---:|---:|
| 10 000 | 453 µs | 415 µs | 1.1× |
| 50 000 | 5.24 ms | 2.29 ms | **2.3×** |
| 100 000 | 15.2 ms | 5.15 ms | **3.0×** |

Both clone names into a `Vec<String>`. Reftable wins because the
sorted-`Vec` iteration is more cache-friendly than walking a `HashMap`.

### Append one ref + persist (rewrite full file)

| Refs | packed-refs | reftable | speedup |
|---:|---:|---:|---:|
| 10 000 | 8.56 ms | 480 µs | **18×** |
| 50 000 | 53.2 ms | 2.11 ms | **25×** |
| 100 000 | 164 ms | 4.88 ms | **34×** |

The packed-refs append cost is dominated by base32-encoding every
`ChangeId` for the rewrite. At 100 k refs this is **164 ms of CPU on
every `set_thread`**. Reftable just memcpys raw bytes.

Caveat: a real reftable would append a new table rather than rewriting,
collapsing this to near-zero per write at the cost of periodic
compaction. The 34× we measured is the worst-case-of-reftable vs
worst-case-of-packed; the production reftable design is much better.

## Trade-off analysis

### Arguments for shipping in 0.3

- Cold single-ref lookup is genuinely transformative at scale
  (50 ms → 0.5 ms at 100 k refs). Every short-lived CLI invocation
  benefits.
- 34 % smaller on disk, which matters more for sync/replication than
  for local repos.
- Append cost on rewriting is 18–34× lower, which compounds across
  any batch operation.
- The format is small (~250 lines of model code) and the bench gives
  us confidence it does what we want.

### Arguments to defer

1. **Heddle's users don't have 10 k+ refs.** The dramatic wins are at
   50 k and 100 k. At 1 000 refs (a plausible upper bound for the
   current user base), cold-lookup is 0.4 ms vs ~6 µs — a 0.4 ms saving
   the user will never notice. We'd be solving a problem nobody has yet.
2. **Format migration burden.** Existing `.heddle` repos have
   `refs/packed-refs` text files. Either we add migrate-on-read +
   write-new-format, or we ship a `heddle refs migrate` command, or we
   read-both / write-old until 0.4. None of those are *hard*, but they
   add surface area and test cases.
3. **Format-stability commitment.** Once we ship a binary format on
   disk, "REFT01" lives forever in users' repos. We picked the layout
   in a one-pass spike — we should pressure-test it before committing.
4. **Spike is not a backend.** The "What the prototype is NOT"
   list above is roughly two weeks of follow-up work: wire through
   `RefManager`, atomic writes, CAS, transactions, summary-index
   integration, append-only stacking, packing, migration. That's a
   full impl issue, not a side change to a release.
5. **Warm lookup regression.** Packed-refs's `HashMap` is faster
   per-op once loaded. For long-lived processes (`heddled` mount
   daemon, server), the warm shape may be the dominant cost; we'd
   need to either build a hashmap on top of the loaded reftable or
   accept the regression.
6. **Coverage gate.** `refs=80` in `.github/workflows/rust-tests.yml`.
   New backend code needs ≥ 80 % coverage — the spike's
   `reftable_tests` already covers the model, but a real
   `ReftableBackend` would need a much larger test corpus.

### Triggers that should change this decision

- A user reports `heddle status` or `heddle sync` being slow on a
  >10 k-ref repo. (Not hypothetical: long-lived monorepos with one
  thread per CI run / PR easily hit this.)
- We touch the on-disk ref layout for another reason (e.g., reflog
  support, atomic multi-ref transactions across remotes). At that
  point bundling reftable amortizes the migration cost.
- A downstream Heddle deployment (Weft hosted product) needs to
  serve thousands of refs per repo per request and is bottlenecked
  on packed-refs parse cost. The server already has `PgRefBackend`
  as an alternative, but if Postgres isn't desirable for some
  deployments, reftable is the next escape hatch.

## Decision

**Defer past 0.3.** Keep the spike code in tree as documentation +
red-tested model. File a follow-up impl issue if any of the triggers
above fires. Until then, packed-refs is fine for heddle's target user.

## Pointers

- Prototype model: `crates/refs/src/refs/reftable_model.rs`
- Red-commit tests (14): `crates/refs/src/refs/reftable_tests.rs`
- Bench: `crates/refs/benches/reftable_vs_packed.rs`
- Current packed-refs cold-load hot path:
  `crates/refs/src/refs/refs_manager.rs:116` (and `:168` for markers)
- `ChangeId` raw shape: `crates/objects/src/object/hash.rs:99`
  (`[u8; 16]`)
- Existing packed-refs format: `crates/refs/src/refs/packed_model.rs`
- Coverage gate location: `.github/workflows/rust-tests.yml`
  (`refs=80`)
