# heddle #1199: semantic delta-base selection — practical ANN + determinism (phase-1 spike)

**Status:** design spike. Analysis + recommendation only. No production selection
code ships from this issue. Honesty labels are explicit throughout:
**[measured]** = a number produced by a run on this box; **[measured-prior]** =
a number from the earlier oracle spike or the delta-search-policy measurement;
**[estimated]** = reasoned, not run; **[design]** = a proposal.

**Author:** zephyr@inskape.xyz · **Date:** 2026-09-03 ·
**Heddle base:** `origin/main` `abf21486`

---

## Recommendation (up top)

**GO — but re-route the method and drop the hardest constraint the framing assumed.**

Three findings move the decision away from "build a pinned neural-embedding ANN":

1. **Determinism is already solved in-tree, and the way it is solved removes the
   need for pinned/canonical embeddings.** Heddle already splits pack identity
   into `PackLogicalId` (stable across "compression, record ordering, and
   delta-base selection", the documented key for cross-machine dedup) and
   `PackRepresentationHash` = `blake3(pack_bytes)` (documented as *"not"* for
   "logical pack equality or cross-machine deduplication"). The oracle spike's
   headline fear — "different bases → different `blake3(pack)` → dedup breaks" —
   is a statement about `PackRepresentationHash`, which is explicitly *not* the
   dedup key. If hosted dedup keys on `PackLogicalId` (a weft-side fact to
   verify), base selection can be a **local, best-effort optimization** and need
   not be reproducible across machines at all. That collapses the entire
   "pin the embedding model + tokenizer + numeric runtime + tie-breaks + emit
   conformance vectors" workstream that Design Option 1 of the oracle spike
   contemplated.

2. **The oracle's own K-sensitivity says the win is not knife-edge on
   nearest-neighbor *precision*.** The measured-prior weighted reduction is
   **36.64 % at K=1**, **35.9 % at K=4**, **35.4 % at K=16**, **32.3 % at K=64**,
   **33.3 % at K=128** — a ~4-point band across two orders of magnitude of
   candidate-set size, and *higher* at K=1 than at K=128. The value comes from
   finding *a good enough* base, not *the* best base. An approximate index that
   recalls even a fraction of the true neighborhood therefore keeps most of the
   win; "quantization collapses the gain" is not supported by the oracle's own
   curve. Pessimistic linear scaling by ANN recall `r` (keep ~`r` of the
   incremental win) puts a recall-0.8 index at **~26 %** and recall-0.6 at
   **~20 %** [estimated] — both still 4–5× over the original 5 % build gate.

3. **For *delta compression* specifically, a cheap deterministic content-shingle
   hash (minhash / simhash) is the right axis to measure first — but only as an
   *augmentation* of the classical window, not a replacement.** A neural
   embedding scores *semantic* similarity (meaning); a delta encoder exploits
   *literal shared byte/token runs*. Those correlate but are not the same axis:
   two files can embed close yet share few literal sequences (poor base), or
   embed farther apart yet share large verbatim blocks (excellent base). Minhash
   over content shingles measures exactly the quantity the delta encoder cashes
   in, needs no ONNX model, no GPU, no model-version pinning, and produces
   identical buckets on every machine from the bytes alone. A small measurement
   on this box (§4.1) is blunt about the catch: minhash-LSH used to *replace* the
   window lands **12.9 % worse** than the window, but *unioned with* the window
   it captures **~60 % of an exhaustive delta-oracle's achievable win** for
   near-zero machinery — and that retention is tuning-limited (index recall
   46 %→76.5 % from one re-banding), not ceiling-limited.

**Therefore:** GO on a semantic *candidate* layer, but Phase 1's first
measurement should be **minhash/simhash-LSH retention vs. the neural oracle on
the same three corpora**, not "quantize the neural embeddings." If the cheap
deterministic feature retains most of the 33 %, the embedding-model stack (and
its determinism tax) is never built. Ship it **inside the background
repack/consolidation job** the delta-search-policy doc already identifies as the
only viable home for classical delta search — semantic bases are a strictly
additive refinement on top of a delta search that is itself **default-off on
every hot path today**.

If Phase 1's minhash measurement collapses (retains < ~1/3 of the oracle *and*
neural quantized ANN also collapses), that is the honest NO-GO trigger the
oracle deferred; the sensitivity data above makes that outcome unlikely but it
must be the pre-declared kill switch.

