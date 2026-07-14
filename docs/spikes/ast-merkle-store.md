# Spike: content-addressed AST / symbol-graph store over the Merkle DAG

Status: spike (design only) · Repo: heddle · Feeds: code-symbol search (weft#451 Tier-2), semantic merge.

## Goal
Store tree-sitter ASTs (and a cross-file symbol graph) as content-addressed objects over heddle's existing Merkle DAG, so we can build the default-branch graph once, update it incrementally per commit, and materialize + query the **complete tree graph at any commit/branch** cheaply.

## What already exists (do not rebuild)
- **Merkle object store** — `heddle/crates/objects` is a content-addressed DAG (blob/tree/commit; stable change ids). New object kinds are defined with the `versioned_msgpack_blob!` macro (see `object/state_context.rs::ContextBlob`, `object/discussion.rs`, `object/state_review.rs`). This is the template for an AST object kind.
- **tree-sitter + grammars** — pinned in weft (`tree-sitter 0.26` + rust/ts/js/python/go/java/c/cpp). The `heddle-semantic` crate owns the parsing.
- **`heddle-semantic` crate** already has, TODAY (on-the-fly, not persisted): `parser` (tree-sitter → `ParsedFile`, `Language`), `symbol_extraction.rs`, `symbol_resolver.rs`, `analysis` (`HotSpot`/`HotEventKind` change events), `merge_driver` (semantic merge), `cache.rs`. weft consumes it feature-gated (`GetSemanticHotSpots`, `content.rs`).
- **Ingest hook precedent** — `heddle/crates/ingest` already runs per-content extraction during ingest (`reasoning_extract.rs`/`reasoning_pipeline.rs`), so a per-blob AST-extraction hook has an established seam.

**So this is a promotion, not a greenfield build:** take the analysis `heddle-semantic` already does on-the-fly and (a) persist it as content-addressed objects, (b) make the cross-file graph incremental, (c) expose a query surface.

## The reframe that makes it tractable: two layers, different physics

**Syntax is content-addressable; semantics is not.** Splitting on that line is the whole design.

### Layer A — AST-per-blob (trivially Merkle, cheap)
A new object kind `AstBlob` = a normalized, language-agnostic IR (node kind + byte span + child links + captured symbol occurrences), serialized as a flat post-order array (cache-friendly, mmap-scannable), content-addressed **by the source blob's oid**: a pure memoized function `ast(blob_oid) → AstBlob`.
- **Structural sharing is free** — an unchanged file across commits/branches has the same blob oid → same `AstBlob` oid → zero recompute. Branch switches share exactly what the source shares.
- **"Iteratively update" is the tree-diff** — sley already yields the changed-blob set per commit; re-parse only those (tree-sitter is MB/s; intra-file incremental parsing is not worth it at VCS granularity). The new commit's tree references the new blob oids; their ASTs are looked up or computed.
- **"Complete tree at any point" = walk commit C's tree → fetch each blob's `AstBlob`** (unchanged ones cached). Time-travel is free (commit-anchored + content-addressed).
- **Delivers on its own:** syntax-aware diff, per-file structure, single-file symbol extraction. This is the de-risking first slice.

### Layer B — cross-file symbol graph (the real project)
"Complete tree *graph*" = defs↔refs, imports, calls across files. A cross-file edge depends on **two** blobs' content + language resolution rules → **not content-addressable**, and an edit invalidates edges non-locally (A's exports change → every importer of A, transitively, re-resolves). This is incremental compilation, not Merkle.
1. **Per-file symbol tables** (exports/imports/defs/refs) — extracted from `AstBlob`, keyed by blob oid → these DO content-address and Merkle-share (extend `symbol_extraction.rs`).
2. **Resolution pass** — bind refs→defs across files (per-language name binding; `symbol_resolver.rs` is the seed).
3. **Reverse-dependency index** (file → its importers) so an edit re-resolves only the invalidation **frontier**.
4. **Persist the resolved edge-set per commit as a delta over the parent** — only edges touching changed files change.

