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
**~97× faster than packed-refs at 100k refs** (515 µs vs 50 ms). It also
shrinks the on-disk payload by 34 %.

The wins are real but **invisible at heddle's current target scale**.
A 1 000-ref repo loses ~0.5 ms of cold-load cost; nobody notices.
Meanwhile, shipping the format requires an on-disk migration, a real
`RefBackend` integration (CAS, transactions, summary index, packing),
and committing to a binary format we'd then need to keep stable.

**Recommendation: defer.** Revisit if (a) a user reports
slowness on a >10 k-ref repo, or (b) we change the on-disk layout for
another reason and can bundle the migration.

> **Round-2 correction (Codex P1).** The original spike write-up reported
> an **18–34× append+persist speedup**; that bench mutated the in-memory
> model and serialized it but never actually wrote to disk. With the bench
> corrected to use `objects::fs_atomic::write_file_atomic` (the production
> packed-refs write path: temp file + `fsync` + atomic rename + parent
> directory `fsync`), the speedup is **2.4–6.2×** instead — still real,
> meaningfully smaller. The "defer past 0.3" recommendation stands; if
> anything the case for shipping in 0.3 weakens, because one of the
> larger pro-ship numbers turned out to be a measurement artifact.
> Cold-lookup wins (which were measured correctly) are unaffected.

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
  (`uname -srm`: `Linux 6.8.0-71-generic x86_64`). The tempdir backing
  `append_one_persist` lives on the same regular ext4 mount as the
  workspace (verified `/tmp` is not tmpfs on this host), so `fsync`
  measurements reflect real disk I/O. Numbers are representative of a
  workstation, not CI.
- Persist semantics: `append_one_persist` calls
  `objects::fs_atomic::write_file_atomic` for both backends, matching
  production `PackedRefs::save` (write temp file → `fsync` → atomic
  rename → `fsync` parent directory). All other bench metrics are
  in-memory or read-only.
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

### Append one ref + persist (rewrite full file with fsync + atomic rename)

| Refs | packed-refs | reftable | speedup |
|---:|---:|---:|---:|
| 10 000 | 15.1 ms | 6.19 ms | **2.4×** |
| 50 000 | 58.2 ms | 12.3 ms | **4.7×** |
| 100 000 | 143 ms | 23.0 ms | **6.2×** |

The packed-refs append cost at 100 k is still dominated by base32-encoding
every `ChangeId` for the rewrite — **~120 ms of CPU plus the disk I/O on
every `set_thread`**. Reftable serializes nearly instantly (a memcpy of
raw bytes), so its cost is dominated by `fsync` of the 4.3 MB payload.

This benchmark goes through `objects::fs_atomic::write_file_atomic` for
both backends — the same call `PackedRefs::save` uses in production
(`crates/refs/src/refs/packed_refs.rs:42`). Each iteration writes the
serialized bytes to a temp file in the parent directory, `fsync`s the
file, atomically `rename`s into place, then `fsync`s the parent
directory. **Persist cost is included; this is not a CPU-only number.**

> **Round-2 history.** The first spike write-up reported 18–34× here; the
> bench at that point only called `to_text()` / `to_bytes()` under
> `black_box` and never wrote to disk. Codex flagged this as P1 (cid
> `3253671446`) and we re-ran. The corrected speedup is materially
> smaller but reftable still wins, because for packed-refs at the larger
> scales serialization CPU dominates the I/O.

Caveat: a real reftable would append a new table rather than rewriting,
collapsing this to near-zero per write at the cost of periodic
compaction. The 6.2× we measured is the worst-case-of-reftable vs
worst-case-of-packed; the production reftable design is much better.

## Trade-off analysis

### Arguments for shipping in 0.3

- Cold single-ref lookup is genuinely transformative at scale
  (50 ms → 0.5 ms at 100 k refs). Every short-lived CLI invocation
  benefits.
- 34 % smaller on disk, which matters more for sync/replication than
  for local repos.
- Append cost on rewriting is 2.4–6.2× lower (with `fsync` + atomic
  rename included on both sides), which compounds across any batch
  operation.
- The format is small (~250 lines of model code) and the bench gives
  us confidence it does what we want.