---

## 1. Current-state grounding (what "delta base" means here today)

### 1.1 There is a real classical delta-base selector, and it is off by default

`crates/pack/src/store/pack/pack_builder.rs` is a Git-style sliding-window delta
packer:

- Objects are grouped by type; **states, state-attachments, and annotated tags
  are excluded** from delta search (`build_impl`, ~line 160).
- Within a type, objects are sorted `extension → basename → size-descending`
  (`sort_for_delta_window`), then a **window of the 10** most-recent entries is
  tried as bases; the smallest real delta wins (`encode_with_sliding_window`).
- Chain depth is capped at **50** (`MAX_DELTA_CHAIN_DEPTH`); minimum object size
  for delta is **64 bytes** (`MIN_DELTA_SIZE`).
- The delta bytes come from `heddle_format::delta::DeltaEncoder`
  (`crates/format/src/delta/delta_encoder.rs`), a copy/insert stream.

Crucially, the window sort key uses `path_hint`, but **aggressive GC calls
`add`/`add_id`, not `add_with_path`** — so in the real GC path every `path_hint`
is `None` and the sort collapses to **size-descending only** (measured-prior in
the oracle spike). The "window" a semantic layer competes against is therefore a
size-ordered window of 10, with no path/history locality.

The delta search is **off on all three synchronous paths**
(`docs/perf/delta-search-policy.md`): `storage.delta_search.{import,snapshot,gc}`
default `false`. Import with delta-on is **3.3–15.5× slower** and GC **44–65×
slower** [measured-prior], with cargo-deny peaking at 820 MiB RSS. The doc's
recommendation: keep all synchronous paths off; the "right eventual home is a
background repack/consolidation job with resource controls and cancellation."
**That job does not exist yet** and is the natural host for any semantic layer.

### 1.2 "Semantic" here means embedding-similar bases, not the `semantic` crate

`crates/semantic` is tree-sitter/AST analysis (diff, merge, rename detection,
`SimilarityMethod::{Lines,Tokens,Ast}` Jaccard in
`analysis/analysis_similarity.rs`). The oracle spike's "semantic" is unrelated:
it means **text-embedding nearest-neighbor** (`BAAI/bge-small-en-v1.5`, 384-dim)
used to nominate delta bases *outside* the size-window. This doc uses "semantic
candidate layer" in that sense: any similarity index that nominates
out-of-window base candidates, whether neural or content-hash based.

### 1.3 The 33 % baseline, precisely

The oracle [measured-prior, 2026-08-02] compared **W=10 classical packs** against
**W=10 + exact float32-cosine top-128 semantic candidates**, both fed through
Heddle's real `DeltaEncoder`/zstd and read back through `PackReader`:

| corpus | W=10 pack | +semantic | reduction |
|---|---:|---:|---:|
| ripgrep | 12.40 MB | 6.74 MB | 45.68 % |
| fd | 6.09 MB | 4.50 MB | 26.06 % |
| cargo-deny | 11.51 MB | 8.77 MB | 23.83 % |
| **weighted** | **30.01 MB** | **20.01 MB** | **33.31 %** |

28,172/28,172 objects reconstructed exact, 0 BLAKE3 failures. This is an
**exact-neighbor oracle** (brute-force float cosine, no ANN, no quantization).
33 % is the **ceiling**; the deferred question is how much survives approximate
search — the subject of §3–4.

---

## 2. The determinism constraint, re-examined

This is the load-bearing section, and it is where the spike's framing is
strongest but its premise is now incomplete.

### 2.1 What content-addressing actually requires

