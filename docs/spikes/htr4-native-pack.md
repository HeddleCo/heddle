# HTR4 native packed-tree spike

Date: 2026-08-27
Branch: `spike/htr4-native-pack`
Base: `0e43f845` (`spike/htr4-radical`)

## Verdict

Yes. A native structural tree pack can be smaller than Git packed while one
tree or entry remains directly addressable from the pack. The depth-16
prototype stores all 212,832 trees in **35,842,671 bytes**, including its pack
header, dictionaries, content-hash index, and ordinal-to-record-offset table.
That is **0.430x** the inherited strict Git-packed baseline (83,413,546 object
record bytes), **0.401x** a dedicated Git tree-only pack plus index
(89,381,610 bytes), **0.070x** HLR1/HDC1, and **0.020x** Git loose.

This is a pack winner, not a replacement for the hot object format. It buys
the result by reintroducing a global pack/repack lifecycle, a similarity search,
shared dictionaries, and chains which are commonly at the configured depth
limit. HLR1/HDC1 remains the simpler independent-object form for capture and
unsettled objects. The native pack is credible as a settlement/GC format.

## NPK1 prototype

### Pack-wide dictionaries

- Names receive lexicographic pack ordinals. The dictionary is front-coded in
  128-name restart blocks, so a name remains individually resolvable.
- A 32-byte target hash is interned only when it occurs at least twice. Single-
  use hashes stay inline; this avoids making history-free tiny trees worse just
  to claim interning.
- Frequent target hashes receive the shortest varints.
- Gitlinks and spoollinks remain typed inline values.

Across all eight packs the name dictionaries cost 223,947 bytes and the
194,705 repeated-target rows cost 6,230,607 bytes.

### Structural anchors and deltas

An anchor is a sorted stream of `(name ordinal, mode/type, target)` rows. A
delta is a sorted stream of remove/upsert operations against another packed
tree. Both use 128-row restart blocks; each block independently keeps the
smaller of raw or zstd level 3. A full resolve reads one anchor and applies at
most 16 deltas. A named-entry lookup walks the chain newest-to-oldest, reads
only the candidate block in each record, and stops at the first upsert/remove.

The winning aggregate has 22,061 anchors and 190,771 deltas. Of those deltas,
104,042 select a non-parent base. Chain depth is p50 15, p95 16, max 16: the
bound is real and frequently active.

### Cross-object candidate window

Each tree has a name/type shape hash and four MinHash values. The builder
collects candidates from exact-shape, MinHash, and neighboring-size buckets,
retains the most recent 64 members of each bucket, ranks the union, and exactly
encodes at most 16 candidates. Historical parent is included when it is a
valid backward record. Equal similarity scores prefer the most recent candidate,
making the sliding-window result deterministic. A reused tree ID can make the
inferred tree-parent map cyclic; such a hint is rejected because pack bases
must point backward.

### Direct pack serving and accounted index

Records are concatenated in pack order. A hash-sorted index maps each 32-byte
Heddle tree ID to a 32-bit pack ordinal; an ordinal-sorted 32-bit offset table
locates the record and its predecessor boundary. Delta records carry a backward
ordinal distance. The index includes fanout and checksum bytes and costs
8,521,792 bytes aggregate. Packs over 2 GiB would need Git-style 64-bit escape
offsets; no measured pack crosses that boundary.

The prototype resolves sampled objects by binary-searching this content-hash
index, slicing records from a single concatenated pack buffer, walking backward
base references, decoding, and validating the final semantic tree hash. It is
not wired into the production filesystem store.

## Complete-corpus storage

Git loose is the actual loose object file size at zlib level 1. `Git packed`
is the inherited harness's sum of `verify-pack` object record sizes and excludes
pack/index fixed overhead. Native totals include all of their index and shared
metadata, making `native / Git packed` the deliberately stricter comparison.

