# HTR4 radical tree-storage spike

Date: 2026-08-27  
Branch: `spike/htr4-radical`  
Base: `bd2f083c` (`measure/htr4-vs-git`)

## Verdict

There is a credible storage format beyond v5, but this spike did **not** find a
complete magic path that clears every invariant.

The best prototype—lean materialized anchors plus anchor-relative coalesced
deltas—stores the complete 212,832-tree corpus in 511,721,061 bytes. That is
0.284x Git loose, 6.135x Git packed, 0.193x v5, and 0.152x raw HTR4. Its
file-backed first-entry read is 1.062x raw bytes and 1.248x raw latency; its
first-100 read is 0.932x raw bytes and 0.134x raw latency. A known one-entry
delta at 100,000 entries encodes in 294 ns.

The end-to-end capture gate remains red. Production v5 measures 224.177 ms p95
at 100,000 paths. A controlled raw-tree-write build measures 223.449 ms, proving
that merely deferring zstd does not recover the budget. Both runs decode 1,008
objects for a one-file capture. Capture must consume parent/tree-index changes
incrementally before this storage primitive can matter end to end.

Therefore:

- **smaller than Git loose + seekable:** yes, on the aggregate and on every
  corpus except the deliberately history-free many-tiny-trees case;
- **close to Git packed:** no in general (6.135x aggregate), although the
  one-file huge-fanout history is 0.919x Git packed;
- **under the 100 ms capture gate:** no; measured at 224.177 ms, with raw
  encoding as a negative control at 223.449 ms;
- **winner/PR:** no. Keep this as a design/prototype commit, not a production
  format PR.

## Prototype: HLR1 anchors + HDC1 coalesced deltas

### Lean restartable anchors (HLR1)

HLR1 is the cheap hot/base form:

- the content-addressed store key supplies the expected tree ID, so the body
  does not repeat the 32-byte tree hash;
- counts, name-prefix lengths, and suffix lengths use varints;
- sorted names use predecessor-prefix compression;
- entry mode/type and the complete 32-byte target hash remain in the body;
- full decode recomputes the normal Heddle semantic tree hash and compares it
  with the external expected ID.

HLR1 is restartable by retaining the predecessor name in a resume cursor. A
production format would add sparse restart records for arbitrary ordinal seeks;
the prototype exercises the required first-entry and first-100 paths.

### One-hop coalesced parent deltas (HDC1)

Each HDC1 object stores sorted upsert/remove operations relative to a
materialized ancestor, not relative to the immediately previous delta. The
capture path can update the parent's coalesced operation map with its known
changed entries. Full reconstruction always needs one anchor plus one delta;
there is no read-time delta chain.

The prototype refreshes an anchor after 127 descendants, rejects deltas over
512 coalesced operations, and falls back to an anchor whenever a delta is not
smaller. These are local object dependencies: retaining a delta and its named
anchor is sufficient; no global pack or GC lifecycle is mandatory.

### Seek front porch

An HDC1 header carries byte offsets and anchor-entry counts for entry 1 and
entry 100. The reader fetches only the relevant leading delta operations. A
block-compressed anchor additionally stores a compact HLR1 copy of its first
100 entries. The porch is paid once per anchor and shared by up to 127 delta
objects.

The encoder falls back to a new anchor if deletes would require more than one
anchor entry for the first-entry read or more than 100 for the first-100 read.
This makes the byte bound structural rather than corpus luck.

### Deferred settlement

The hot form is HLR1 for an anchor or HDC1 for a delta. In the settled form, an
anchor uses the smaller of HLR1 and v5 plus its HLR1 front porch. Delta bodies
remain uncompressed because they are already small and prefix-indexed. The
prototype measures the hot and settled totals separately; it does not wire a
background sweeper into the production store.

## Complete-corpus storage results

All reachable, importable tree objects were measured (`HTR4_REAL_TREE_LIMIT=0`).
Git loose is the actual loose-object file size after unpacking with zlib level 1.
Git packed is the sum of `verify-pack` object-record sizes and excludes fixed
pack/index overhead, matching the inherited harness definition. Radical sizes
include every anchor body, delta body, and compressed-anchor seek porch.

| Corpus | Trees | Raw HTR4 | v5 | Git loose | Git packed | Radical settled | vs loose | vs packed |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| Heddle | 12,022 | 11,859,145 | 10,879,700 | 6,960,113 | 1,374,333 | 6,606,810 | 0.949x | 4.807x |
| ripgrep | 6,351 | 4,609,691 | 4,537,923 | 2,841,433 | 558,782 | 2,526,717 | 0.889x | 4.522x |
| Flask | 11,414 | 11,828,380 | 11,330,371 | 7,211,630 | 1,438,405 | 6,584,613 | 0.913x | 4.578x |
| Git | 167,988 | 3,301,664,847 | 2,600,180,873 | 1,769,314,788 | 78,319,496 | 493,722,880 | 0.279x | 6.304x |
| hierarchical monorepo | 910 | 2,096,415 | 1,552,791 | 1,059,260 | 299,367 | 745,089 | 0.703x | 2.489x |
| many tiny trees | 12,001 | 1,896,061 | 1,716,905 | 926,183 | 845,658 | 988,527 | 1.067x | 1.169x |
| huge fanout/history | 64 | 34,563,904 | 22,282,909 | 15,828,447 | 482,567 | 443,324 | 0.028x | 0.919x |
| deep vendored | 2,082 | 230,472 | 230,152 | 107,431 | 94,938 | 103,101 | 0.960x | 1.086x |
| **All** | **212,832** | **3,368,748,915** | **2,652,711,624** | **1,804,249,285** | **83,413,546** | **511,721,061** | **0.284x** | **6.135x** |