### Arguments to defer

1. **Heddle's users don't have 10 k+ refs.** The dramatic wins are at
   50 k and 100 k. At 1 000 refs (a plausible upper bound for the
   current user base), cold-lookup is 0.4 ms vs ~6 µs — a 0.4 ms saving
   the user will never notice. We'd be solving a problem nobody has yet.
2. **Format migration burden.** Existing `.heddle` repos have
   `refs/packed-refs` text files. Either we add migrate-on-read +
   write-new-format, or we ship a `refs migrate` command, or we
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

## Prototype limitations

The reftable model in `crates/refs/src/refs/reftable_model.rs` is
exploration code, not production-ready. Codex's round-1 review of PR
[#67](https://github.com/HeddleCo/heddle/pull/67) surfaced four real
issues we did **not** fix in this PR because spikes shouldn't bear the
cost of full hardening — but any productionization PR **must** address
them before wiring `ReftableModel` through `RefManager`.

Each finding is cited by its Codex comment ID (cid) for traceability.

1. **Overlong-name `u16` wrap on encode** (cid `3253671447`,
   `reftable_model.rs:256`). `encode_block` casts `name_bytes.len()` to
   `u16` without validating length. Any ref name ≥ 65 536 bytes silently
   wraps and produces a malformed stream that decodes with the following
   `ChangeId` misaligned. `validate_ref_name` does not enforce a length
   ceiling. **Fix before productionizing:** validate name length on
   encode (return a `ReftableError::NameTooLong` or similar), and
   tighten `validate_ref_name`. Add round-trip tests for the boundary.

2. **Marker-offset underflow on malformed input** (cid `3253671448`,
   `reftable_model.rs:320`). `block_byte_len_from_index` computes
   `after_last - block_start` with unchecked `usize` subtraction. If a
   corrupted thread index points to a record that ends before
   `block_start`, this underflows — debug panic, release wrap —
   breaking the function's `Result`-based contract for "bad on-disk
   data". **Fix before productionizing:** use `checked_sub` and surface
   a `ReftableError::Truncated` (or `BadIndex`) on underflow. Property
   tests with malformed index entries.

3. **Sorted-decode hazard for binary-search lookups** (cid `3253671450`,
   `reftable_model.rs:172`). `from_bytes` accepts thread/marker records
   in whatever order they appear in the on-disk block, but `get_thread`
   / `get_marker` rely on `Vec::binary_search_by`, which is undefined on
   unsorted slices. For structurally valid but unsorted files, lookups
   become unspecified — `None` for present keys, wrong values, or
   correct results, depending on input. **Fix before productionizing:**
   either reject unsorted decode with a `ReftableError::Unsorted`, or
   normalize by sorting before returning the model. The former matches
   the wire-stability story (the writer always sorts) better than the
   latter.

4. **Index tables not validated on full decode** (cid `3253671451`,
   `reftable_model.rs:176`). `from_bytes` linearly decodes both blocks
   and never reads either offset index, so corrupted index tables
   deserialize successfully. This leaves the two read paths inconsistent
   — `from_bytes`-then-`get_*` succeeds while `lookup_*_in_bytes` on
   the same file may fail or return different refs — and masks
   corruption that should surface as a format error. **Fix before
   productionizing:** walk each index entry during full decode and
   verify it points at the same record the linear scan produces, or
   stop maintaining the linear path entirely and route every read
   through the indexed lookup.

These are **not** bench-level concerns — the bench data is unaffected
because it round-trips well-formed payloads only. They are
spike-quality artifacts of "format design done in one pass, no
production hardening". A productionization PR should bring this
crate-level surface area to the same robustness as
`crates/refs/src/refs/packed_model.rs`.

## Decision

**Defer past 0.3.** Keep the spike code in tree as documentation +
red-tested model. File a follow-up impl issue if any of the triggers
above fires. Until then, packed-refs is fine for heddle's target user.

The round-2 bench correction (real `fsync` + atomic rename on persist)
narrowed the append+persist speedup from 18–34× to 2.4–6.2×. That
weakens, not strengthens, the case for shipping in 0.3 — the
cold-lookup wins are still the headline, and those numbers were
measured correctly. The defer call holds.

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