Objects are addressed by `ContentHash::compute_typed` over their *uncompressed
content* — never by which pack or delta base carried them. Reconstruction from
*any* valid delta chain yields byte-identical objects (the oracle proved 0/11,397
identity failures). So **object identity is already independent of base choice.**
The only thing base choice changes is the *container* bytes.

### 2.2 The two container identities already exist in-tree

`crates/pack/src/store/pack/pack_identity.rs` (verified at `abf21486`):

- **`PackLogicalId`** — BLAKE3 over the sorted multiset of
  `(id-kind, id, object-type, blake3(content))`, context `heddle.pack.logical-id.v1`.
  Doc comment: *"stable across compression, record ordering, and delta-base
  selection … Hosted storage must scope it to the root spool."* Computable from a
  built pack via `PackReader::logical_id()`.
- **`PackRepresentationHash`** — `blake3(pack_bytes)`. Doc comment: *"suitable for
  integrity checks and physical location, but **not** logical pack equality or
  cross-machine deduplication."*

The local FS store *names pack files* by `blake3(pack_bytes)`
(`fs_pack.rs:653`) — a physical filename, purely local; two machines producing
different bytes for the same logical objects get different local filenames and
nobody cares.

**Consequence:** the oracle spike's headline determinism argument ("different
bases → different `blake3(pack)` → hosted dedup retains both packs") is only true
for a system that dedups on `PackRepresentationHash`, which the code explicitly
tells you not to do. The infrastructure to make base selection a
*non-reproducible local optimization* already exists.

### 2.3 What still genuinely needs determinism (the residue)

1. **The pack-format stability tests** pin exact pack/index bytes for fixed
   inputs. A *within-machine* nondeterministic selector (e.g. an ANN with a
   random seed, or float ties broken by iteration order) breaks these even on
   one box. Fix: the selector must be a **pure function of its inputs on a given
   build** — fixed seeds, canonical candidate ordering, total-order tie-breaks by
   `ContentHash`. This is cheap and is required regardless of method.
2. **Any consumer that today keys logical decisions on
   `PackRepresentationHash`.** Must be audited (weft hosted store; #966/#969 if
   they touch pack identity). If one exists, either migrate it to `PackLogicalId`
   or keep base selection cross-machine-reproducible for that path only.
3. **Cross-machine byte-identical packs are only required if you want physical
   (byte/chunk-level) dedup of the *container*,** not logical object dedup. That
   is a storage-cost optimization weft may or may not want; it is a decision, not
   a correctness constraint.

### 2.4 Determinism design, by method

