# Spike: content-addressed AST / symbol-graph store over the Merkle DAG

Status: active design; Layer A decided by #1277 and Layer B decided by #1272 · Repo: heddle · Feeds: code-symbol search (weft#451 Tier-2), semantic merge.

## Goal
Store a compact semantic projection and cross-file symbol graph over heddle's existing Merkle DAG, while reconstructing full tree-sitter ASTs on demand, so we can build the default-branch graph once, update it incrementally per commit, and query the **complete tree graph at any commit/branch** cheaply.

## What already exists (do not rebuild)
- **Merkle object store** — `heddle/crates/objects` is a content-addressed DAG (blob/tree/commit; stable change ids). New object kinds are defined with the `versioned_msgpack_blob!` macro (see `object/state_context.rs::ContextBlob`, `object/discussion.rs`, `object/state_review.rs`). This is the template for derived semantic objects.
- **tree-sitter + grammars** — pinned in weft (`tree-sitter 0.26` + rust/ts/js/python/go/java/c/cpp). The `heddle-semantic` crate owns the parsing.
- **`heddle-semantic` crate** already has, TODAY (on-the-fly, not persisted): `parser` (tree-sitter → `ParsedFile`, `Language`), `symbol_extraction.rs`, `symbol_resolver.rs`, `analysis` (`HotSpot`/`HotEventKind` change events), `merge_driver` (semantic merge), `cache.rs`. weft consumes it feature-gated (`GetSemanticHotSpots`, `content.rs`).
- **Ingest hook precedent** — `heddle/crates/ingest` already runs per-content extraction during ingest (`reasoning_extract.rs`/`reasoning_pipeline.rs`), so per-blob semantic extraction has an established seam.

**So this is a promotion, not a greenfield build:** take the analysis `heddle-semantic` already does on-the-fly and (a) persist it as content-addressed objects, (b) make the cross-file graph incremental, (c) expose a query surface.

## The reframe that makes it tractable: two layers, different physics

**Syntax is content-addressable; semantics is not.** Splitting on that line is the whole design.

### Layer A — parse on demand; persist the compact semantic projection

**Decision #1277: no `AstBlob`.** Reconstruct the full AST from the source blob on demand with tree-sitter. Persist only the compact per-file semantic projection (definitions today; imports and occurrences in #1273). Storage, rather than parser throughput, is the binding constraint; revisit full-AST persistence only if measurements show parse latency is the bottleneck.

- **Structural sharing remains free** — an unchanged source blob maps to the same deterministic semantic file artifact, so commits and branches reuse it.
- **"Iteratively update" is the tree diff** — parse only new or changed source blobs. Intra-file incremental parsing is not useful at VCS-state granularity.
- **Time travel remains commit-anchored** — walk commit C's semantic tree; parse a source blob only for queries not answered by the compact artifact.

### Layer B — cross-file symbol graph (the real project)
"Complete tree *graph*" = defs↔refs, imports, calls across files. A cross-file edge depends on **two** blobs' content + language resolution rules → **not content-addressable**, and an edit invalidates edges non-locally (A's exports change → every importer of A, transitively, re-resolves). This is incremental compilation, not Merkle.
1. **Per-file symbol tables** (exports/imports/defs/refs) — extracted from an on-demand parse, keyed by blob oid → these DO content-address and Merkle-share (extend `symbol_extraction.rs`).
2. **Resolution pass** — bind refs→defs across files (per-language name binding; `symbol_resolver.rs` is the seed).
3. **Reverse-dependency index** (file → its importers) so an edit re-resolves only the invalidation **frontier**.
4. **Persist the resolved edge-set per commit as a delta over the parent** — only edges touching changed files change.

### Layer B decision (#1272): extend the Heddle-owned resolver

**Choose a Heddle-owned occurrence/import artifact and extend (while splitting up) `symbol_resolver`; do not adopt `tree-sitter-stack-graphs` as the Layer-B substrate.** Stack Graphs remains useful prior art for scope/path semantics, but it is not a viable dependency boundary for this graph.

The spike evaluated equivalent two-file Rust and TypeScript throwaway prototypes; their findings are retained here, while the rejected experiments and their dependencies are not shipped:

- The Heddle-owned prototype reused the existing definition walker, added import/call occurrence extraction, resolved both fixtures, and confirmed that canonical occurrence bytes need not include repository paths.
- The Stack Graphs prototype used `tree-sitter-stack-graphs = 0.10`, built minimal per-language TSG rules, and resolved both fixtures through the partial-path stitcher. Its dependency graph could not coexist with Heddle's parser ABI, so the prototype and dependency were discarded after recording the result.

