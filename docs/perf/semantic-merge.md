# Semantic merge — performance notes

Bench harness: `crates/semantic/benches/merge_function_level.rs` (Criterion).
Raw numbers archived in [`semantic-merge-baseline.json`](semantic-merge-baseline.json).

## Throughput

Workload "disjoint" — ours edits first half of functions, theirs edits second
half. Both engines should produce a clean merge. Numbers are median across
Criterion's quick run (1s warm-up + 1s measurement); rerun under the default
configuration for production sign-off.

| # functions | `text_hunk_merge` | `semantic_three_way_merge` | semantic overhead |
|---:|---:|---:|---:|
| 10 (≈ 100 LOC)    | 19 μs    | 0.95 ms  | 48× |
| 100 (≈ 1k LOC)    | 1.8 ms   | 10.4 ms  | 5.7× |
| 1000 (≈ 10k LOC)  | 9.2 ms   | 90 ms    | 9.8× |

Workload "structural-reshape" — ours reverses function order, theirs modifies
one function. This is the heddle#54 shape; `text_hunk_merge` produces conflict
markers and the semantic driver resolves cleanly.

| # functions | `text_hunk_merge` | `semantic_three_way_merge` | semantic overhead |
|---:|---:|---:|---:|
| 10              | 21 μs     | 377 μs    | 18× |
| 100             | 205 μs    | 2.6 ms    | 13× |
| 1000            | 2.2 ms    | 25.7 ms   | 12× |

Conclusion: semantic merge is 9-12× slower than the text engine at realistic
file sizes (1k-10k LOC), with overhead heavily front-loaded by the parser. At
the small-file end (10-100 functions) the multiplier is worse because the
parser has a fixed setup cost.

## Cost split — where does the time go

For a 1000-function file (≈ 60 KB per side, 180 KB total):

| Stage | Median time |
|---|---:|
| `text_hunk_merge` baseline (whole-file) | 9.9 ms |
| Parsing all 3 sides with tree-sitter (parse_only ×3) | 78 ms |
| Full semantic merge | 93 ms |
| Item extraction + reconstruction overhead | ≈ 5 ms (93 - 78 - 10) |

**The parser is the hotspot — 84 % of wall-clock time.** Item extraction and
reconstruction together cost ~5 ms (about half of what the text engine costs
on its own). Per `docs/design/semantic-merge-function-level.md` follow-up:
this points at `heddle-semantic::cache::SemanticParseCache` integration so a
single tree-sitter parse can be amortized across post-merge `--with-diff`
display and the merge driver. Filing as a separate issue; not in scope for
this PR.

## Memory

The driver allocates owned `Vec<u8>` slices for each matched item's body
(ours, theirs, base) when running the per-item three-way merge — a `~3×` peak
of file size during execution. At 60 KB per side this is ~180 KB peak; at
600 KB per side (a 100k-LOC monolith) we'd see ~1.8 MB peak. Acceptable for
the merge driver's transient lifecycle; not a concern at heddle's typical
file sizes.

## Flamegraph

See [`semantic-merge-flame.svg`](semantic-merge-flame.svg) for the visual.
Captured by running `crates/semantic/benches/profile_target.rs` with
`pprof-rs` at 997 Hz × 200 iterations × 1000 functions
(`HEDDLE_PROFILE_ITERS=200 HEDDLE_PROFILE_N=1000`).

Top frames by sample share, out of 21,663 total samples in
`semantic_three_way_merge`:

| Frame | Samples | Share |
|---|---:|---:|
| `ParsedFile::parse` (tree-sitter) | 19,492 | **90.0 %** |
| └ `ts_parser_parse_with_options` (libtree-sitter-rust) | 19,417 | 89.6 % |
| `similar::algorithms::myers::conquer` (LCS for hunk-level) | 4,031 | 18.6 % |
| `ts_parser__condense_stack` | 12,297 | 56.8 % |
| `ts_parser__handle_error` | 12,097 | 55.8 % |

The big takeaway: the parser owns ~90 % of wall time. The merge logic
proper (item extraction, key matching, per-item resolution, inter-item
concat + `text_hunk_merge`) is a rounding error by comparison.

`ts_parser__handle_error` showing up at 55.8 % is interesting and tells
us tree-sitter is spending substantial time on error-recovery paths even
on syntactically valid input — likely the parse table exploring
ambiguities during the LR walk. Optimizing this is out of scope; it's an
upstream concern of `tree-sitter` itself.

**Follow-up issue** (file separately): integrate
`heddle-semantic::cache::SemanticParseCache` into the merge driver. The
cache already keys `ParsedFile` by `(content_hash, language)` for the
diff display path; reusing it during merge would amortize the parse
cost across `--with-diff`'s post-merge call. Expected win: ~50 % drop in
wall time when the diff display fires after a merge (both invocations
share the parses).

## Reproducing

```sh
# Quick run (~30s):
cargo bench -p heddle-semantic --bench merge_function_level -- \
    --quick --measurement-time 1 --warm-up-time 1

# Production run (~5min):
cargo bench -p heddle-semantic --bench merge_function_level

# Baseline + comparison:
cargo bench -p heddle-semantic --bench merge_function_level -- --save-baseline pre
# ... make changes ...
cargo bench -p heddle-semantic --bench merge_function_level -- --baseline pre
```

Criterion writes per-bench HTML reports + per-run JSON into
`target/criterion/`. The committed baseline at
[`semantic-merge-baseline.json`](semantic-merge-baseline.json) is the
machine-readable summary used to detect regressions in CI follow-ups.