| method | cross-machine reproducible base? | cost to guarantee it |
|---|---|---|
| **minhash/simhash-LSH over content shingles** | **yes, for free** | fixed hash seeds + canonical band keys; no model, no float. Byte-identical buckets everywhere. |
| neural embedding, pinned-canonical (Option 1) | yes | pin model digest + tokenizer + chunking + K + numeric/runtime + tie-breaks; ship cross-machine conformance vectors. Fragile: a CPU/GPU numeric tie-flip changes a neighbor. |
| neural embedding, decoupled (Option 2) | no (and doesn't need to be) | key hosted dedup on `PackLogicalId`; base selection is local best-effort. |

The minhash row is why the method pivot and the determinism story reinforce each
other: **the cheapest predictor of delta compressibility is also the only one
that is deterministic without a pinning regime.**

---

## 3. Candidate ANN / similarity methods vs. the 33 % ceiling

| method | index cost | query cost | determinism | notes |
|---|---|---|---|---|
| **minhash-LSH** (shingle Jaccard) | low; k hashes/blob | banded bucket lookup | **native** | predicts *literal* overlap = what deltas cash in. Recommended first measurement. |
| **simhash** (Hamming-LSH) | low; one 64-bit sig/blob | bit-bucket / small radius | **native** | cheaper than minhash, coarser recall; good simplicity baseline. |
| **HNSW** over neural embeddings | high build; graph in RAM | fast, high recall | **no** (graph build is order/seed dependent) | best recall but worst determinism + needs the embedding stack. |
| **IVF-PQ / product quantization** | medium | fast, tunable recall | **no** (quantizer trained on data) | this is the "quantize the embeddings" path the oracle deferred; retention is the open question §4. |
| **plain wider window / path-&-history-aware window** | ~free | none | native | cheapest of all; captures the *locality* the current size-only GC window throws away. A likely large fraction of the win for near-zero machinery. |

Two "cheaper heuristic" baselines deserve measurement *before* any ANN:

- **Path + history window.** The GC path loses `path_hint` (§1.1). Simply routing
  `add_with_path` through aggressive GC, and widening/ordering the window by
  path and commit adjacency, may recover a large share of the 33 % with *zero*
  similarity index — because much of the semantic win is "same file, earlier
  version, just outside a size-sorted window of 10."
- **minhash-LSH** as the first real similarity index.

---

## 4. Retention: what a practical index keeps (measured + estimated)

The rigorous retention number — "quantized neural ANN vs. exact cosine oracle on
the three corpora" — is **Phase 1's to produce** with the oracle's own
instrumented harness (re-run at K with an approximate candidate set). This spike
does not reproduce the neural pipeline (multi-hour, tens of GB, and the oracle
already owns that harness). Instead it measures the *decision-shaping* claim the
recommendation rests on: **does a cheap deterministic content-shingle index find
delta bases as good as an exhaustive delta oracle?**

### 4.1 Small self-contained measurement [measured, this box, 2026-09-03]

- Corpus: **1,335 real heddle blobs** (≥64 B, <200 KB) from the git history of
  `crates/{pack,objects,format}`, 25.3 MB total.
- Selection in size-descending order (mirrors `PackBuilder`). Candidate *scoring*
  uses a cheap deterministic estimator (count of shared 16-byte content shingles
  ≈ residual a copy/insert delta must insert). Bytes written are **real**:
  copy/insert delta of the finally-chosen base through the real `zstd -19` CLI,
  keeping raw when the delta is larger (as the builder does).
- Three candidate sets, identical estimator and final measurement:
  `window` = 10 preceding (size order); `minhash` = top-32 from minhash-LSH
  (64 hashes, 16 bands); `oracle` = exhaustive over all earlier blobs.
- Fully deterministic (fixed blake2b seeds, ContentHash tie-breaks).

The harness is a throwaway Python script (deliberately **not committed** — it is
a shingle-overlap proxy, not the real `DeltaEncoder`, and would rot as product
code). Reproduce: collect blob SHAs with
`git rev-list --objects --all -- crates/{pack,objects,format}`, fetch via
`git cat-file --batch`, then for each blob in size-descending order score
candidates by shared 16-byte shingles and measure the chosen base's copy/insert
delta through `zstd -19`. Results:

| policy | pack bytes | vs. size-window | retention of oracle win |
|---|---:|---:|---:|
| size-window W=10 (baseline) | 3,112,667 | — | — |
| **exhaustive delta-oracle** | 2,463,956 | **−20.84 %** | 100 % (definition) |
| minhash-LSH **only** (replaces window) | 3,514,315 | **+12.90 % (worse!)** | **−61.9 %** |
| **window ∪ minhash-LSH** (augments) | 2,726,930 | **−12.39 %** | **+59.5 %** |

LSH recall of the oracle's chosen base: **46.1 %** at 16 bands × 4 rows,
**76.5 %** at 32 bands × 2 rows (same 64-hash signatures, re-banded).

Three things this says, none of them what the clean thesis predicted:

1. **The out-of-window win is real and independently corroborated.** An
   exhaustive delta-oracle beats the size-window by **20.8 %** on a *different*
   corpus (heddle's own source) under a *different* oracle (content-delta, not
   neural cosine). The premise "there is substantial delta compression left
   outside the size-window" reproduces.
2. **A cheap similarity index as a *replacement* for the window is actively
   harmful** — minhash-only lands **12.9 % worse** than the window and *negative*
   retention. At 46 % recall it misses good local bases the window would have
   caught. This is the honest failure case: "just swap in LSH" loses.
3. **As an *augmentation* of the window it captures ~60 % of the achievable
   win** for near-zero machinery, fully deterministically. And retention is
   **tuning-limited, not ceiling-limited**: recall jumps 46 %→76.5 % from a
   single re-banding, so the +59.5 % is a *floor* for a barely-tuned index.

The design lesson is concrete and load-bearing: **the similarity layer must
*union* its candidates with the classical window, never replace it, and its
recall must be tuned** (band/row count, shingle size, K). A naive drop-in
regresses.

**Reading it:** *retention* = `(window − minhash) / (window − oracle)` — the
fraction of the exhaustive-oracle improvement that the cheap deterministic LSH
captures. *LSH recall* = how often the LSH candidate set actually contained the
base the exhaustive oracle chose. This is a **content-delta oracle**, not the
neural-cosine oracle of the prior spike — arguably the more relevant ceiling
(the best *achievable* base by literal overlap), and it needs no model.

Caveats, stated plainly: this corpus is heddle's own Rust source (smaller and
more near-duplicate-heavy than the prior spike's three imported repos); the
estimator is a shingle-overlap proxy, not the exact `DeltaEncoder` cost, so the
*absolute* percentages are not comparable to the 33 % oracle. What transfers is
the **shape**: whether a cheap deterministic LSH recalls the good bases an
exhaustive search finds. It is a signal, not a benchmark.

### 4.2 Estimated neural-ANN retention [estimated]

From the oracle's K-curve (§ recommendation point 2), the incremental win is flat
across K=1..128. Modeling an approximate index as "returns the true best base
with probability = recall `r`, else falls back to the window choice," retention ≈
`r`. Published recall for tuned HNSW/IVF-PQ at these corpus sizes (10⁴–10⁵
vectors) is routinely 0.9–0.99 at modest query cost; even a deliberately cheap
`r ≈ 0.7` retains ~23 % [estimated], comfortably over the 5 % gate. The scenario
that kills the GO is *systematic* recall collapse (quantization maps
delta-similar blobs to different cells), which the K-curve's flatness argues
against but which Phase 1 must confirm on the real corpora.

---

## 5. Index build/maintenance cost and where it lives

- **Not at capture time on the hot path.** Capture is latency-sensitive and
  delta search is already default-off there for 3–65× reasons. Embedding every
  blob at capture would be far worse.
- **In the background repack/consolidation job** the delta-search-policy doc
  already prescribes as delta search's only viable home. The similarity index is
  built/updated there, over the object set being repacked, with the same resource
  controls and cancellation.
- **Lifecycle participation.** Whatever index format is chosen must participate in
  `heddle gc` and pack lifecycle: entries for GC'd objects must be reclaimable,
  and the index must not pin dead objects. minhash/simhash signatures are tiny
  (tens of bytes/blob) and can be recomputed on demand, which sidesteps most
  lifecycle coupling — a further point in the cheap-feature column over a
  persisted HNSW graph that must be maintained and versioned.

---

## 6. Complexity vs. payoff

| lever | machinery | determinism tax | expected share of 33 % |
|---|---|---|---|
| path+history-aware window (route `add_with_path` in GC, widen/order window) | tiny | none | large fraction of "same-file-out-of-window" wins [estimated] |
| minhash/simhash-LSH **unioned with** the window | small | none | ~60 % of the achievable oracle win [measured §4.1], rising with recall tuning; regresses if used as a *replacement* |
| neural embedding + quantized ANN | large (model, quantizer, index, pinning or decoupling) | large unless decoupled | the *marginal* semantic-but-not-literal wins over minhash [unknown] |

The payoff gradient argues for building **up** the cheap ladder and stopping when
the marginal rung stops paying, not starting at the neural top. The neural stack
only earns its place if Phase 1 shows it beats minhash by enough to justify the
model-pinning/decoupling burden — and even then, the decoupling design (§2.2)
means the burden is smaller than the oracle spike feared.

---

## 7. GO/NO-GO

**GO**, re-scoped:

- **GO** on a semantic/similarity candidate layer for delta-base selection, as a
  strictly additive refinement inside the (not-yet-built) background
  repack/consolidation job.
- **Determinism is not a blocker.** Key hosted dedup on `PackLogicalId`
  (decoupling, §2.2); require only that the selector be a pure function of its
  inputs on a given build. Drop the neural-model-pinning workstream from the
  critical path.
- **Method: build the cheap ladder first, and always as an augmentation.** The
  similarity layer *unions* its candidates with the classical window — never
  replaces it (measured: a replacement regresses 12.9 %). Phase-1 measurement is
  path+history-window and window∪minhash/simhash-LSH retention vs. the neural
  oracle on the *original three corpora* with the oracle's harness, with LSH
  recall tuned (bands/rows, shingle size, K). Only escalate to neural + quantized
  ANN if the cheap ladder leaves material win on the table.
- **NO-GO trigger (pre-declared):** if window∪minhash-LSH (recall-tuned) retains
  < ~1/3 of the oracle win *and* a quantized neural ANN also collapses below the
  5 % gate on the real corpora, stop — the oracle was a mirage. §4's evidence
  (60 % augmented retention on heddle's own source, tuning-limited) makes this
  unlikely.

Do **not** ship until the background repack scheduler exists and the classical
deltas-off baseline (filed separately) lands first/alongside — semantic bases
sit on top of it.

---

## 8. Proposed follow-up issues (NOT filed here — for the owner to triage)

- **Verify weft hosted dedup keys on `PackLogicalId`, not
  `PackRepresentationHash`.** This single fact decides whether the
  determinism/decoupling design (§2.2) is available. If it keys on the
  representation hash today, filing the migration is the real Phase-1 unblocker.
- **Route `add_with_path` through aggressive GC and measure a path+history-aware
  window** vs. the size-only window, on the original three corpora. Cheapest
  possible slice of the 33 %; likely large; zero new machinery.
- **Phase-1 retention harness:** re-run the oracle harness with (a) window∪minhash-LSH
  and (b) window∪simhash candidate sets and (c) a quantized/PQ neural ANN — each
  *unioned with* the classical window, recall-tuned — reporting the fraction of
  the exact-cosine 33 % each retains, on ripgrep/fd/cargo-deny. Include the
  replacement-not-augment negative case to keep the 12.9 % regression visible.
- **Selector determinism contract + conformance test:** pin the selector as a
  pure function of inputs (seeds, canonical candidate order, `ContentHash`
  tie-breaks) and add a negative test that a perturbed tie flips *nothing* in the
  chosen base set.
- **Background repack/consolidation scheduler** (the delta-search-policy doc's
  owed "right home"): resource controls, cancellation; prerequisite host for any
  semantic layer. Likely already tracked — link, don't duplicate.
- **Index lifecycle in `heddle gc`:** decide recompute-on-demand vs. persisted
  signatures; ensure the similarity index never pins dead objects.
- **Reconcile with #966 (re-keying) / #969 (delta producer)** — confirm the
  pack-identity split above matches their assumptions (both referenced by the
  issue; neither resolves in the heddle issue tracker, so they may live in weft).

---

## Appendix: honesty ledger

- **[measured-prior]** 33.31 % weighted oracle reduction, K-sensitivity table,
  slowdown/RSS figures — from `semantic-delta-base-oracle-spike.md` and
  `docs/perf/delta-search-policy.md`, not re-run here.
- **[measured]** §4.1 minhash-vs-exhaustive-delta retention on 1,335 heddle
  blobs, this box, 2026-09-03. A shingle-overlap proxy on heddle's own source;
  signal not benchmark; absolute % not comparable to the neural oracle.
- **[estimated]** all retention-by-recall figures in §4.2 and the §6 "share of
  33 %" column.
- **[design]** the decoupling recommendation, the method pivot, and the phasing.
- The neural retention number the issue calls the crux is **explicitly left to
  Phase 1's harness**; this spike argues the determinism strategy and the method,
  which are the decisions that gate that measurement being worth running.