| Corpus | Trees | Raw HTR4 | v5 | Git loose | Git packed | HLR1/HDC1 | NPK1 d16 | NPK1 / Git packed |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| Heddle | 12,022 | 11,859,145 | 10,879,700 | 6,960,113 | 1,374,333 | 6,606,810 | 1,598,304 | 1.163x |
| ripgrep | 6,351 | 4,609,691 | 4,537,923 | 2,841,433 | 558,782 | 2,526,717 | 718,113 | 1.285x |
| Flask | 11,414 | 11,828,380 | 11,330,371 | 7,211,630 | 1,438,405 | 6,584,613 | 1,335,288 | 0.928x |
| Git | 167,988 | 3,301,664,847 | 2,600,180,873 | 1,769,314,788 | 78,319,496 | 493,722,880 | 29,698,182 | 0.379x |
| hierarchical monorepo | 910 | 2,096,415 | 1,552,791 | 1,059,260 | 299,367 | 745,089 | 433,913 | 1.449x |
| many tiny trees | 12,001 | 1,896,061 | 1,716,905 | 926,183 | 845,658 | 988,527 | 1,455,385 | 1.721x |
| huge fanout/history | 64 | 34,563,904 | 22,282,909 | 15,828,447 | 482,567 | 443,324 | 432,370 | 0.896x |
| deep vendored | 2,082 | 230,472 | 230,152 | 107,431 | 94,938 | 103,101 | 171,116 | 1.802x |
| **All** | **212,832** | **3,368,748,915** | **2,652,711,624** | **1,804,249,285** | **83,413,546** | **511,721,061** | **35,842,671** | **0.430x** |

NPK1 wins the strict Git baseline on Flask, Git history, huge fanout/history,
and the aggregate. It loses on Heddle, ripgrep, hierarchical monorepo, tiny
trees, and deep vendored. Against a like-for-like dedicated Git tree pack plus
index it additionally wins Heddle and ripgrep; it still loses the three
history-poor synthetic shapes.

The tiny/deep loss is structural: there is no history to delta, Heddle retains
32-byte hashes where this Git corpus uses SHA-1, and the directly-served native
index costs 40 bytes per tree plus fixed tables. HLR1/HDC1 is the better format
for those shapes.

## What closed the gap

All rows include the revised full native index described above.

| Variant | Total bytes | vs Git packed | p50/p95/max depth |
|---|---:|---:|---:|
| Interned anchors only | 241,896,576 | 2.900x | 0 / 0 / 0 |
| Parent only, depth 1 | 132,564,564 | 1.589x | 0 / 1 / 1 |
| Parent only, depth 8 | 48,119,266 | 0.577x | 3 / 8 / 8 |
| Cross-object window, depth 1 | 61,960,010 | 0.743x | 1 / 1 / 1 |
| Cross-object window, depth 4 | 44,708,429 | 0.536x | 4 / 4 / 4 |
| Cross-object window, depth 8 | 39,757,673 | 0.477x | 8 / 8 / 8 |
| **Cross-object window, depth 16** | **35,842,671** | **0.430x** | **15 / 16 / 16** |
| Cross-object depth 8, no block zstd | 41,017,749 | 0.492x | 8 / 8 / 8 |
| Byte-delta control, depth 8 | 39,518,077 | 0.474x | 6 / 8 / 8 |

The window cuts depth-1 parent storage from 1.589x to 0.743x. Increasing the
window scheme from depth 1 to 4, 8, and 16 supplies the rest. Structural depth
8 is 17.4% smaller than parent-only depth 8. It is 0.6% larger than the byte-
delta control at equal depth, so this experiment does **not** show structural
deltas intrinsically beating byte deltas. Per-block zstd saves 3.1% at depth 8;
the deeper bounded chain is what makes the structural variant the tested winner.

The byte control uses fixed/rolling 8-byte matches, copy/insert commands, and
zstd. It is a useful same-native-representation control, not a reimplementation
of Git's heavily tuned delta search. Git packed remains the authoritative byte-
delta baseline.

## Read and serve cost

Across every object, NPK1 depth 16 reads a mean 1,669 record bytes per full
chain (p95 3,909) and applies a mean 61 structural operations. Git reads a mean
13,893 compressed record bytes (p95 44,199), with depth p50 4, p95 38, max 50.
NPK1 trades more consistently deep chains for much smaller records.

On the dominant 167,988-tree Git corpus, 16 evenly selected hot samples measured:

| Operation | Bytes/records | Median |
|---|---:|---:|
| NPK1 full semantic resolve + final hash validation | 2,212 mean chain bytes; 90.1 ops | 372.4 us |
| NPK1 one named entry, dictionary resident | 907 mean record bytes; 9.9 records | 32.0 us |
| Git `cat-file --batch`, raw inflated tree | 18,144 mean output bytes | 12.9 us |

