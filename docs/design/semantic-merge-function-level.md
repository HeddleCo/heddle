# Semantic merge — function-level resolution

This doc covers the semantic integration engine used by `heddle ready`,
`heddle land`, and `heddle sync` when built with the `semantic` cargo feature:
how it decomposes a parseable source file into
AST-defined items, merges each item independently against base / ours / theirs,
and falls back to the hunk-level [`heddle-merge`](../../crates/merge) engine
when AST decomposition declines.

It builds on the hunk-level merge engine shipped under
[heddle#79 / PR #84](https://github.com/HeddleCo/heddle/pull/84) (`heddle-merge`)
and addresses the bail-to-whole-file regression flagged in
[heddle#54](https://github.com/HeddleCo/heddle/issues/54) — when both sides
structurally reshape the same file (function reordering, function add/delete),
the line-based diff3 in `heddle-merge` produces a single oversized unstable
hunk that surfaces as a "whole-file collision" to the operator. Audience:
anyone touching `crates/semantic/src/merge_driver/` or the merge command's
content-merge wiring in `crates/cli/src/cli/commands/merge/merge_algo/`.

## The gap

`heddle-merge` performs a diff3-style walk over base lines, identifying *stable*
alignment anchors (lines that line up to the current expected position in both
`ours` and `theirs`) and emitting per-hunk conflict markers around *unstable
hunks*. This is correct for hunk-shaped edits but degrades when both sides
reshape the structure of the file: function reordering shifts every line below
the moved function, function add/delete shifts every line past the insertion
point, and the LCS aligner finds few stable anchors. The unstable hunk grows
until it spans most of the file, and the operator sees a single conflict block
that contains the entire body of both sides — *the* "whole-file collision"
shape the trip report identified.

Before semantic merge became the default, the old `--semantic` flag was easy to
miss and the plain merge path stayed hunk-only. That made the differentiating
AST-aware behavior opt-in even though the `semantic` cargo feature ships in the
default feature set.

## Contract

The semantic merger MUST satisfy the following per-file outcome shape:

| Both sides touch… | Outcome |
|---|---|
| Different functions | Clean merge, zero conflict markers |
| Same function, different lines inside it | Hunk-level conflict markers, scoped to inside that function's body |
| Same function, overlapping lines | Line-level conflict markers, same shape as `heddle-merge` produces today |
| Non-function content (imports, statics, mod-level macros, comments) | Hunk-level via `heddle-merge` on the inter-function segments |
| File the parser can't accept (binary, unsupported language, syntactic errors) | Whole-file `heddle-merge` fallback, same as today |

The contract is conservative on rename detection and on the cross-function
classification: when in doubt, the merger falls through to `heddle-merge` on a
narrower segment. Whole-file fallback is reserved for the unparseable case.

## Algorithm

1. **Detect language** from path extension (`crates/semantic/src/parser/parser_language.rs`).
   `Language::Unknown` ⇒ fall through to `heddle-merge::text_hunk_merge`.

2. **Parse all three blobs** using `ParsedFile::parse` (tree-sitter). `parse`
   returns `None` when the file contains syntactic errors — when *any* of the
   three sides fails to parse, fall through. A parse failure on just one side
   is enough to disqualify semantic resolution: the matched item ranges
   wouldn't have a counterpart.

3. **Extract top-level segments** from each side:
   - Top-level *function-like* items: `fn`, `impl` blocks, `mod` blocks with
     bodies, trait blocks. Each item gets a stable `ItemKey` consisting of
     `(kind, name)` plus a signature hash for ambiguity resolution.
   - For nested closures and inner functions, only the outer item is recorded;
     the inner edits are resolved by the recursive merge of the outer item's
     body bytes.
   - Items overlap-de-duped by retaining the outermost.
   - Non-item content (everything not inside an item's byte range) forms the
     *inter-item segments*: a preamble (before the first item), one segment
     between each pair of consecutive items, and a postamble (after the last
     item).

4. **Match items across the three sides** by `ItemKey`. Build the merged item
   list:

   | base | ours | theirs | resolution |
   |---|---|---|---|
   | present | present | present | 3-way merge of item bytes (recurses to `heddle-merge` on body); see "Item merge" below |
   | present | present | absent | modify/delete: clean delete if `ours == base`, else conflict |
   | present | absent | present | symmetric to above |
   | present | absent | absent | clean delete |
   | absent | present | absent | clean add (take ours) |
   | absent | absent | present | clean add (take theirs) |
   | absent | present | present | both-added: clean if `ours == theirs`, else conflict |
   | absent | absent | absent | impossible by construction |

5. **Merge the inter-item segments** as a single `heddle-merge::text_hunk_merge`
   call on `(base_preamble || base_seg_1 || … || base_postamble)` vs the
   equivalent concatenations from ours / theirs. Inter-item content is rare
   enough in practice (imports, top-level statics) that running a single
   hunk-merge over the concatenated non-item bytes is both simpler and more
   accurate than trying to align per-segment, since segment boundaries can
   drift when items are reordered.

6. **Reconstruct the output file** by emitting the merged inter-item segments
   interleaved with the merged item bytes, in *base's* item order. Items added
   on a single side are appended at the position where they appear on that
   side (preserving locality), with ties broken in favour of the side that
   inserted closer to the base order it shares with the other side.

7. **Tally conflicts**: a per-item conflict adds one to the marker count; the
   inter-item merge adds its own marker count. If the total is zero, return
   `MergeOutcome::Clean`; otherwise `MergeOutcome::Conflicts`.

### Item merge

Given `(base_bytes, ours_bytes, theirs_bytes)` for a matched item:

- `ours_bytes == base_bytes` ⇒ take `theirs_bytes`.
- `theirs_bytes == base_bytes` ⇒ take `ours_bytes`.
- `ours_bytes == theirs_bytes` ⇒ take either.
- Otherwise ⇒ `heddle-merge::text_hunk_merge(base_bytes, ours_bytes, theirs_bytes)`.

This recursive fallback keeps the hunk-level conflict markers scoped *inside*
the function body. Markers don't span function boundaries because the merge
operates on each item independently.

### Rename detection

Out of scope for v1. The brief specifically calls out
`analysis_renames::detect_function_changes`, which already implements
similarity-based rename detection for diff display. Wiring this into the
merger turns rename-vs-modify into a tractable 3-way merge, but it's a
separate concern and is deferred. v1 treats a function present in base + ours
under name `foo` but appearing as `bar` in theirs as: `foo` is deleted in
theirs and `bar` is added. If `foo` was also modified in ours, this surfaces
as modify/delete on `foo` plus clean-add on `bar`. Acceptable for v1 —
matches git's default behaviour.

### Reordering

If `ours` moves function A from line 100 to line 500 with no body change, and
`theirs` modifies function B at line 300, base's item order is `[A, B]`, ours
is `[B, A]`, theirs is `[A, B']`. The item merge resolves `A` to `A` (no
change on either side) and `B` to `B'` (modified by theirs). Reconstruction
uses base's order, producing `[A, B']` — i.e. ours's reordering is lost.
This is acceptable for v1: reordering with no body change is a cosmetic edit,
and merging cleanly is preferable to surfacing a conflict the operator must
hand-resolve.

## Where the code lives

- `crates/semantic/src/merge_driver/` — new module hosting the function-level
  driver. Top-level entry point: `semantic_three_way_merge`.
- `crates/cli/src/cli/commands/merge/merge_algo/executor.rs` — the file-level
  merge fan-out. `text_hunk_merge_blobs` routes through the semantic driver
  when `ConflictLabels::strategy == MergeStrategy::Semantic`.
- `crates/cli/src/cli/commands/merge/merge_algo/mod.rs` — `ConflictLabels`
  carries the `strategy` enum.

## Non-goals

- Markdown / TOML / JSON structural merging. The test matrix exercises these
  paths but the resolution is "fall through to `heddle-merge`" — the
  semantic-merge driver only knows about tree-sitter-parseable source code.
- Optimizing tree-sitter parse time. The profiling artifact will quantify
  parser cost; if it dominates, file a follow-up.
- Cross-file rename / move semantics. The driver operates per-file.
- Detecting `pub use` re-export merges. Rust's `use_declaration` is treated as
  non-item content and goes through the inter-item hunk merge — sufficient
  for the common pattern of two branches adding new re-exports.

## Performance budget

For files up to ~10k lines and ~1000 functions:

- Parser cost (tree-sitter): one parse per side, dominated by `tree-sitter-rust`'s
  internal LR step. Benchmark target: ≤ 50ms for 10k lines.
- Item extraction: linear in node count. Negligible.
- Per-item merge: O(item bytes) per matched item; sum ≤ file size, so the
  total cost is O(file size).
- Inter-item merge: a single `heddle-merge` pass on the concatenated non-item
  bytes (typically <10% of file size for source files).

Total per-file budget: 5-10× the cost of a plain `heddle-merge` pass on the
same file. Acceptable given the conflict-rate reduction.

## Failure modes

- **Parser regression**: if `tree-sitter-rust` upgrades introduce a new node
  kind for impl methods, `is_function_node` returns false, function-level
  resolution silently degrades. Mitigated by the regression-fixture tests
  exercising the real `crates/repo/src/lib.rs` heddle source.
- **Encoding edge cases**: tree-sitter parses UTF-8; non-UTF-8 input is
  caught by `ParsedFile::parse` returning `None` (the parser's lexer
  rejects). Same fall-through path as binary.
- **Memory pressure**: per-item bytes are owned `Vec<u8>` slices, not
  zero-copy views. For a 10k-line file the working-set grows by ~3× the file
  size during merge (base + ours + theirs item bytes). Acceptable; profiled
  in the dhat run.
