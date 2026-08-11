# Unified adopt / export-perf design spike

> **Status:** spike (doc-only) — reference design for a real epic. No production
> code lands with this document. Tracks HeddleCo/heddle#1321.
>
> **Scope:** unify (1) #564 de-lossy / mirror-drop **correctness**, (2) O(delta)
> **fast export** after the mirror is gone, and (3) weft adopt-perf
> (weft#1255 quadratic serialization, weft#1256 `path_hint` delta-grouping)
> into **one shared primitive** — a delta enumerator over adjacent trees.
>
> **Grounding:** every concrete `path:line` below was grepped against this
> checkout (`task/1321-docs-spike-unified-adopt-export-perf-des` @
> `a08e9680`, off `origin/main`, verified 2026-08-11). Where a capability is
> planned-not-shipped it is labeled per AGENTS.md (`shipped` /
> `foundation in place` / `planned`). Unverifiable anchors are marked
> **unverified** rather than invented.
>
> **Hard constraint — do not contradict #593:** non-UTF8 identities are closed
> by making `Principal.name` / `Principal.email` into `Vec<u8>`
> (truth-by-default). An override / enum / "keep String + raw actor-line"
> design was considered and **superseded**. This spike does not re-open that
> decision.

---

## 1. Problem & context

Byte-identical git reconstruction from Heddle `State` is the load-bearing
correctness goal of epic **#564**: once export mints git objects from native
fields (plus a residual floor for the irreducible remainder), the persistent
Bridge Mirror at `.heddle/git` can die (ADR-0042, #568 / PR #1313). That path
unlocks bidirectional forge sync without a secret second git repository.

Two costs remain after the correctness plan is taken seriously:

**(a) Lossless residual.** Some git objects are not injective into any finite
semantic model. Even after the structured-field closure (#565 already shipped
committer / tz / `raw_message` / `extra_headers`; #593 / #606 / S7 / #575 /
notes still open), a small verbatim-bytes floor is irreducible. Today that
floor is the Raw Git Object Residual store under `.heddle/git-residuals/`
(`crates/git-projection/src/git_residual.rs:1-39,56-57`) — a durable sidecar
that ADR-0042 already treats as the long-term exception path, not the normal
source of truth.

**(b) Reconstruction is O(whole reachable history).** The mirror was also a
secret **incremental cache**: it held prior OIDs and bytes so a later push
only paid for new objects. Dropping it without replacing the cache property
re-pays full-history CPU on every export. That is the same defect
weft#1255 measured on adopt — each commit's work scales with the whole tree
rather than the delta it introduced — and the same defect visible in heddle's
memo-less `export_tree` recursion
(`crates/git-projection/src/git_export.rs:232-306`, recursive call at `:274`).

**Goal:** minimal-residual lossless export **+** O(delta) fast export, as
**one coherent direction** — not three parallel fights (#564 correctness,
#568/#1313 mirror-drop speed, weft#1255/#1256 adopt encoding).

### 1.1 What is already shipped (do not re-plan)

| Piece | Status in this checkout | Anchor |
|---|---|---|
| Commit fidelity fields on `State` (committer, tz offsets, `raw_message`, `extra_headers`, `git_lossy`) | **Shipped** (#565) | `crates/object-model/src/object/state_core.rs:254-329` |
| Fidelity fields in content hash | **Shipped** | `state_core.rs:653-740` (`update_hash` / `update_hash_fidelity`) |
| Byte-exact `reconstruct_commit_bytes` + fidelity mint path | **Shipped** (#566/#567) | `crates/git-projection/src/git_reconstruct.rs:67+`; mint routing `git_export.rs:141-167` |
| Raw Git Object Residual store | **Foundation in place** | `crates/git-projection/src/git_residual.rs` |
| Git Projection Mapping (`StateId ↔ git OID`) | **Shipped** (local cache + notes identity) | `crates/git-projection/src/git_mapping.rs:19-28` (`MappingEntry`), `:61-91` (cache path) |
| ADR-0042 endgame: reconstruct-from-state **+** residuals | **Accepted** | `docs/adr/0042-retire-persistent-bridge-mirror.md` |
| Hard OID-equality gate on checkout materialization | **Shipped** (export-time / materialize-time) | `git_core.rs:3500-3504` comment block on `materialize_checkout_closure_from_state` |

### 1.2 What this spike adds

1. The **import-time round-trip gate** as the correctness backstop that the
   enumerated residual list cannot provide alone (§3).
2. An honest characterization of the **residual floor** once structured
   gaps close (§4).
3. The **O(delta) layered export architecture** that replaces the mirror's
   cache value without keeping the mirror's bytes (§5).
4. The **unification** with weft adopt-perf via one delta enumerator (§6).
5. Failure modes (§7) and phasing that batches the #564 format bump once (§8).

---

## 2. Correctness / lossless — DEFER to the decided #564 decomposition

**Do not re-litigate.** Epic #564 (re-scoped 2026-08-09) already enumerates the
remaining loss cases and assigns each a sub-issue. Steps 1–3 are done and
CI-gated (#565 / #566 / #567 / #562). ADR-0042 reframed the endgame as
"reconstruct-from-state **+ Raw Git Object Residuals**" — mirror elimination
requires *residual completeness*, not universal reconstructibility.

This section **cites** the decided closure path for each remaining case.
These become **reconstructable-from-state** (not residual) once their
sub-issue lands. They are **not** residual-floor candidates.

### 2.1 Non-UTF8 identities → #593 (`Principal` becomes `Vec<u8>`)

**DECIDED — truth-by-default. Explicitly NOT an override / enum.**

Today `Principal.name` / `email` are `String`
(`crates/object-model/src/object/state_attribution.rs:8-12`). The content
hash already folds them as raw bytes via `.as_bytes()` with NUL framing
(`state_core.rs:672-675` author; `:734-737` committer). The 2026-06-07 audit
resolved blast-radius:

- Signing is byte-agnostic (signs `compute_hash`, which already folds the
  principal as raw bytes).
- The JSON `--output` contract is **decoupled**: user-facing structs such as
  `PrincipalInfo` / `BlamePrincipalSchema` stay `String`
  (`crates/cli/src/cli/commands/show.rs:45+`, `blame.rs:29+`,
  `crates/cli-contract/src/cli/commands/schemas.rs:972+`) and are populated
  via `from_utf8_lossy` at the boundary.
- ~51 bounded read-sites adapt via `from_utf8_lossy` / `bstr::ByteSlice`.

**State private note — reject the override idea.** "Keep `String` + store a
raw actor-line override for the non-UTF8 case" was considered and
**superseded by #593**. An override collides two distinct identity byte
sequences onto one display string (U+FFFD replacement), then needs a second
channel to disambiguate — exactly the class of silent-identity-collision
`Vec<u8>` avoids by construction. This spike will not re-open that design.

`state_core.rs:289-291` already documents the gap inline:
> non-UTF8 author/committer identity *names* are not yet byte-preserved —
> `Principal` is still `String`; see #564.

### 2.2 Nonstandard tree modes → #606

**DECIDED — accept + preserve the raw mode** (maintainer, 2026-06-09).

Today import **normalizes** legacy modes: `100664 → 100644`, `100775 → 100755`
(`crates/ingest/src/importer.rs:1773-1810` asserts the normalization). That is
correct for git's write-canonicalization story and wrong for #564's
byte-identical reconstruction goal: a tree that carried `100664` on disk
will mint a different SHA on export. #606 closes this by storing the
verbatim mode sparsely (only when nonstandard), same pattern as
`raw_message` / `extra_headers`. Truly malformed modes remain hard errors.

This is the textbook case the import-time round-trip gate (§3) is meant to
catch: the drift is silent at export today because no residual is captured
for a "successfully normalized" tree.

### 2.3 Backslash / control tree-entry names → S7 (correctness gap)

**S7 is a named gap in the #564 2026-08-09 decomposition, not a filed
sub-issue number in this checkout.** Treat "S7" as the epic's label for
tree-entry-name fidelity.

Today unrepresentable names are **silently dropped or converted** at import
via `classify_git_tree_name`
(`crates/objects/src/util/git_tree_name.rs:25-50`): path separators
(`/`, `\`), `.` / `..`, control bytes, and empty names become
`GitTreeNameLossyAction::Dropped`; invalid UTF-8 that becomes representable
after replacement becomes `Converted`. The import path honors `Dropped` by
omitting the entry and marking the commit lossy
(`crates/ingest/src/importer.rs:840-848`). That is a **correctness gap** for
byte-identical export: the original tree bytes are gone unless a residual
was captured for the whole object.

S7's decided direction (per #564 decomposition): close the gap with a
**structured model field** (represent the raw name bytes / a fidelity
escape for names the POSIX/Heddle path model rejects), not by widening the
residual floor to every tree that ever had a weird name. Exact field shape
is S7's problem; this spike only records that it is **in the structured
closure**, not the residual floor.

### 2.4 Annotated tags → #575

**DECIDED — first-class content-addressed objects, not sidecars.**

The earlier `marker-tags/<name>.bin` sidecar approach dripped fidelity gaps
(stale cleanup, tag-of-tag, backend parity, sync propagation, ingest parity).
#575 stores the git tag object in the same content-addressed store as
states/blobs so it inherits sync, both import paths, and tag-of-tag for free.
A marker references the tag-object hash; reconstruction reads the object
from the store.

### 2.5 Notes → in-store

Git notes at `refs/notes/heddle` carry Heddle state metadata for the public
mirror (`crates/git-projection/src/git_notes.rs:1-28`). The #564 endgame and
the #1313 residual-completeness work treat note closures as part of the
store-backed projection (installable without `.heddle/git`), not as a
parallel sidecar. Exact packing of "notes in-store" is sequenced with the
#564 format bump (P2, §8); this spike does not invent a second notes path.

### 2.6 Framing

| Loss case | Closure | Residual? |
|---|---|---|
| Commit fields (committer, tz, message, headers, gpgsig position) | **Shipped** (#565–#567) | No (when `!git_lossy`) |
| Non-UTF8 identities | **#593** `Vec<u8>` | No (after #593) |
| Nonstandard tree modes (`100664`, …) | **#606** raw mode field | No (after #606) |
| Backslash / control tree names | **S7** structured name fidelity | No (after S7) |
| Annotated tags | **#575** first-class objects | No (after #575) |
| Notes | **in-store** with #564 format bump | No (after in-store notes) |
| Pathological non-canonical object grammar | **irreducible floor** (§4) | **Yes — tiny, in-store** |

---

## 3. The import-time round-trip gate

### 3.1 The gap an enumerated list cannot close

Even a complete structured-field checklist cannot **prove** the class is
closed. A future git quirk (or a regression in an existing serializer) will
mint a wrong SHA at export with no residual captured — exactly the #606
`100664` shape today: import "succeeds," export drifts, nobody noticed
because nothing compared OIDs at the moment the bytes were still in hand.

### 3.2 Recommendation

**At import, reconstruct each object immediately and compare OIDs.**

```
for each imported git object O with oid OID_src:
    state_or_tree ← semantic_import(O)
    OID_round    ← reconstruct_and_hash(state_or_tree)
    if OID_round == OID_src:
        proven byte-faithful; no residual
    else:
        capture the smallest divergent unit as residual (fail-closed)
        mark the owning state git_lossy / residual-backed as appropriate
```

Properties:

1. **Catches divergence at IMPORT**, while the source bytes are still
   available — not at a later export when the only copy is gone.
2. **Fail-closed on inequality** for the residual path: never claim
   reconstructable-from-state for an object whose mint disagrees with the
   source OID.
3. **Closes the "enumerated list" soundness gap**: the gate is the proof;
   the structured fields are the *optimization* that keeps the residual
   floor near zero.
4. **Composes with the existing materialize-time OID assertion**
   (`git_core.rs:3500-3504`): import-time gate prevents minting a wrong
   mapping; materialize-time gate defends against later reconstruction
   drift. Both stay.

### 3.3 Smallest divergent unit

Prefer residualizing the **smallest** object that diverges:

- Tree mode / name mismatch → residualize that **tree** (not the whole
  commit closure, and never a blob — blobs are content-addressed identically
  once the bytes are stored; see §4).
- Commit header / identity mismatch after #593 → residualize that **commit**
  until the structured field lands; after #593 the gate should flip to
  equal.
- Annotated tag mismatch → residualize that **tag** until #575 lands.

Blob residuals are never required for fidelity of blob *content* (the
content-addressed blob store already holds the bytes). A "blob residual"
would only arise from a framing bug in the git object header; treat that as
a serializer bug, not a floor entry.

### 3.4 Cost

One reconstruct-and-hash per imported commit/tree/tag. That is O(import)
work already dominated by object I/O on large repos; it is paid once, not
on every later export. P0 measures the wall-clock delta on the corpus (§4.3)
before arguing about skipping the gate under a flag.

---

## 4. The residual FLOOR (honest, bounded)

### 4.1 Why a floor remains

Git's object grammar is **non-canonical**: two byte sequences can be
semantically equivalent under git's readers and still hash differently, and
pathological objects exist for which `parse → reserialize ≠ original`. No
injective semantic model covers the full object set. Therefore a
**verbatim-bytes floor is irreducible**.

### 4.2 What the floor is (and is not)

| Property | Floor | Bridge Mirror (dies) |
|---|---|---|
| Location | **In the content-addressed store** (hash-covered, verified-by-OID, synced) | Separate bare repo at `.heddle/git` |
| Granularity | Field-level / object-level overrides for **divergent commits / trees / tags only** | Whole reachable object closure |
| Blobs | **Never in the floor** | Copied eagerly |
| Typical repos | ≈ 0 objects after §2 structured closure | Always full history |
| Adversarial bound | Commit + tree (+ tag) bytes of divergent objects only; **never worse than the mirror** | Full pack |
| Trust | OID-verified on put (`git_residual.rs:121-157` pattern) | Implicit "whatever is in `.heddle/git`" |

**The SIDECAR dies; a tiny in-store floor remains.**

Today's `ResidualStore` under `.heddle/git-residuals/`
(`git_residual.rs:12-17,56-57`) is the **foundation** for this floor, but it
is still a parallel on-disk layout outside the main object store. The P2
format bump (§8) migrates residual bodies into the content-addressed store
(same transfer path as every other object — the #575 argument applied to the
whole floor) and deletes the need for a persistent Bridge Mirror *and* for a
long-lived parallel residual directory as a second source of truth. Migration
detail is an impl concern; the invariant is:

> residual bytes are content-addressed, OID-verified, and travel with the
> repository — not a machine-local secret.

### 4.3 P0 corpus measurement (price the floor as a number)

Before claiming "≈ 0," measure. **P0 (read-only):** on a fixed corpus
(linux, git, curl, ghostty — and any internal adversarial fixtures), run
import-with-round-trip-gate (§3) and report:

| Metric | Why |
|---|---|
| `# objects with OID_round ≠ OID_src` before structured fixes | Upper bound on today's floor |
| Same metric with #593 / #606 / S7 / #575 simulated or landed | Floor after structured closure |
| Residual bytes / source-pack bytes | Storage multiplier |
| Breakdown by object type (commit / tree / tag) | Confirms blobs stay out |

Ghostty is the useful mid-size pilot: #1313 measured its eager mirror at
**16,872 KiB** on an 800-empty-commit fixture path and ~77 MB pack on the
weft pilot; the durable `ContentHash → OID` map (§5) is budgeted at
~40–64 B/entry → **~5–8 MB for ghostty-scale object counts**, not 16.9 MB of
mirrored bytes.

---

## 5. O(delta) fast export (the new speed architecture)

### 5.1 Key insight

**The mirror's cache value was its OIDs, not its bytes.**

On a push the remote already holds prior objects. Bytes are needed only for
the **delta** (objects the remote lacks). The durable thing worth keeping
after mirror-drop is a `ContentHash → git-OID` **map** (~40–64 B/entry), not
a second copy of every object body.

Today heddle already has a `StateId → git-OID` projection mapping
(`git_mapping.rs:19-28`) and notes-backed identity recovery. What it lacks
is a **tree/blob ContentHash → OID** memo that lets `export_tree` skip
re-walking unchanged subtrees across commits and across export runs.

### 5.2 Layered architecture

#### Layer 0 — per-run memo (kills the quadratic re-pay)

Thread a `HashMap<ContentHash, ObjectId>` through `export_tree` (and any
materialize helper that walks the same trees). On a cache hit, return the
OID without re-reading the tree or re-writing blobs.

- **Complexity:** O(unique objects in the exported closure), not
  O(Σ tree-size over commits).
- **Kills:** the #1313-class re-pay where every commit re-exports its full
  tree through memo-less recursion (`git_export.rs:232-306`).
- **Proposed name** for the helper that seeds / consumes the map across a
  multi-state export walk: `materialize_cached_mappings` — **not present in
  this checkout**; named here as the Layer-0 seam. Today's closest cousin is
  `materialize_checkout_closure_from_state` (`git_core.rs:3516+`), which
  walks states and reconstructs but does not memoize `export_tree` by
  `ContentHash`.

#### Layer 1 — durable `ContentHash → OID` map

Persist the memo across runs, version-stamped.

- **Complexity per commit after first export:** O(changed paths) — walk
  `tree_diff(parent.tree, state.tree)`, materialize only new/changed
  entries, inherit parent OIDs for unchanged subtrees.
- **Invalidation:** version stamp on serializer / fidelity-field changes →
  **regenerate, do not migrate** (same posture as content-hash format
  bumps). A stamp mismatch drops the map and rebuilds from Layer 0.
- **Scope:** trees and blobs (and tag objects once #575 lands). Commits
  continue to flow through the existing `StateId → OID` projection mapping.

#### Layer 2 — materialize bytes ONLY for the wire delta

Given exported-ref manifests (what the remote already has vs what we are
advertising), emit pack bytes solely for OIDs absent on the remote.

- **Push cost:** O(delta bytes + metadata), not O(history).
- **Source of truth for "what we own":** durable Heddle projection state
  (managed-refs / projection mapping), not a bare mirror. In this checkout
  the managed-refs record still names the mirror path
  (`git_core.rs:2766-2797`, `heddle-mirror-managed-refs`); #568 / #1313
  re-sources that frontier from `.heddle/git-projection/` — this spike
  assumes that ownership move and does not re-design it.

#### Layer 3 — OPTIONAL evictable bytes cache

A pure performance cache of already-minted git object bytes.

- **NEVER load-bearing.** Correctness and OID identity must hold with the
  cache absent.
- **Mandatory test:** delete the cache mid-sequence (between two exports /
  pushes) and assert **byte-identical** output and identical advertised
  OIDs. If that test cannot be written, Layer 3 is mis-designed.

### 5.3 Visibility epoch

Embargo / redaction / visibility-tier writes currently force careful
whole-frontier reconcile on export (see the embargo purge discussion in
`git_export.rs:421-498,713-806` and the audience gate at
`git_export.rs:128-139`). After O(delta) export, a naive "skip work if
nothing changed" risks **leaking** a now-embargoed tip that an incremental
path forgot to retract.

**Recommendation:** a store-level **monotonic visibility epoch**, bumped at
every tier / redaction / marker write chokepoint.

| Epoch vs last export | Behavior |
|---|---|
| Unchanged | Skip the whole-repo embargo purge; incremental export only |
| Changed **or any doubt** | Full reconcile (fail expensive, not leaky) |

This is deliberately conservative: the failure mode of a wrong skip is a
visibility leak; the failure mode of a spurious full reconcile is CPU.

---

## 6. The unification (the strategic core)

### 6.1 Same defect, two call sites

`export_tree`'s memo-less full-tree recursion **is the same defect** as
weft#1255 (adopt serializes work proportional to the whole tree per commit).
weft#1255 measured it: ghostty adoption ~29 minutes, `serialization_encoding_ms`
~70 % of hydrate, per-commit cost scaling with commit count (quadratic in
repo scale). The physical model is "each commit re-encodes state proportional
to the whole tree," not "encoding is slow."

### 6.2 One primitive

Heddle already has the shared substrate:

```text
tree_diff(parent_state.tree, state.tree) → changed (path, entry)
```

Implemented as `diff_trees` / `diff_trees_visit` in
`crates/object-model/src/object/tree_diff.rs:129-180` (sorted merge-join,
deterministic order, early-exit visitor). Public re-exports live at
`crates/object-model/src/object/mod.rs:133-134`.

| Consumer | Walks the delta to… |
|---|---|
| **Heddle export** (#1313 / #568 speed) | Materialize only new/changed git objects; inherit OIDs for unchanged `ContentHash`es (Layers 0–2) |
| **Weft adopt encoding** (weft#1255) | Encode only the delta into the projection / pack, not the full tree per commit |
| **Weft pack delta-grouping** (weft#1256) | The changed **path** falls out of the enumerator for free → exactly the `path_hint` `PackBuilder::add_with_path` already accepts (`crates/pack/src/store/pack/pack_builder.rs:61-89`) and that production currently hardcodes to `None` (`:57`) |

So **#1313 / #568** (mirror-drop O(delta)), **weft#1255** (adopt quadratic),
and **weft#1256** (delta-grouping / `path_hint`) consume **one primitive**
instead of three separate fights.

### 6.3 What this is not

- Not a proposal to share process memory between heddle and weft.
- Not a proposal to change the pack wire format.
- Not a re-opening of #593, #606, S7, or #575 field shapes.
- The primitive is the **delta enumerator + OID memo discipline**; each
  consumer keeps its own encoder / pack builder.

### 6.4 Cross-repo ownership

| Layer | Owner | Issue |
|---|---|---|
| `tree_diff` (already shipped) | heddle `object-model` | — |
| Layer-0 memo in `export_tree` | heddle `git-projection` | P0 with/after #1313 |
| Durable `ContentHash → OID` map + visibility epoch | heddle `git-projection` / store | P1 |
| Structured fidelity remaining gaps | heddle object-model + ingest | #593, #606, S7, #575 (P2) |
| Adopt encoding walks delta | weft | weft#1255 (P3) |
| `path_hint` from delta paths | weft (calls heddle `PackBuilder`) | weft#1256 (P3) |

---

## 7. Failure modes

Drawn from the fable design pass; kept as load-bearing risks, not folklore.

### 7.1 Durable OID map is a new trust surface

A wrong `ContentHash → OID` entry that happens to name an OID the remote
already has yields **wrong content** under a valid-looking push (the remote
accepts the OID; the bytes are someone else's).

**Mitigations:**

1. **Version-stamp invalidation** — serializer or fidelity-field change
   regenerates the map; no cross-version migration of entries.
2. **Visibility-epoch / redaction purging** — any redaction or tier write
   that changes exported bytes for a hash **must** drop or rewrite the
   affected map entries (the epoch bump forces full reconcile when in
   doubt).
3. **Map-free conformance path** — a `--no-cache` (or test-only) recompute
   from Layer 0 / full reconstruction is ground truth. CI conformance
   (#562 corpus, #533 round-trip matrix) runs map-free, or runs both and
   diffs OIDs.

### 7.2 Force-push / deep rewrite with a cold cache

Rebuilding O(rewrite) is **inherent**, bounded, and paid once. Do not
pretend Layer 1 makes history mutation free. Document the cost; do not add
a second mirror to paper over it.

### 7.3 Eviction thrash (Layer 3 only)

Only arises if Layer 3 is wrongly made load-bearing (export fails or
diverges when the bytes cache is cold). Guard with the delete-and-assert
test (§5.2). Prefer paying Layer 0 CPU over serving stale bytes.

### 7.4 Identity display collision

#593's `Vec<u8>` avoids the U+FFFD display-collision class entirely.
Display paths use `from_utf8_lossy` at the JSON/UI boundary; the stored and
hashed identity remains exact bytes. An override-based alternative would
re-introduce this failure mode — another reason §2.1 is closed.

### 7.5 Visibility leak under incremental export

Mitigated by the visibility epoch (§5.3): any doubt → full reconcile.
There is no "best-effort skip" of embargo purge.

### 7.6 Residual floor growth after a serializer bug

If a regression makes `OID_round ≠ OID_src` for a previously-faithful class,
the import-time gate (§3) **fail-closes into residual capture** rather than
silently mapping a wrong OID. Floor size is then an observability signal
("why did residual rate jump?") rather than a silent correctness hole.

---

## 8. Phasing

### P0 — with / after #1313 (mirror-drop foundation)

| Work | Outcome |
|---|---|
| **Layer-0 memo** in `export_tree` | Kills the per-export quadratic re-pay on unique trees |
| **Import-time round-trip gate** (§3) | Stops #606-class silent SHA-drift at the source |
| **Corpus measurement** (§4.3) | Prices the residual floor as a number on linux/git/curl/ghostty |

P0 does **not** require the durable map, the format bump, or weft changes.
It is safe to land alongside or immediately after #568 / #1313's
store-sourced export.

### P1 — "push auto-exports in O(delta)"

| Work | Outcome |
|---|---|
| Durable **ContentHash → OID** map (version-stamped) | Cross-run incremental export |
| **Delta pusher** (Layer 2) off exported-ref manifests | Push = O(delta bytes + metadata) |
| **Visibility epoch** | Incremental export without embargo leaks |

### P2 — the #564 format bump (**one migration**)

Batch the remaining structured closures into a **single** content-hash /
on-disk format bump:

- #593 `Principal` → `Vec<u8>`
- #606 nonstandard modes
- S7 tree-entry name fidelity
- #575 annotated tags as first-class objects
- Notes in-store

Then **delete the Bridge Mirror sidecar** as a load-bearing path (residual
floor stays, tiny, in-store). Do not ship five sequential re-hashes.

### P3 — weft consumes the same primitive

| Work | Outcome |
|---|---|
| Thread delta enumerator into adopt encoding | Closes weft#1255 quadratic serialization |
| Feed changed paths as `path_hint` into `PackBuilder::add_with_path` | Closes weft#1256 delta-grouping |
| Optional Layer-3 bytes cache | Only if clone-serving becomes hot; never load-bearing |

---

## 9. Explicit non-goals

- Replacing git's object model with a "better" one.
- Claiming zero residual for adversarial / pathological repos.
- Keeping `.heddle/git` as a performance cache after correctness no longer
  needs it.
- Backwards-compatibility shims for pre-fidelity states beyond the single
  P2 migration (AGENTS.md compatibility rule).
- Re-opening #593 as an override / enum / dual-representation design.
- Implementing any of the above in this docs-only change.

---

## 10. Cross-links for the orchestrator

When this spike is accepted, update or comment on:

| Link | Role |
|---|---|
| HeddleCo/heddle#564 | Parent correctness epic — this spike **extends**, does not replace |
| HeddleCo/heddle#568 | Mirror elimination — speed half becomes P0/P1 of this doc |
| HeddleCo/heddle#1313 | Landing PR for #568 store-sourced export; Layer 0 rides with/after it |
| HeddleCo/heddle#593 | Identities = `Vec<u8>` (do not contradict) |
| HeddleCo/heddle#606 | Nonstandard modes structured field |
| HeddleCo/heddle#575 | Annotated tags first-class |
| HeddleCo/heddle#1321 | This spike's tracking issue |
| HeddleCo/weft#1255 | Adopt quadratic serialization — consumes delta enumerator at P3 |
| HeddleCo/weft#1256 | `path_hint` / delta-grouping — path falls out of the same enumerator |
| `docs/adr/0042-retire-persistent-bridge-mirror.md` | Accepted endgame this spike refines with speed + floor honesty |
| `CONTEXT.md` (Raw Git Object Residual, Bridge Mirror, Git Projection Mapping) | Glossary; residual "in-store" evolution may need a glossary note at P2 |

---

## 11. Anchor verification log (this checkout)

| Claim | Verified? | Anchor |
|---|---|---|
| `export_tree` recursive, no ContentHash memo | Yes | `git_export.rs:232-306` |
| Fidelity mint path / `has_git_fidelity` | Yes | `git_export.rs:141-167` |
| `Principal` is still `String` | Yes | `state_attribution.rs:8-12` |
| Hash folds principal via `.as_bytes()` | Yes | `state_core.rs:672-675,734-737` |
| Fidelity fields on `State` | Yes | `state_core.rs:254-329` |
| `diff_trees` / `diff_trees_visit` exist | Yes | `tree_diff.rs:129-180` |
| Residual store layout + OID verify on put | Yes | `git_residual.rs:1-39,121-157` |
| `100664` currently normalized on import | Yes | `importer.rs:1773-1810` |
| Tree name drop/convert classifier | Yes | `git_tree_name.rs:25-50` |
| `PackBuilder::add_with_path` / default `path_hint: None` | Yes | `pack_builder.rs:47-89` |
| Materialize-time OID equality hard-error | Yes | `git_core.rs:3500-3504` |
| Projection mapping is `StateId ↔ git OID` | Yes | `git_mapping.rs:19-23` (`MappingEntry { state_id, git_oid }`) |
| `materialize_cached_mappings` symbol | **Absent** — proposed Layer-0 name only | — |
| S7 as a filed GitHub issue number | **Unverified** — labeled S7 in #564 decomposition comment only | #564 comment 2026-08-09 |
| Exact ghostty ContentHash→OID map byte size | **Estimated** (~5–8 MB) from entry-size × object count; not measured in this checkout | §4.3 P0 will measure |

---

## 12. One-paragraph summary

#564 already decides how to make git export **correct** without a Bridge
Mirror; this spike decides how to make it **fast** and how to stop fighting
the same full-tree walk three times. Close the remaining structured gaps
(#593 `Vec<u8>` identities — not an override — #606 modes, S7 names, #575
tags, notes in-store), backstop the class with an **import-time
reconstruct-and-compare gate**, keep a **tiny in-store residual floor** for
the irreducible non-canonical tail, and replace the mirror's cache value
with a **version-stamped `ContentHash → OID` map** plus a shared
**`tree_diff` delta enumerator** that heddle export and weft adopt
(weft#1255 / #1256) both consume. Phase it so the quadratic dies at P0, push
becomes O(delta) at P1, the format bump batches once at P2, and weft rides
the same primitive at P3.