The Git timing is a warm persistent subprocess and returns raw Git bytes; it
does not parse a Heddle tree or validate the Heddle semantic hash. Treat it as a
lower bound, not an apples-to-apples codec benchmark. NPK1 full resolve is 29x
slower on that sample. Direct NPK1 entry lookup is 2.5x that Git lower bound,
but avoids inflating and parsing the whole tree; its byte count excludes cold
reads of the shared dictionaries.

The pathological 10,000-entry history makes the distinction clear: full NPK1
resolve is 7.66 ms, while one entry is 0.638 ms and about 1,149 record bytes.
The pack is directly servable, but callers that repeatedly materialize whole
large trees should cache resolved anchors/trees.

The per-corpus measurements below use 16 evenly selected objects each. Native
times include the content-hash index lookup; dictionaries and the concatenated
pack buffer are memory-resident. The last column is the warm Git lower bound
described above.

| Corpus | NPK1 build | Full tree median | Entry median | Entry record bytes / records | Git tree median |
|---|---:|---:|---:|---:|---:|
| Heddle | 0.726 s | 11.7 us | 1.88 us | 220 / 5.2 | 6.08 us |
| ripgrep | 0.311 s | 6.67 us | 1.56 us | 210 / 8.2 | 6.68 us |
| Flask | 0.700 s | 13.1 us | 1.92 us | 169 / 4.4 | 6.16 us |
| Git | 48.916 s | 372.4 us | 32.0 us | 907 / 9.9 | 12.9 us |
| hierarchical monorepo | 0.077 s | 49.2 us | 7.31 us | 196 / 3.2 | 6.59 us |
| many tiny trees | 0.261 s | 0.406 us | 0.291 us | 43 / 1.0 | 8.21 us |
| huge fanout/history | 0.222 s | 7.66 ms | 0.638 ms | 1,149 / 14.8 | 0.156 ms |
| deep vendored | 0.058 s | 0.373 us | 0.261 us | 41 / 1.0 | 7.37 us |

## Build and lifecycle cost

The winning pack plans/builds in **51.3 s** after corpus ingestion. The full
multi-variant exploration takes 79.5 s, of which the byte-delta controls take
28.2 s. The exhaustive pack harness, including Git accounting, decoding all
trees, HLR1/HDC1, read measurements, and native variants, takes 5m07s and peaks
at 13,874,972 KiB RSS. Git packs were pre-existing, so this run does not supply
an apples-to-apples Git repack time. The prototype is intentionally in-memory;
a production builder must stream sketches/dictionaries and cap memory.

This reintroduces the lifecycle HLR1/HDC1 avoided:

- captures should write independent hot objects first;
- settlement must build dictionaries, choose bases, and atomically publish a
  pack plus index;
- append-only additions can tolerate suboptimal compression, but optimal
  dictionaries/base choices require repack;
- deletion and retention must preserve every live base until GC rewrites its
  dependants;
- dictionary/index corruption affects the pack, not one object, so checksums
  and atomic generation swaps are mandatory;
- serve caches are important because full resolution commonly walks 16 records.

The recommendation is therefore **HLR1/HDC1 hot, NPK1 settled**, with adaptive
fallback to HLR1/HDC1 for history-poor/tiny packs. It is worth pursuing as a
pack/GC layer because the aggregate saving is 57.0% versus Git packed and 93.0%
versus HLR1/HDC1. It is not worth replacing the simple capture-time object path.

## Reproduction

Use the mirrors and synthetic repositories recorded by the predecessor spike:

```bash
HTR4_REAL_TREE_LIMIT=0 HTR4_VALIDATE_ONLY=1 \
cargo run --release -p heddle-objects --features zstd \
  --example htr4_measure -- \
  /path/to/heddle.git /path/to/ripgrep.git /path/to/flask.git /path/to/git.git \
  /path/to/monorepo.git /path/to/many-tiny-trees.git \
  /path/to/huge-fanout.git /path/to/deep-vendored.git
```

The harness self-checks structural and byte-delta round trips, resolves sampled
objects through the hash index from the concatenated pack buffer, verifies the
final tree hash, and compares direct entry lookup with the source tree.