The hot total is 512,144,438 bytes; settlement saves only another 423,377
bytes because most retained anchors prefer the cheap lean form. Of 212,832
objects, 187,877 are deltas and 24,955 are materialized anchors. The history
walk found a capture-realistic parent for 197,412 objects; the remainder are
roots or objects introduced without a same-path first-parent tree.

The many-tiny-trees loss is the irreducible self-contained-object corner of this
prototype: there is one commit, no parent reuse, and Heddle retains 32-byte
targets against Git SHA-1's 20 bytes. A repo-local short-reference table could
close it, but would add lookup latency and a retention side structure; this
spike did not pretend that complexity was free.

## Encode/decode and partial access

The 100,000-entry capture fixture changes one known entry. Times are medians of
15 calibrated samples on an AMD Ryzen 7 7700, Rust 1.98.0. Git timings are not
reported: the inherited harness accounts real Git storage but does not provide
an in-process Git tree codec, and process-level `git mktree`/`cat-file` timing
would not be comparable.

| Format/operation | Bytes | Encode | Full decode |
|---|---:|---:|---:|
| raw HTR4 | 7,399,123 | 16.750 ms | 16.703 ms |
| v5 block-zstd | 4,537,894 | 41.816 ms | 34.925 ms |
| HLR1 lean anchor | 6,505,179 | 0.797 ms | 26.665 ms |
| HDC1 known one-entry delta | 119 | 0.000294 ms | 27.424 ms including anchor merge + full hash validation |
| HDC1 with full anchor/current diff | 119 | 0.713 ms | same as above |

The HLR1 encode advantage partly comes from emitting the already validated tree
without recomputing its content hash. HDC1 receives the already-known anchor
ID, as the store would. Every full-decode path above validates the resulting
semantic tree hash.

| File-backed partial read | Raw bytes | Radical bytes | Byte ratio | Raw median | Radical median | Time ratio |
|---|---:|---:|---:|---:|---:|---:|
| first entry | 111.33 | 118.21 | 1.062x | 6.464 us | 8.066 us | 1.248x |
| first 100 | 5,309.24 | 4,947.43 | 0.932x | 169.595 us | 22.763 us | 0.134x |

These samples cover 48 delta trees for entry 1 and 21 trees with at least 100
entries, evenly selected across the eight corpora. The prototype self-checks
lean and delta round trips and expected-hash mismatches; the inherited v5 checks
cover range reconstruction plus header/payload corruption.

## Capture gate and the actual wall

Exact current CI command, 20 samples:

| Build | `capture_one @ 100000` p95 | Budget | Object decodes | Result |
|---|---:|---:|---:|---|
| production v5 | 224.177 ms | 100 ms | 1,008 | red |
| temporary raw-tree-write control | 223.449 ms | 100 ms | 1,008 | red |

The raw control changed only `encode_tree` to emit raw HTR4 and was reverted
after measurement. It refutes the narrow deferred-zstd explanation. The next
prototype must change snapshot construction so the fsmonitor/tree index passes
the changed leaf-to-root path and parent tree objects directly to the writer.
Only then can HDC1's 294 ns known-delta encode replace the 1,008-object decode
walk. Until that integration clears the real gate, an end-to-end speed claim is
not supported.

## Reproduction

The real mirror heads used for this run were:

- Heddle `82a11acf0c3a39cf591835cc717892a25764d232`
- ripgrep `3fce3b5bb0236da2df6d99672afb8a719642eca7`
- Flask `d318b683471101618febed18996405ad26462110`
- Git `f78ce2f7b6df702f93d40b85d6bda92a3f65da79`

Generate the deterministic synthetic repositories:

```bash
corpus=$(mktemp -d /home/scratch/htr4-radical-corpus.XXXXXX)
bash crates/objects/examples/htr4_measure_corpus.sh "$corpus"
```

Run the complete measurement (replace the four real mirror paths):

```bash
HTR4_REAL_TREE_LIMIT=0 \
cargo run --release -p heddle-objects --features zstd \
  --example htr4_measure -- \
  /path/to/heddle.git /path/to/ripgrep.git /path/to/flask.git /path/to/git.git \
  "$corpus/monorepo.git" "$corpus/many-tiny-trees.git" \
  "$corpus/huge-fanout.git" "$corpus/deep-vendored.git"
```

The exhaustive run took 591.22 seconds and peaked at 13,772,248 KiB RSS. Keep
the default `HTR4_REAL_TREE_LIMIT=4000` for ordinary iteration; use zero only
when dependency-complete totals are required.

Run the production capture contract:

```bash
TMPDIR=/home/scratch \
cargo test --locked --release -p heddle-cli --test cli_basics \
  perf_core_loop::core_loop_release_contract -- --ignored --nocapture
```

The `AGENTS.md` ignore message still names the retired `cli_integration` test
binary; the command above is the current command from
`.github/workflows/perf-core-loop.yml`.
