# Delta Compression

## Overview

Heddle's packfile format uses delta compression to reduce storage size. The system
lives in two layers: the **delta codec** (`crates/core/src/delta/`) encodes/decodes
individual deltas, and the **pack builder** (`crates/core/src/store/pack/pack_builder.rs`)
decides which objects to delta against which bases.

## Current Results (as of 2026-03-12)

| Repo | Objects | Git | Heddle | Ratio |
|------|---------|-----|------|-------|
| goawk | 4262 | 2596 KB | ~3011 KB | **1.16x** |
| git-semver | 422 | 191 KB | ~121 KB | **0.63x** |

Heddle beats Git on small repos (zstd advantage) and is within 1.2x on medium repos.

## Architecture

### Pack Format

```
[LMPK magic][version: u32][object_count: u64]
[entries...]
[blake3 checksum: 32B]
```

Each entry:
```
[hash: 32B][type+uncompressed_size: varint][compressed_size: varint]
[base_hash: 32B (delta only)][compressed_data]
```

Uses raw zstd compression (no wrapper header) since the entry already records
both sizes. The varint encoding saves 6-10 bytes per entry vs a naive format
with fixed u32 fields.

### Delta Instruction Format

Identical to Git's format. Two instruction types:

**Copy** (2-8 bytes, typically 2-4):
```
Byte 0: 1oooosss
  bit 7: always 1 (copy flag)
  bits 0-3 (o): which offset bytes follow (up to 4 -> 32-bit offset)
  bits 4-6 (s): which size bytes follow (up to 3 -> 24-bit size)
  all s bits zero -> size = 0x10000
[offset bytes, low to high, only those flagged]
[size bytes, low to high, only those flagged]
```

**Insert**: `[length-1] [literal bytes]` (max 127 bytes per chunk).

Key detail: offset byte 0 is always emitted (bit 0 always set) to avoid
producing the reserved `0x80` instruction (which occurs when offset=0 and
size=0x10000, leaving no bits set except the copy flag).

### Sliding Window Base Selection

The pack builder groups objects by type, sorts them for optimal adjacency, then
slides a window of W=10 across the sorted list. For each object, it estimates
delta size against all window entries and picks the smallest. If the best delta
(after zstd compression) is smaller than the raw compressed data, it encodes as
a delta; otherwise it stores raw.

Window entries cache their 4-byte hash index (`HashMap<[u8;4], Vec<usize>>`)
to avoid rebuilding it W times per object. This is the single biggest
performance optimization in the delta path.

Chain depth is tracked per window entry. When B deltas against A,
`B.depth = A.depth + 1`. Candidates at `depth >= MAX_DELTA_CHAIN_DEPTH` (50)
are skipped.

### Sort Order

Objects within each type group are sorted by **(extension, basename, size desc)**.
This clusters files with the same extension together (all `.go` files adjacent),
then same-named files within that, with largest first (best delta base candidate).

## Key Files

| File | Purpose |
|------|---------|
| `crates/core/src/delta/delta_encoder.rs` | Delta encoding, hash index building, size estimation |
| `crates/core/src/delta/delta_decoder.rs` | Delta decoding with structured errors |
| `crates/core/src/delta/delta_tests.rs` | Codec roundtrip, boundary, and error tests |
| `crates/core/src/store/pack/pack_builder.rs` | Pack building, sliding window, sort order |
| `crates/core/src/store/pack/pack_reader.rs` | Pack reading, delta chain resolution |
| `crates/core/src/store/pack/pack_tests.rs` | Pack-level roundtrip and integration tests |
| `crates/core/src/store/pack/varint.rs` | Varint encoding for type+size pairs |

## Important Constants

| Constant | Value | Location | Notes |
|----------|-------|----------|-------|
| `MIN_DELTA_SIZE` | 64 | `pack_builder.rs` | Objects smaller than this skip delta entirely |
| `MAX_DELTA_CHAIN_DEPTH` | 50 | `pack_builder.rs` | Matches Git's default |
| `WINDOW_SIZE` | 10 | `pack_builder.rs` | Number of recent objects to try as bases |
| `MIN_MATCH_LENGTH_LARGE` | 16 | `delta/delta_encoder.rs` | Min match for targets >= 1024 bytes |
| `MIN_MATCH_LENGTH_SMALL` | 8 | `delta/delta_encoder.rs` | Min match for targets < 1024 bytes |
| `MAX_DELTA_OUTPUT_SIZE` | 128 MB | `delta/delta_decoder.rs` | Safety limit for decoded output |

## Testing Notes

- Always run with `--features zstd` to test the full compression
  path. Some tests only exist in the zstd-enabled build.
- Delta tests with "dissimilar" data must account for zstd making delta+compress
  beat raw+compress even when objects share no logical similarity. Don't assert
  `delta_count == 0` for dissimilar data; assert roundtrip correctness instead.
- The `estimate_delta_size` function must produce results identical to the actual
  `encode` — several tests verify exact equality.

## Strategies Tried

### What worked

1. **Git-style variable-length copy instructions** — Uses Git's 2-8 byte
   format instead of a fixed 5-6 byte layout. Pure encoding efficiency win
   with no algorithmic risk.