### Layer C — query / operate
A memoized (Salsa-style) query engine: "refs of X at commit C", "callers of F", "structural diff C1→C2". Resolution is memoized + content-anchored, so repeat queries at a commit are O(1) and switching commits re-resolves only the diff frontier.

## Prior art to steal from (don't roll your own where you don't have to)
- **`tree-sitter-stack-graphs` (GitHub)** — exactly incremental, per-file, content-addressable name resolution across a repo on tree-sitter. This is the leverage for Layer B; the spike's central *decision* is **stack-graphs vs. extending `symbol_resolver.rs`** per-language by hand. Recommend evaluating stack-graphs first — per-language name binding is 80% of Layer B's cost.
- **Salsa / rust-analyzer** — the incremental query + invalidation-frontier engine (Layer C).
- **GitHub `semantic` (archived)** — tree-sitter → per-language symbol-table shape.

## Hard parts (be honest)
- **Per-language resolution is the cost center.** tree-sitter gives uniform *syntax* cheaply; *name binding* differs per language. Bound v1 to the languages that matter (rust, ts/js, python) and lean on stack-graphs.
- **Storage.** AST IR ≈ 2–5× source. Merkle sharing amortizes across history/branches, but full default-branch history is large → relies on object GC/packing (already a known pressure point — heddle-readpath-perf / loose-object gc). The spike must include a packing/GC plan for `AstBlob` + a "don't index generated/vendored/binary" filter.
- **The graph is NOT one clean Merkle tree.** It's a Merkle *syntax* layer + a *derived* semantic graph needing real incremental invalidation. Don't design as if it's all content-addressed.

## Strategic fit
- **Unblocks Tier-2 code-symbol search** (weft#451) — the symbol graph IS that index; the search spike explicitly deferred symbols to "a dedicated index."
- **Strengthens semantic merge** (`heddle-semantic/merge_driver`) — heddle's stated moat vs Mesa/jj. Structural diff + syntax-aware conflict resolution consume exactly this graph.
- **Agent-native VCS** — lets agents operate on code structurally (rename-symbol, find-callers, structural patch) rather than textually.

## Open decisions this spike must resolve (the point of the spike)
1. **AstBlob IR schema** — node model, span encoding, symbol-occurrence capture; msgpack via `versioned_msgpack_blob!` vs a flat binary. (Draft the schema.)
2. **Where AstBlob lives** — a first-class object kind in the DAG vs a derived side-store keyed by blob oid. (Merkle-native = object kind; but derived/regenerable argues for a rebuildable cache — decide, incl. GC policy.)
3. **Ingest hook point** — parse-on-ingest (eager, in `importer.rs` alongside `reasoning_extract`) vs parse-on-first-query (lazy). Recommend lazy+memoized to start.
4. **Layer B substrate** — stack-graphs vs extend `symbol_resolver.rs`. THE decision. Prototype a rust+ts resolution on both, compare effort + incrementality.
5. **Invalidation-frontier model** — reverse-dep index shape; how the resolved edge-set is stored per commit (full vs delta-over-parent).
6. **Query API surface** — what weft/CLI RPCs expose (feeds the search + nav surfaces); Salsa vs hand-rolled memoization.
7. **Language scope for v1** + the not-index filter (generated/vendored/binary/oversized).
8. **GC/packing plan** for AST/symbol objects at default-branch-history scale.

## Phasing (recommended)
- **Phase A — AstBlob content-addressed store + lazy parse-on-query + a `heddle ast`/query seam.** Standalone value (syntax diff, per-file structure, single-file symbols). Low risk, weeks. Build first regardless.
- **Phase B — symbol tables + cross-file resolution (stack-graphs eval) + reverse-dep incremental frontier.** The real project; own spike-decided substrate.
- **Phase C — memoized query engine + time-travel queries + wire to search Tier-2 / semantic merge.**

## DoD for this spike
Land this doc with: the drafted `AstBlob` IR schema; the object-kind-vs-side-store decision with GC plan; a stack-graphs-vs-`symbol_resolver` evaluation (prototype both on a rust+ts fixture, report effort + incremental-update behavior); the invalidation-frontier design; the query-API sketch; v1 language scope + not-index filter. Then it hands off to Phase-A implementation.