| Criterion | `tree-sitter-stack-graphs` prototype | Heddle-owned resolver prototype |
|---|---|---|
| Rust + TS fixture | Both resolve. Minimal rules cover only free-function definitions and direct calls; they deliberately do not claim production language semantics. | Both resolve. Existing definition extraction is reused; the spike adds a small import/call pass and a deterministic name-binding pass. |
| Per-language effort | The framework does not supply binding semantics. TypeScript has a published language package, but its rule file is 6,297 lines; no published Rust package was found. Rust rules would therefore be a new language implementation. | Heddle already has about 1,280 lines of definition taxonomy across its supported grammars. Real cross-file work is still substantial (imports, aliases, scopes, type/value namespaces, macros), but it extends one owned taxonomy and current parser instead of adding a second one. Split extraction, scope, language-policy, and binding modules rather than adding another monolith to `symbol_resolver.rs`. |
| Dependency fit | Upstream 0.10 requires tree-sitter 0.24; Heddle requires 0.26. Cargo rejects both `links = "tree-sitter"` versions in one graph. The isolated prototype also had to pin `tree-sitter-rust` 0.23.2 because the current 0.24 grammar ABI is newer than tree-sitter 0.24 accepts. The upstream repository is archived. | Uses Heddle's pinned tree-sitter 0.26 and grammar versions, so one parse and taxonomy feed definitions, semantic hashes, imports, and references. |
| Incremental updates | Per-file graph construction and partial-path storage are good: only changed files need rebuilding. However, materializing resolved edges still requires re-stitching affected importers, and its SQLite-oriented storage/path identity does not replace Heddle's Merkle tree or reverse-dependency frontier. | Per-file extraction is O(new/changed blobs). Resolution is **frontier-bounded**, not strictly O(changed files): an export change must revisit direct/transitive importers. Store reverse imports so the cost is O(changed files + affected frontier); unchanged per-file artifacts are reused by hash. This lower bound applies to either substrate. |
| Clean content addressing | Stack-graph fragments carry file identities, file-scoped node IDs, and may consume `FILE_PATH`; their persisted bytes are repository-placement dependent. Normalizing them would require a Heddle-owned artifact layer anyway. | Yes for the persisted file artifact: no repository path, commit, resolved target, or allocator ID is encoded; canonical sorting makes identical `(source blob, grammar, extractor version)` produce identical bytes. Resolved edges are correctly kept in the state-scoped layer. |

This decision favors a smaller number of semantic truths and a clean Merkle boundary over Stack Graphs' stronger ready-made stitching machinery. It does **not** mean implementing all language semantics in the current 1,280-line file: #1273 should establish the artifact and extraction seam; resolution policy should move into per-language modules as it grows.

### Per-file artifact schema for #1273

Extend `SemanticFileNode` (and bump `EXTRACTOR_VERSION`) with deterministic source-local facts. The sketch is Rust-shaped but the durable form remains canonical named MessagePack like the existing semantic index nodes:

```rust
struct SemanticFileNodeV2 {
    format_version: u8,
    language: String,
    grammar_version: String,
    extractor_version: u32,
    source_blob: ContentHash,
    scaffold_hash: ContentHash,
    symbols: Vec<SymbolEntry>,       // existing definitions
    scopes: Vec<ScopeEntry>,         // deterministic preorder local IDs
    imports: Vec<ImportEntry>,       // source-local module specifiers/bindings
    occurrences: Vec<OccurrenceEntry>,
    semantic_digest: ContentHash,
}

struct ScopeEntry {
    local_id: u32,
    parent: Option<u32>,
    kind: ScopeKind,                 // module/type/function/block
    span: ByteSpan,                  // provenance only
}

struct ImportEntry {
    kind: ImportKind,                // use/import/reexport/dynamic
    module_specifier: String,        // source spelling, not a resolved repo path
    bindings: Vec<ImportBinding>,    // canonical source order
    scope: u32,
    span: ByteSpan,                  // provenance only
}

struct ImportBinding {
    imported: String,                // `greet`, `default`, or `*`
    local: String,                   // alias visible in this file
    namespace: SymbolNamespace,      // value/type/both
}

struct OccurrenceEntry {
    local_id: u32,                   // deterministic source-order ordinal
    role: OccurrenceRole,            // definition/reference/call/type-reference
    name: String,
    qualifier: Vec<String>,          // `crate::api`, `ns`, etc.; unresolved
    namespace: SymbolNamespace,
    scope: u32,
    span: ByteSpan,                  // provenance only
}
```

Canonicalization rules:

1. Local IDs come from deterministic preorder/source order, never parser pointer values or insertion order.
2. `scopes`, `imports`, bindings, and `occurrences` have specified sort keys; maps use lexicographic key order.
3. Repository-relative path, commit/state ID, package configuration, and resolved target are excluded. The same source blob under two paths therefore encodes to the same file-node bytes and Merkle-shares.
4. Storage identity includes the full encoded node, including spans. The reformat-stable `semantic_digest` follows the existing two-hash rule: it includes canonical scope/import/occurrence fields and excludes only spans.
5. `grammar_version` and `extractor_version` remain identity inputs; changing extraction or normalization forces a clean rebuild.