2. **Sliding window base selection (W=10)** — Replaced sequential chaining
   (each object deltas against the previous) with trying all W recent objects
   and picking the best. Increased delta utilization from 57% to 85% on goawk.

3. **Extension-first sorting** — Sorting by `(extension, basename, size desc)`
   before the sliding window. Groups all `.go` files together, giving the window
   maximum overlap opportunities within the same file type.

4. **Lowering MIN_DELTA_SIZE from 256 to 64** — More small objects eligible for
   delta encoding. Modest improvement (~20-50 KB on goawk).

5. **Adaptive MIN_MATCH_LENGTH** — 8 bytes for small targets (< 1024), 16 for
   larger. Lets small files find matches that would be missed at the higher
   threshold.

6. **Cached hash indices in window entries** — Building the 4-byte hash index
   once per object when it enters the window, rather than rebuilding for every
   comparison. Critical for performance with W=10.

### What didn't work

1. **Git's `pack_name_hash()` for sorting** — Git uses
   `hash = (hash >> 2) + (c << 24)` where last characters dominate. This
   clusters files by basename ending, which works when the window contains
   temporal versions of the same file (commit N, N-1, N-2 of `parser.go`).
   But Heddle imports objects grouped by type without temporal ordering, so the
   name-hash scattered related files without benefit. Result: goawk went from
   3011 KB to 3265 KB (8% worse).

2. **Custom shift-left-5 name hash** — Variant of Git's hash with different
   bit mixing. Also performed worse than extension-first: 3122 KB on goawk.

## The Remaining Gap: Why Git is Still Smaller on Large Repos

The ~1.16x gap on goawk comes from **temporal ordering**. Git knows the commit
graph and can sort objects so that `parser.go@commit-N` is adjacent to
`parser.go@commit-N-1`. This means the sliding window always contains the most
relevant delta base (the previous version of the same file).

Heddle's content-addressed store groups objects by type but has no inherent
temporal ordering. Even with extension-first sorting, the window of 10 may not
contain the ideal base when there are hundreds of `.go` files.

## Future Improvement Strategies

### High potential

1. **Temporal ordering from import history** — During `heddle import git`, walk the
   Git commit graph and assign each object a "generation number" or commit
   timestamp. Sort by `(extension, basename, generation desc)` so recent
   versions of the same file land adjacent. This is the single biggest
   remaining opportunity and would close most of the gap with Git.

2. **Larger window (W=50 or W=100)** — More candidates to try. Git uses W=10
   as default but supports `--window=250` for aggressive packing. Heddle's cached
   indices make this feasible. Trade-off: memory (each entry holds full data +
   index) and CPU (more delta estimates per object). Worth benchmarking.

3. **Two-pass packing** — First pass: estimate deltas with a large window to
   find optimal base pairs. Second pass: encode only the winning pairs. Avoids
   the memory cost of a huge window by only keeping the best matches.

### Medium potential

4. **Tree delta encoding** — Currently `ObjectType::State` objects skip delta
   entirely. Tree objects (which are often very similar across commits) could
   benefit from delta encoding too. Would need careful handling of the tree
   serialization format.

5. **Object reuse across packs** — When repacking, reuse existing delta
   encodings from source packs instead of re-encoding from scratch. Git does
   this aggressively and it's a big performance win for incremental operations.

6. **Better hash index** — The current 4-byte rolling hash has collisions.
   A gear-hash or content-defined chunking approach could find better matches,
   especially for files where edits shift content (insertions/deletions).

### Low potential / diminishing returns

7. **Increasing MAX_DELTA_CHAIN_DEPTH beyond 50** — Diminishing returns and
   slower reads. Git uses 50 as default for good reason.

8. **Dictionary-based zstd compression** — Train a zstd dictionary on common
   patterns and use it across entries. Complex to implement and the delta
   encoding already captures most redundancy.

9. **Further lowering MIN_DELTA_SIZE below 64** — Objects that small rarely
   produce beneficial deltas. The overhead of the delta header and base hash
   reference (32 bytes) erases most savings.

## Common Pitfalls

- **Bit ordering in copy instructions**: Git's format puts offset flags in bits
  0-3 and size flags in bits 4-6. Easy to swap these (we did, causing
  `InvalidBaseRange` errors with huge garbage offsets).

- **Reserved instruction 0x80**: When offset=0 and size=0x10000, no extra bytes
  are needed, producing cmd=0x80 (just the copy flag). This is reserved in
  Git's format. Always emit at least offset byte 0 to avoid it.

- **Rust string hex escapes**: `"\xff"` is invalid in Rust strings (only
  `\x00`-`\x7f` allowed). Use `"\u{FFFF}"` for Unicode escapes in sort sentinel
  values.

- **zstd invalidating test assumptions**: With zstd enabled, delta+compress can
  beat raw+compress even for unrelated data. Tests must not assume "different
  data = no deltas". Assert roundtrip correctness, not delta count.

- **Feature gate awareness**: The `zstd` feature is not in default
  features. Always test with `cargo test --features zstd` to
  exercise the full compression path.