Resolved edges cannot live in that per-file artifact because binding depends on repository placement and the selected state. Persist them in a separate state-scoped delta:

```rust
struct BindingDelta {
    format_version: u8,
    parent: Option<ContentHash>,
    files: Vec<FileBindingDelta>,     // sorted by repo-relative path
}

struct FileBindingDelta {
    path: String,
    file_node: ContentHash,
    replace_edges: Vec<ResolvedEdge>, // complete replacement for this file
}

struct ResolvedEdge {
    source_occurrence: u32,
    target_path: String,
    target_file_node: ContentHash,
    target_definition: u32,
    kind: EdgeKind,                   // refers-to/calls/type-ref/imports
}
```

`replace_edges` avoids edge tombstone ambiguity. When a file's artifact or an export changes, walk the reverse-import index to obtain the affected frontier, recompute only those files' edge lists, and write one delta over the parent state. A query overlays deltas (with periodic compaction/checkpoints later); per-file facts remain independently Merkle-shareable.

### Layer C — query / operate
A memoized (Salsa-style) query engine: "refs of X at commit C", "callers of F", "structural diff C1→C2". Resolution is memoized + content-anchored, so repeat queries at a commit are O(1) and switching commits re-resolves only the diff frontier.

## Prior art to steal from (don't roll your own where you don't have to)
- **`tree-sitter-stack-graphs` (GitHub, archived)** — valuable model for per-file graph construction and path stitching, but #1272 rejected it as Heddle's substrate because of parser-version incompatibility, missing Rust rules, path-dependent graph artifacts, and upstream status.
- **Salsa / rust-analyzer** — the incremental query + invalidation-frontier engine (Layer C).
- **GitHub `semantic` (archived)** — tree-sitter → per-language symbol-table shape.

## Hard parts (be honest)
- **Per-language resolution is the cost center.** tree-sitter gives uniform *syntax* cheaply; *name binding* differs per language. Bound v1 to the languages that matter (Rust and TS/JS first), keep policies in per-language modules, and use Stack Graphs as design prior art.
- **Storage.** The compact semantic projection still needs packing/GC and a "don't index generated/vendored/binary" filter, but #1277 avoids the estimated 2–5×-source cost of full AST persistence.
- **The graph is NOT one clean Merkle tree.** It's a Merkle *syntax* layer + a *derived* semantic graph needing real incremental invalidation. Don't design as if it's all content-addressed.

## Strategic fit
- **Unblocks Tier-2 code-symbol search** (weft#451) — the symbol graph IS that index; the search spike explicitly deferred symbols to "a dedicated index."
- **Strengthens semantic merge** (`heddle-semantic/merge_driver`) — heddle's stated moat vs Mesa/jj. Structural diff + syntax-aware conflict resolution consume exactly this graph.
- **Agent-native VCS** — lets agents operate on code structurally (rename-symbol, find-callers, structural patch) rather than textually.

## Decision ledger for #987
1. **Full AST persistence — decided NO by #1277.** Parse on demand; persist the compact semantic projection.
2. **Layer B substrate — decided by #1272.** Extend a modularized Heddle-owned resolver and artifact; Stack Graphs is prior art only.
3. **Ingest hook point for the semantic projection** — parse-on-ingest (eager, in `importer.rs` alongside `reasoning_extract`) vs parse-on-first-query (lazy).
4. **Invalidation-frontier model — sketched by #1272.** Reverse imports select the frontier; resolved file edges are replacement records in a delta over the parent. #1274 owns implementation detail.
5. **Query API surface — open.** Decide what weft/CLI RPCs expose (feeds the search + nav surfaces); Salsa vs hand-rolled memoization.
6. **Language scope and storage policy — open.** Finalize v1 languages, not-index filters, and packing/GC for semantic artifacts.

## Phasing (recommended)
- **Phase A — decided: parse full ASTs on demand; retain the existing content-addressed semantic index as the persisted projection.**
- **Phase B — Heddle-owned imports/occurrences + modular cross-file resolution + reverse-dep incremental frontier.** #1273 can implement the per-file schema above.
- **Phase C — memoized query engine + time-travel queries + wire to search Tier-2 / semantic merge.**

## DoD for #1272

Prototype both substrates on Rust + TypeScript; record the Layer-B choice with effort, incrementality, and content-addressing evidence; and give #1273 a deterministic per-file imports/occurrences schema. This decision record captures the prototype findings and satisfies that decision gate; the throwaway prototype modules and rejected dependencies are intentionally not shipped. The remaining ledger items belong to their downstream issues.
