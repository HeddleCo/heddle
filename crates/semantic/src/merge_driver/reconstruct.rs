// SPDX-License-Identifier: Apache-2.0
//! Per-item 3-way merge + file reconstruction.
//!
//! Given three [`FileSegments`] (base / ours / theirs), produce a merged file
//! by:
//!
//! 1. Resolving each *item* (matched by [`ItemKey`]) via 3-way merge of its
//!    bytes, falling through to `heddle-merge::text_hunk_merge` when both
//!    sides modify the same item.
//! 2. Stitching the resolved items back together in *base order*, with
//!    one-side-only added items spliced in at their natural position relative
//!    to neighbours.
//! 3. Weaving the inter-item segments back between the items at their
//!    original positions. Each output gap is the per-segment 3-way merge of
//!    the corresponding source segments on the sides that have the trailing
//!    item; added items bring their adding-side segment with them.

use std::collections::{BTreeMap, BTreeSet};

use merge::{text_hunk_merge_with_markers, ConflictMarkers, MergeOutcome};

use super::items::{segment_file, ContainerSpan, FileSegments, Item, ItemKey, ItemKind};
use crate::parser::{Language, ParsedFile};

/// Three sides of the merge: `[base, ours, theirs]`. Each per-iteration
/// segment contribution is indexed by [`Side`] so emission tracking can
/// say "this side has already contributed range N — don't re-emit".
const N_SIDES: usize = 3;

/// Identity used for cross-side item matching. The bare [`ItemKey`]
/// would collapse repeated declarations that share the same key — e.g.
/// two top-level JavaScript `function foo() {}` statements with
/// identical signatures, or two Python module-level `def foo()` blocks
/// — and `BTreeMap<ItemKey, _>` would silently keep only the LAST
/// occurrence per side. `MatchKey` pairs the key with the item's
/// per-key occurrence index within its side (0 for the first, 1 for
/// the second, …) so each duplicate gets a distinct slot. Matching
/// across sides pairs same-key items positionally — base's first `foo`
/// pairs with ours's first `foo` pairs with theirs's first `foo`.
///
/// Positional occurrence governs only NON-`use` items. `use` items are
/// resolved as whole leaf-components by [`resolve_use_component`] (a set
/// comparison over every declaration each side contributes), so no `use`
/// item's content is ever decided by its occurrence index — the occurrence
/// slot survives for them only to anchor inter-item whitespace weaving
/// (heddle#468 r5: the duplicate-import class the positional path produced).
type MatchKey = (ItemKey, usize);

/// An instance-tagged structural-scope chain: each level pairs a container name
/// with the source-order ordinal of the concrete span it sits in (see
/// [`Item::struct_scope_inst`]).
type InstChain = Vec<(String, usize)>;

/// An item's conservation identity for the heddle#484 output-boundary floor:
/// its [`ItemKey`] plus the instance-tagged container chain it sits in.
type TaggedItem = (ItemKey, InstChain);

/// Stitch three sides together via per-item resolution + inter-item hunk merge.
#[allow(clippy::too_many_arguments)]
pub(crate) fn reconstruct_merged_file(
    base: &str,
    ours: &str,
    theirs: &str,
    base_segments: &FileSegments,
    ours_segments: &FileSegments,
    theirs_segments: &FileSegments,
    language: Language,
    markers: ConflictMarkers<'_>,
) -> MergeOutcome {
    // Per-side match keys walked in source order. Each item gets a
    // (ItemKey, occurrence_within_side) tuple — see [`MatchKey`].
    let base_mks = build_match_keys(base_segments);
    let ours_mks = build_match_keys(ours_segments);
    let theirs_mks = build_match_keys(theirs_segments);

    // Build (match-key -> item) maps per side for matching. Duplicates
    // no longer collide because each occurrence has a distinct
    // `MatchKey`.
    let base_map: BTreeMap<MatchKey, &Item> = base_mks
        .iter()
        .zip(base_segments.items.iter())
        .map(|(mk, i)| (mk.clone(), i))
        .collect();
    let ours_map: BTreeMap<MatchKey, &Item> = ours_mks
        .iter()
        .zip(ours_segments.items.iter())
        .map(|(mk, i)| (mk.clone(), i))
        .collect();
    let theirs_map: BTreeMap<MatchKey, &Item> = theirs_mks
        .iter()
        .zip(theirs_segments.items.iter())
        .map(|(mk, i)| (mk.clone(), i))
        .collect();

    let all_keys: BTreeSet<&MatchKey> = base_map
        .keys()
        .chain(ours_map.keys())
        .chain(theirs_map.keys())
        .collect();

    // Resolve every match-key independently. Each resolution yields
    // either (Some(merged_bytes), conflict_count) or `None` if both
    // sides removed the item.
    let mut resolved: BTreeMap<MatchKey, (Option<Vec<u8>>, usize)> = BTreeMap::new();
    let mut total_conflicts = 0usize;

    // Whole-file source bundle: lets `resolve_item` slice per-item
    // bytes AND carries a whole-file `EolPolicy` used by the trailing
    // newline path (`reconcile_trailing_newline`) and as a fallback by
    // the marker path (`emit_addadd_conflict`) when the conflicting
    // item bodies carry zero EOL observations (Codex r8, cid
    // 3256283857). The marker path otherwise weights its policy on
    // the items' own bytes so a CRLF item in a majority-LF file gets
    // CRLF markers (Codex r2 P2 on PR #193, cid 3291860840).
    let sides = SideSources::new(base, ours, theirs);

    // Non-`use` items: per-item positional resolution, matched by
    // (key, occurrence). `use` items are skipped here — their content is
    // NEVER decided by positional occurrence index (the heddle#468 r5 bug
    // class). They are resolved below as whole leaf-components.
    for key in &all_keys {
        if key.0.kind == ItemKind::Use {
            continue;
        }
        let resolution = resolve_item(
            sides,
            base_map.get(*key).copied(),
            ours_map.get(*key).copied(),
            theirs_map.get(*key).copied(),
            markers,
        );
        total_conflicts += resolution.1;
        resolved.insert((*key).clone(), resolution);
    }

    // `use` items: resolve each canonical leaf-component as ONE set-valued
    // unit. After `canonicalize_use_keys`, every declaration in a component
    // shares one `ItemKey`, so a side may contribute SEVERAL items to it
    // (e.g. base `use a::Bar;` widened by theirs to `use a::{Bar, Baz};`
    // while ours adds a separate `use a::Baz;`). Occurrence-matching those
    // items positionally emitted both the widened group AND the standalone
    // leaf — a duplicate import, no conflict (heddle#468, Codex r5 on PR
    // #477). Comparing full component leaf-SETS instead makes the whole
    // class impossible: the verdict lands on the component's first slot and
    // every later slot emits nothing, so the component is resolved exactly
    // once regardless of how many declarations each side spells it across.
    let mut use_components: BTreeMap<ItemKey, [Vec<&Item>; 3]> = BTreeMap::new();
    for (side, seg) in [base_segments, ours_segments, theirs_segments]
        .iter()
        .enumerate()
    {
        for item in &seg.items {
            if item.key.kind == ItemKind::Use {
                use_components
                    .entry(item.key.clone())
                    .or_insert_with(|| [Vec::new(), Vec::new(), Vec::new()])[side]
                    .push(item);
            }
        }
    }
    for (key, [base_items, ours_items, theirs_items]) in &use_components {
        let (bytes, conflicts) =
            resolve_use_component(sides, base_items, ours_items, theirs_items, markers);
        total_conflicts += conflicts;
        resolved.insert((key.clone(), 0), (bytes, conflicts));
        // Higher-occurrence slots of this component exist only so the
        // inter-item segment weaver can place the surrounding whitespace;
        // they carry no item bytes (the verdict above is the whole unit).
        let slots = base_items.len().max(ours_items.len()).max(theirs_items.len());
        for occ in 1..slots {
            resolved.insert((key.clone(), occ), (None, 0));
        }
    }

    // Instance-annotated structural scope of each match-key — the chain of
    // module/impl/trait/class bodies the item sits inside, with each level
    // tagged by the source-order ordinal of the concrete container span it
    // physically lives in (see [`Item::struct_scope_inst`], assigned from the
    // real parse spans in `items.rs`). Distinct from `ItemKey::scope` (the
    // logical match scope). Base wins when present so matched items keep a
    // stable placement; otherwise the adding side's nesting is used. Both the
    // emit-order grouping AND the brace-weave depth below key on this — never
    // the bare name — so two reopened `impl Foo {}` / `namespace N {}` blocks
    // stay distinct no matter what separates them (nothing, a comment,
    // whitespace, or a top-level item), and a clean merge never collapses or
    // reorders one reopened block across another (heddle#484).
    let mut struct_scope_inst_of: BTreeMap<MatchKey, Vec<(String, usize)>> = BTreeMap::new();
    for (mks, seg) in [
        (&theirs_mks, theirs_segments),
        (&ours_mks, ours_segments),
        (&base_mks, base_segments),
    ] {
        for (mk, item) in mks.iter().zip(seg.items.iter()) {
            struct_scope_inst_of.insert(mk.clone(), item.struct_scope_inst.clone());
        }
    }

    let flat_order = compute_item_emit_order(&base_mks, &ours_mks, &theirs_mks, &all_keys);
    // Re-group so items physically inside the same container *instance* stay
    // contiguous (a valid pre-order over the scope tree). The positional
    // splice in `compute_item_emit_order` can otherwise drop a top-level
    // added item between two children of a module added on the other side,
    // stranding a later child outside the module's `}` (heddle#484 Bug 3).
    // Grouping is keyed by `(scope, instance)` so a scope reopened around an
    // intervening item stays a distinct group — a clean merge never reorders
    // a reopened block across that item (heddle#484 P1). For files with no
    // cross-scope interleaving this is the identity.
    let item_emit_order = group_by_struct_scope(&flat_order, &struct_scope_inst_of);

    // For each side, record each item's index so we can look up the
    // inter-item segment that preceded it in source.
    let side_idx_maps = [
        match_key_index(&base_mks),
        match_key_index(&ours_mks),
        match_key_index(&theirs_mks),
    ];
    let side_ranges = [
        base_segments.inter_item_ranges(),
        ours_segments.inter_item_ranges(),
        theirs_segments.inter_item_ranges(),
    ];
    let side_sources = [base, ours, theirs];
    let side_containers = [
        base_segments.containers.as_slice(),
        ours_segments.containers.as_slice(),
        theirs_segments.containers.as_slice(),
    ];

    // Walk emit_order. For each item, emit:
    //   1. The inter-item segment that PRECEDED it in each side that
    //      HAS the item — that side's range immediately before it. A
    //      side that lacks the item contributes nothing for this slot.
    //   2. The merged item bytes.
    // After the last item, emit the postamble (each side's final range).
    //
    // Every side range maps to exactly one purpose: range `i` (for
    // `i < n_items`) is the preceding gap of the one item it sits
    // before, and the final range (`n_items`) is the postamble. Because
    // a lacking side never borrows a neighbouring range, no range can be
    // pulled into two slots — which is what made the prior "bridging"
    // model emit a one-item side's trailing postamble BOTH as the gap
    // before an added item AND again as the postamble (heddle#484: the
    // duplicated `// MARK` / `mod tests {…}` / closing-brace class).
    let side_n: [usize; N_SIDES] = [
        side_ranges[0].len() - 1,
        side_ranges[1].len() - 1,
        side_ranges[2].len() - 1,
    ];
    let mut output: Vec<u8> = Vec::new();
    let empty_scope: &[(String, usize)] = &[];
    let mut prev_struct: &[(String, usize)] = empty_scope;
    let mut prev_key: Option<&MatchKey> = None;

    for key in item_emit_order.iter() {
        let y_struct = struct_scope_inst_of
            .get(key)
            .map(Vec::as_slice)
            .unwrap_or(empty_scope);
        // Containers the gap before this item should structurally close /
        // open: the depths dropped from / added to the previously-emitted
        // item's scope, relative to the scope they share. A side whose own
        // preceding item sat at a different depth (because the merged
        // predecessor was inserted from another side) carries extra braces
        // in its raw gap; those are trimmed so each `{`/`}` is emitted once.
        //
        // Crucially this depth is over the INSTANCE-tagged scope: a level
        // matches only when both its name and its container-instance ordinal
        // match. So the gap between two reopened same-name containers
        // (`impl Foo {…}` then `impl Foo {…}`) sees `needed_exits ==
        // needed_enters == 1` — closing the first and opening the second —
        // instead of the name-only `0`/`0` that made `trim_redundant_structure`
        // drop both braces and silently collapse the blocks (heddle#484).
        let common = common_prefix_len(prev_struct, y_struct);
        let needed_exits = prev_struct.len() - common;
        let needed_enters = y_struct.len() - common;
        let mut segs: [Option<&str>; N_SIDES] = [None, None, None];
        let mut any_preceding = false;
        for s in 0..N_SIDES {
            if let Some(&r) = side_idx_maps[s].get(key) {
                // `r == 0` is the file preamble, which belongs to the FIRST
                // emitted item only; for a later item it is not a separator,
                // so a side on which this item leads contributes nothing.
                if r > 0 || prev_key.is_none() {
                    segs[s] = Some(trim_redundant_structure(
                        side_sources[s],
                        &side_ranges[s],
                        side_containers[s],
                        r,
                        needed_exits,
                        needed_enters,
                    ));
                    any_preceding |= r > 0;
                }
            }
        }
        // No side offers a real preceding separator — this item leads every
        // side that has it (both sides independently prepended a new item
        // before the first shared item). Source the separator from the
        // merged predecessor's own trailing between-gap (never its
        // postamble), so the two prepended items stay separated rather than
        // being concatenated (heddle#484 regression guard).
        if let Some(pk) = prev_key
            && !any_preceding
        {
            for s in 0..N_SIDES {
                if let Some(&j) = side_idx_maps[s].get(pk)
                    && j + 1 < side_n[s]
                {
                    // Only the leading whitespace: the rest of the
                    // predecessor's trailing gap belongs to ITS real
                    // successor (e.g. a `mod {` header) and is emitted with
                    // that item — pulling it here would drag the following
                    // item into the wrong scope.
                    segs[s] = Some(leading_whitespace(inter_slice(
                        side_sources[s],
                        &side_ranges[s],
                        j + 1,
                    )));
                }
            }
        }
        let (seg_bytes, seg_conflicts) = merge_segment(segs[0], segs[1], segs[2], markers);
        output.extend_from_slice(&seg_bytes);
        total_conflicts += seg_conflicts;

        if let Some((Some(item_bytes), _)) = resolved.get(key) {
            output.extend_from_slice(item_bytes);
        }
        prev_struct = y_struct;
        prev_key = Some(key);
    }

    // Postamble: each side's final range, merged once. Every side has
    // exactly one (its last range) and it is never an item's preceding
    // gap, so there is nothing to dedup against.
    let mut post: [Option<&str>; N_SIDES] = [None, None, None];
    for s in 0..N_SIDES {
        let last = side_ranges[s].len() - 1;
        post[s] = Some(inter_slice(side_sources[s], &side_ranges[s], last));
    }
    let (post_bytes, post_conflicts) = merge_segment(post[0], post[1], post[2], markers);
    output.extend_from_slice(&post_bytes);
    total_conflicts += post_conflicts;

    reconcile_trailing_newline(&mut output, sides);

    if total_conflicts == 0 {
        // Model-independent safety floor (heddle#484). Every prior round
        // (r1/r2/r3) fixed a *model-internal* re-derivation of container-
        // instance identity, and each was defeated by the next file
        // arrangement. This check sits at the OUTPUT boundary and is blind to
        // the instance machinery: re-parse the bytes we are about to return
        // and assert they conserve the item set + container nesting the merge
        // resolved to emit. Any reconstruction defect — present or future —
        // that drops, duplicates, moves, or collapses/splits a container
        // instance fails the check, and we fall back to a textual conflict
        // instead of returning a silently-wrong clean merge. A conflict the
        // user resolves is safe; a silent structural collapse is the P0.
        if output_conserves_structure(
            &output,
            language,
            &item_emit_order,
            &resolved,
            &struct_scope_inst_of,
        ) {
            MergeOutcome::Clean(output)
        } else {
            text_hunk_merge_with_markers(
                base.as_bytes(),
                ours.as_bytes(),
                theirs.as_bytes(),
                markers,
            )
        }
    } else {
        MergeOutcome::Conflicts {
            merged_bytes_with_markers: output,
            conflict_count: total_conflicts,
        }
    }
}

/// Output-boundary safety floor (heddle#484): re-parse the reconstructed
/// `output` and verify it conserves the item set + container nesting the merge
/// resolved to emit. Returns `true` when conservation holds (safe to return
/// `Clean`), `false` when the output dropped, duplicated, moved an item, or
/// collapsed/split a container instance — in which case the caller MUST fall
/// back to a textual conflict.
///
/// # The invariant (precise)
///
/// Let `E` be the *expected emitted set*: every NON-`use` item the merge placed
/// with bytes (`item_emit_order` filtered to keys whose `resolved` entry holds
/// `Some(bytes)`), each tagged by its `ItemKey` and instance-annotated scope
/// chain (`struct_scope_inst_of`). Let `O` be the items found by re-parsing
/// `output` with the SAME extraction the driver uses ([`segment_file`]),
/// likewise NON-`use` and tagged by `(ItemKey, struct_scope_inst)`.
///
/// Conservation holds iff, after [`canonicalize_instance_chains`] normalizes
/// both sides' instance ordinals by first-appearance in output/source order,
/// the `(ItemKey, canonical-chain)` **multisets are equal**:
///
/// * the `ItemKey` component enforces **item-set conservation** — no non-`use`
///   item dropped, duplicated, or moved to a different-named scope;
/// * the canonical-chain component enforces **container / nesting
///   conservation** — two items share a chain iff they physically sit in the
///   same container instance, so a collapse (two instances → one) or split
///   (one → two) changes a chain and breaks multiset equality. This is exactly
///   the r1/r2/r3 class.
///
/// Canonicalization is required because the ordinals in `E` are base-anchored
/// (3-way aligned) while a fresh re-parse numbers instances per-file in source
/// order; only the *partition* of items into instances is invariant, not the
/// absolute ordinal values. Normalizing both by first-appearance makes
/// structurally-identical reconstructions compare equal while any reparenting
/// compares unequal.
///
/// `use` items are EXCLUDED on both sides: their `ItemKey::name` is rekeyed
/// across the three sides by `canonicalize_use_keys` (a 3-way leaf-set union)
/// and cannot be recovered from a single-file re-parse, and a single resolved
/// `use` component can emit several declarations — so the resolved-unit ↔
/// re-parsed-declaration mapping is not 1:1. Their conservation is the
/// separately-hardened set-valued `resolve_use_component` path (heddle#468).
///
/// # Why conservation, not a `has_error` gate, is the trigger
///
/// The re-parse is error-TOLERANT ([`ParsedFile::parse_allow_errors`]): the
/// driver already guarantees all three INPUTS parse cleanly, but a clean
/// reconstruction can carry benign error-recovery noise — e.g. a deleted
/// single-line method leaves a stray `;` empty statement that tree-sitter flags
/// as an error even though every surviving item is present and correctly
/// nested. Gating on `has_error` would conflict that genuinely-clean merge (a
/// false positive). Conservation is the real guarantee: a structural collapse
/// that produces an unparseable file (Bug 2/3's unclosed / stray delimiters)
/// still fails conservation, because tree-sitter's error recovery re-nests or
/// duplicates the swallowed items and the recovered `(key, chain)` multiset no
/// longer matches `E`.
fn output_conserves_structure(
    output: &[u8],
    language: Language,
    item_emit_order: &[MatchKey],
    resolved: &BTreeMap<MatchKey, (Option<Vec<u8>>, usize)>,
    struct_scope_inst_of: &BTreeMap<MatchKey, InstChain>,
) -> bool {
    // Expected emitted set, in output order.
    let mut expected: Vec<TaggedItem> = Vec::new();
    for key in item_emit_order {
        if key.0.kind == ItemKind::Use {
            continue;
        }
        if let Some((Some(_), _)) = resolved.get(key) {
            let chain = struct_scope_inst_of.get(key).cloned().unwrap_or_default();
            expected.push((key.0.clone(), chain));
        }
    }

    // Re-parse the output with the SAME extraction the driver uses, tolerating
    // error nodes (see the doc comment): tree-sitter's recovery still surfaces
    // a structural collapse as a conservation mismatch below. Non-UTF-8 output
    // or a parser that can't be built can't be checked at all — trip the floor.
    let Ok(text) = std::str::from_utf8(output) else {
        return false;
    };
    let Some(parsed) = ParsedFile::parse_allow_errors(text, language) else {
        return false;
    };
    let seg = segment_file(&parsed);
    let mut actual: Vec<TaggedItem> = Vec::new();
    for item in &seg.items {
        if item.key.kind == ItemKind::Use {
            continue;
        }
        actual.push((item.key.clone(), item.struct_scope_inst.clone()));
    }

    let mut e = canonicalize_instance_chains(&expected);
    let mut a = canonicalize_instance_chains(&actual);
    e.sort();
    a.sort();
    e == a
}

/// Renumber the instance ordinals in a sequence of `(key, instance-chain)` by
/// first-appearance within each `(canonical-parent, container-name)` group, in
/// the order the items appear. Two differently-numbered but structurally-
/// identical chains then compare equal: only the *partition* of items into
/// instances (and their order) matters, not the absolute ordinal values, which
/// differ between the base-anchored emit metadata and a fresh re-parse. Each
/// level is canonicalized against its already-canonicalized parent, so the
/// normalization is consistent depth-by-depth.
fn canonicalize_instance_chains(seq: &[TaggedItem]) -> Vec<TaggedItem> {
    // (canonical-parent, name, original-ordinal) -> canonical ordinal.
    let mut remap: BTreeMap<(InstChain, String, usize), usize> = BTreeMap::new();
    // (canonical-parent, name) -> next free canonical ordinal.
    let mut next: BTreeMap<(InstChain, String), usize> = BTreeMap::new();
    let mut out = Vec::with_capacity(seq.len());
    for (key, chain) in seq {
        let mut canon: InstChain = Vec::with_capacity(chain.len());
        for (name, ord) in chain {
            let parent = canon.clone();
            let remap_key = (parent.clone(), name.clone(), *ord);
            let canon_ord = if let Some(&c) = remap.get(&remap_key) {
                c
            } else {
                let slot = next.entry((parent.clone(), name.clone())).or_insert(0);
                let c = *slot;
                *slot += 1;
                remap.insert(remap_key, c);
                c
            };
            canon.push((name.clone(), canon_ord));
        }
        out.push((key.clone(), canon));
    }
    out
}

/// Walk a side's items in source order and tag each with its
/// per-key occurrence index — 0 for the first item with that key, 1
/// for the second, and so on. Length matches `seg.items.len()`.
fn build_match_keys(seg: &FileSegments) -> Vec<MatchKey> {
    let mut counters: BTreeMap<ItemKey, usize> = BTreeMap::new();
    seg.items
        .iter()
        .map(|item| {
            let n = counters.entry(item.key.clone()).or_insert(0);
            let occurrence = *n;
            *n += 1;
            (item.key.clone(), occurrence)
        })
        .collect()
}

fn match_key_index(mks: &[MatchKey]) -> BTreeMap<MatchKey, usize> {
    mks.iter()
        .enumerate()
        .map(|(i, mk)| (mk.clone(), i))
        .collect()
}

fn inter_slice<'a>(source: &'a str, ranges: &[(usize, usize)], idx: usize) -> &'a str {
    let (start, end) = ranges[idx];
    &source[start..end]
}

/// Length of the shared leading prefix of two *instance-tagged* scope paths.
/// A level matches only when BOTH its container name and its source-order
/// instance ordinal match, so two reopened same-name containers share a prefix
/// length of 0 at that level — forcing the gap between them to close one
/// container and open the other rather than be trimmed as redundant (heddle#484:
/// the back-to-back / comment- / whitespace-separated reopens that a name-only
/// prefix silently collapsed).
fn common_prefix_len(a: &[(String, usize)], b: &[(String, usize)]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

/// The leading run of whitespace in `s` (up to the first non-whitespace
/// byte). Used to extract just the blank-line separator from a gap whose
/// remainder belongs to a following item.
fn leading_whitespace(s: &str) -> &str {
    let end = s.find(|c: char| !c.is_whitespace()).unwrap_or(s.len());
    &s[..end]
}

/// The inter-item gap before an item on one side, with any *redundant*
/// leading container braces trimmed off.
///
/// `ranges[range_idx]` is the raw gap (`pred.end .. item.start`). On the
/// originating side it closes every container that ended between this item
/// and its source predecessor, and opens every container the item newly sits
/// inside. But in the merged emit order the predecessor may be a different
/// item — inserted from another side at a different scope — so some of those
/// containers are *already* closed (or already open) in the output.
///
/// `needed_exits` / `needed_enters` are how many closes / opens the gap
/// *should* perform, derived from the merged predecessor's and this item's
/// shared scope depth. Extra leading closing braces (over-closing a scope
/// that is already closed) and extra leading opening braces (re-opening a
/// scope that is already open) are dropped by advancing the gap start past
/// them, so each `{` / `}` is emitted exactly once across the merge
/// (heddle#484 Bug 3 + the add/add-module shape).
fn trim_redundant_structure<'a>(
    source: &'a str,
    ranges: &[(usize, usize)],
    containers: &[ContainerSpan],
    range_idx: usize,
    needed_exits: usize,
    needed_enters: usize,
) -> &'a str {
    let (orig_start, end) = ranges[range_idx];
    let mut start = orig_start;

    // Drop extra leading closing braces: containers that opened before the
    // gap and close inside it (innermost / earliest close first).
    let mut closes: Vec<usize> = containers
        .iter()
        .filter(|c| c.open < orig_start && orig_start < c.close && c.close <= end)
        .map(|c| c.close)
        .collect();
    closes.sort_unstable();
    if closes.len() > needed_exits {
        start = closes[closes.len() - needed_exits - 1];
    }

    // Drop extra leading opening braces: containers that open inside the
    // (already exit-trimmed) gap and still enclose the item (outermost /
    // earliest open first).
    let mut opens: Vec<usize> = containers
        .iter()
        .filter(|c| c.open >= start && c.open < end && c.close > end)
        .map(|c| c.open)
        .collect();
    opens.sort_unstable();
    if opens.len() > needed_enters {
        start = opens[opens.len() - needed_enters - 1] + 1;
    }

    &source[start..end]
}

/// Re-order a flat emit order so items physically nested in the same
/// container *instance* are contiguous, yielding a valid pre-order over the
/// structural scope tree. Items are grouped one level at a time by their
/// instance-tagged scope ([`Item::struct_scope_inst`], derived from the real
/// parse spans in `items.rs`), preserving
/// first-appearance order of groups and of items within a group, then each
/// group recurses one level deeper. Keying on `(name, instance)` rather than
/// the bare name keeps a reopened scope a distinct group, so a clean merge
/// never reorders one reopened block across the item that separates it from
/// the other (heddle#484 P1). For an order whose scopes are already
/// contiguous (every file with no cross-side scope-interleaving) this is the
/// identity.
fn group_by_struct_scope(
    order: &[MatchKey],
    struct_scope_inst_of: &BTreeMap<MatchKey, Vec<(String, usize)>>,
) -> Vec<MatchKey> {
    let empty: &[(String, usize)] = &[];
    let annotated: Vec<(MatchKey, &[(String, usize)])> = order
        .iter()
        .map(|k| {
            (
                k.clone(),
                struct_scope_inst_of
                    .get(k)
                    .map(Vec::as_slice)
                    .unwrap_or(empty),
            )
        })
        .collect();
    group_by_struct_scope_depth(annotated, 0)
}

fn group_by_struct_scope_depth(
    items: Vec<(MatchKey, &[(String, usize)])>,
    depth: usize,
) -> Vec<MatchKey> {
    // A unit at this depth is either a leaf (scope length == depth) or a
    // module group keyed by `scope[depth]` — the `(name, instance)` pair.
    // Units are kept in first-appearance order; a group gathers every item
    // that shares that exact `(name, instance)` regardless of interleaving,
    // then recurses. A different instance of the same name starts a fresh
    // group, so reopened scopes never merge (heddle#484 P1).
    enum Unit<'a> {
        Leaf(MatchKey),
        Group(Vec<(MatchKey, &'a [(String, usize)])>),
    }
    let mut units: Vec<Unit> = Vec::new();
    let mut group_at: BTreeMap<(String, usize), usize> = BTreeMap::new();
    for (key, scope) in items {
        if scope.len() <= depth {
            units.push(Unit::Leaf(key));
        } else {
            let level = scope[depth].clone();
            if let Some(&idx) = group_at.get(&level) {
                if let Unit::Group(v) = &mut units[idx] {
                    v.push((key, scope));
                }
            } else {
                group_at.insert(level, units.len());
                units.push(Unit::Group(vec![(key, scope)]));
            }
        }
    }
    let mut out = Vec::new();
    for unit in units {
        match unit {
            Unit::Leaf(k) => out.push(k),
            Unit::Group(v) => out.extend(group_by_struct_scope_depth(v, depth + 1)),
        }
    }
    out
}

/// 3-way merge a single inter-item segment. Handles "side doesn't have
/// this segment" by promoting the present side(s).
fn merge_segment(
    base: Option<&str>,
    ours: Option<&str>,
    theirs: Option<&str>,
    markers: ConflictMarkers<'_>,
) -> (Vec<u8>, usize) {
    match (base, ours, theirs) {
        (None, None, None) => (Vec::new(), 0),
        (Some(b), Some(o), Some(t)) => materialize_segment(
            text_hunk_merge_with_markers(b.as_bytes(), o.as_bytes(), t.as_bytes(), markers),
            b,
        ),
        // Base has it; one side doesn't carry this segment (item missing
        // there). Treat the missing side as "no change" against base.
        (Some(b), Some(o), None) => materialize_segment(
            text_hunk_merge_with_markers(b.as_bytes(), o.as_bytes(), b.as_bytes(), markers),
            b,
        ),
        (Some(b), None, Some(t)) => materialize_segment(
            text_hunk_merge_with_markers(b.as_bytes(), b.as_bytes(), t.as_bytes(), markers),
            b,
        ),
        (Some(b), None, None) => (b.as_bytes().to_vec(), 0),
        // Added item — only the adding side(s) contribute a segment.
        (None, Some(o), Some(t)) => {
            if o == t {
                (o.as_bytes().to_vec(), 0)
            } else {
                materialize_segment(
                    text_hunk_merge_with_markers(&[], o.as_bytes(), t.as_bytes(), markers),
                    "",
                )
            }
        }
        (None, Some(o), None) => (o.as_bytes().to_vec(), 0),
        (None, None, Some(t)) => (t.as_bytes().to_vec(), 0),
    }
}

fn materialize_segment(outcome: MergeOutcome, fallback: &str) -> (Vec<u8>, usize) {
    match outcome {
        MergeOutcome::Clean(bytes) => (bytes, 0),
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            conflict_count,
        } => (merged_bytes_with_markers, conflict_count),
        // Binary / DeleteVsModify shouldn't fire on a text subset, but
        // carry through with base bytes rather than nothing.
        MergeOutcome::Binary | MergeOutcome::DeleteVsModify => (fallback.as_bytes().to_vec(), 0),
    }
}

/// Whole-file source bundle threaded through item resolution. Lets
/// `resolve_item` slice item bytes per side AND carries the
/// whole-file `EolPolicy` used by `reconcile_trailing_newline` and as
/// the zero-observation fallback by `emit_addadd_conflict`. The
/// marker path's primary policy is per-item — see
/// `emit_addadd_conflict` — but reuses this whole-file policy when
/// the conflicting items contribute no `\n` of their own
/// (single-line items, Codex r8 cid 3256283857).
#[derive(Clone, Copy)]
struct SideSources<'a> {
    base: &'a str,
    ours: &'a str,
    theirs: &'a str,
    eol_policy: EolPolicy,
}

impl<'a> SideSources<'a> {
    fn new(base: &'a str, ours: &'a str, theirs: &'a str) -> Self {
        let eol_policy = EolPolicy::detect(&[base.as_bytes(), ours.as_bytes(), theirs.as_bytes()]);
        SideSources {
            base,
            ours,
            theirs,
            eol_policy,
        }
    }
}

/// Dominant line-ending across a set of byte samples. Built via
/// [`EolPolicy::detect`] once over the whole-file sources (see
/// [`SideSources::new`]) and reused everywhere downstream that needs
/// to emit a newline. Counts `\r\n` occurrences vs bare `\n` (LF not
/// preceded by CR); the strict majority wins, and ties fall back to
/// the first sample's own dominant style — by convention callers pass
/// `base` first — then to LF.
///
/// Earlier revisions returned CRLF as soon as ANY sample contained
/// one `\r\n`; that wrongly flipped a majority-LF file to CRLF when a
/// single side happened to be CRLF (Codex r7 P2, cid 3256225712).
/// Majority voting respects the file's actual style without
/// overweighting a single divergent side.
#[derive(Clone, Copy)]
struct EolPolicy {
    crlf: usize,
    lf: usize,
    first_crlf: usize,
    first_lf: usize,
}

impl EolPolicy {
    fn detect(samples: &[&[u8]]) -> Self {
        let mut crlf = 0usize;
        let mut lf = 0usize;
        let mut first_crlf = 0usize;
        let mut first_lf = 0usize;
        for (i, s) in samples.iter().enumerate() {
            let (c, l) = count_eols(s);
            crlf += c;
            lf += l;
            if i == 0 {
                first_crlf = c;
                first_lf = l;
            }
        }
        EolPolicy {
            crlf,
            lf,
            first_crlf,
            first_lf,
        }
    }

    fn eol(self) -> &'static [u8] {
        if self.crlf > self.lf {
            return b"\r\n";
        }
        if self.lf > self.crlf {
            return b"\n";
        }
        if self.first_crlf > self.first_lf {
            return b"\r\n";
        }
        b"\n"
    }
}

/// Resolve a single item's 3-way merge. Returns `(Some(bytes), n_conflicts)`
/// when the item survives, `(None, n_conflicts)` when both sides removed
/// it.
fn resolve_item(
    sides: SideSources<'_>,
    base_item: Option<&Item>,
    ours_item: Option<&Item>,
    theirs_item: Option<&Item>,
    markers: ConflictMarkers<'_>,
) -> (Option<Vec<u8>>, usize) {
    let base_bytes = base_item.map(|i| &sides.base.as_bytes()[i.start_byte..i.end_byte]);
    let ours_bytes = ours_item.map(|i| &sides.ours.as_bytes()[i.start_byte..i.end_byte]);
    let theirs_bytes = theirs_item.map(|i| &sides.theirs.as_bytes()[i.start_byte..i.end_byte]);

    match (base_bytes, ours_bytes, theirs_bytes) {
        (None, None, None) => (None, 0),
        // Added on one side only — take it.
        (None, Some(o), None) => (Some(o.to_vec()), 0),
        (None, None, Some(t)) => (Some(t.to_vec()), 0),
        // Both sides added the same item. Clean only if bytes match.
        // For diverging add-add we MUST surface a conflict directly rather
        // than delegating to text_hunk_merge — the engine's "same-anchor
        // insertion" path concatenates both insertions, which produces a
        // syntactically invalid file when both sides added a function /
        // method with the same name. heddle#68 calls this out as a conflict.
        (None, Some(o), Some(t)) => {
            // The ONLY clean add/add is byte-identical. `use` items never
            // reach this arm — they are resolved as whole leaf-components by
            // `resolve_use_component` (set-valued, not positional). This arm
            // now governs only non-`use` items (e.g. two top-level functions
            // with the same name added on both sides): byte-identical → dedup,
            // anything else → conflict (heddle#68).
            if o == t {
                (Some(o.to_vec()), 0)
            } else {
                (Some(emit_addadd_conflict(o, t, markers, sides)), 1)
            }
        }
        // Existed in base, removed on both sides → clean delete.
        (Some(_), None, None) => (None, 0),
        // Modify/delete: clean delete when the modifying side preserved
        // base; conflict otherwise.
        (Some(b), Some(o), None) => {
            if b == o {
                (None, 0)
            } else {
                // Encode the modify-vs-delete conflict as a synthetic
                // 3-way merge where the deleting side is empty.
                let outcome = text_hunk_merge_with_markers(b, o, &[], markers);
                materialize_outcome(outcome)
            }
        }
        (Some(b), None, Some(t)) => {
            if b == t {
                (None, 0)
            } else {
                let outcome = text_hunk_merge_with_markers(b, &[], t, markers);
                materialize_outcome(outcome)
            }
        }
        // 3-way modify.
        (Some(b), Some(o), Some(t)) => {
            if o == b {
                (Some(t.to_vec()), 0)
            } else if t == b || o == t {
                (Some(o.to_vec()), 0)
            } else {
                let outcome = text_hunk_merge_with_markers(b, o, t, markers);
                materialize_outcome(outcome)
            }
        }
    }
}

fn materialize_outcome(outcome: MergeOutcome) -> (Option<Vec<u8>>, usize) {
    match outcome {
        MergeOutcome::Clean(bytes) => (Some(bytes), 0),
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            conflict_count,
        } => (Some(merged_bytes_with_markers), conflict_count),
        // Binary / DeleteVsModify shouldn't fire on UTF-8 source we already
        // parsed, but carry through safely.
        MergeOutcome::Binary | MergeOutcome::DeleteVsModify => (None, 1),
    }
}

/// Resolve one canonical leaf-component of `use` items as a single
/// set-valued unit. `base_items` / `ours_items` / `theirs_items` are every
/// declaration each side contributes to the component, in source order;
/// any of them may be empty (component absent on that side) or hold more
/// than one declaration (the heddle#468 r5 base-widened-grouped shape).
///
/// The component's text on a side is the byte-exact concatenation of its
/// declarations (one EOL between consecutive lines). The 3-way verdict is
/// taken over those WHOLE-component texts — never per declaration by
/// positional occurrence — and reduces to exactly three outcomes:
///
/// * **one side left the component byte-identical to base** → take the
///   other side (the standard "unchanged side defers" rule, generalized
///   from [`resolve_item`]'s 3-way-modify arm to the set);
/// * **both sides produced byte-identical text** → dedup to one copy;
/// * **everything else** — a widened/regrouped base item, divergent
///   additions, alias / `cfg` / visibility drift, or any multi-occurrence
///   ambiguity within the component → **conflict** the whole component as
///   one `<<<<<<< / ======= / >>>>>>>` block.
///
/// Because the comparison is over complete leaf-SETS rather than
/// occurrence positions, the r5 class (base `use a::Bar;`; ours adds a
/// separate `use a::Baz;`; theirs widens to `use a::{Bar, Baz};`) lands in
/// the conflict outcome instead of silently emitting both `{Bar, Baz}` and
/// `Baz` — a duplicate import (Rust E0252). No future regroup / widen /
/// multi-occurrence shape can drip the same way.
fn resolve_use_component(
    sides: SideSources<'_>,
    base_items: &[&Item],
    ours_items: &[&Item],
    theirs_items: &[&Item],
    markers: ConflictMarkers<'_>,
) -> (Option<Vec<u8>>, usize) {
    let eol = sides.eol_policy.eol();
    let base_bytes = join_component(base_items, sides.base, eol);
    let ours_bytes = join_component(ours_items, sides.ours, eol);
    let theirs_bytes = join_component(theirs_items, sides.theirs, eol);

    let non_empty = |v: Vec<u8>| if v.is_empty() { None } else { Some(v) };

    if ours_bytes == base_bytes {
        // ours left the component untouched → take theirs (which may be a
        // clean delete when theirs is empty).
        return (non_empty(theirs_bytes), 0);
    }
    if theirs_bytes == base_bytes || ours_bytes == theirs_bytes {
        // theirs left it untouched → take ours; or both sides made the
        // byte-identical change → dedup to a single copy.
        return (non_empty(ours_bytes), 0);
    }
    // Both sides changed the component, differently. Conflict the whole
    // unit — see the outcome list above.
    (
        Some(emit_addadd_conflict(&ours_bytes, &theirs_bytes, markers, sides)),
        1,
    )
}

/// Concatenate a `use` component's declarations into one byte-exact text,
/// separating consecutive lines with `eol`. A single declaration yields its
/// own bytes verbatim (so single-occurrence components compare and emit
/// exactly as [`resolve_item`] did before the set-valued path existed).
fn join_component(items: &[&Item], source: &str, eol: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.extend_from_slice(eol);
        }
        out.extend_from_slice(&source.as_bytes()[item.start_byte..item.end_byte]);
    }
    out
}

/// Determine the order items should appear in the output. Strategy:
///
/// 1. Start with base's order.
/// 2. Items missing in base (added on a side) are spliced in after their
///    "left neighbour" in their originating side. The left neighbour is
///    found by walking left from the added item until a key common with
///    base is reached.
/// 3. If both ours and theirs added the same key independently and base
///    doesn't have it, we use ours's neighbour; ties go to ours.
fn compute_item_emit_order(
    base_mks: &[MatchKey],
    ours_mks: &[MatchKey],
    theirs_mks: &[MatchKey],
    all_keys: &BTreeSet<&MatchKey>,
) -> Vec<MatchKey> {
    let mut order: Vec<MatchKey> = base_mks.to_vec();

    for side_mks in [ours_mks, theirs_mks] {
        for (idx, key) in side_mks.iter().enumerate() {
            if order.contains(key) {
                continue;
            }
            let mut insert_at = 0usize;
            for i in (0..idx).rev() {
                if let Some(pos) = order.iter().position(|k| *k == side_mks[i]) {
                    insert_at = pos + 1;
                    break;
                }
            }
            order.insert(insert_at, key.clone());
        }
    }

    order.into_iter().filter(|k| all_keys.contains(k)).collect()
}

/// Append `eol` to `out` unless `out` already ends with a `\n` (which
/// covers both LF and CRLF terminations). Used to keep conflict-marker
/// blocks well-formed when a body doesn't end with its own newline.
fn ensure_trailing_newline(out: &mut Vec<u8>, eol: &[u8]) {
    if !out.is_empty() && *out.last().unwrap() != b'\n' {
        out.extend_from_slice(eol);
    }
}

/// Count (`\r\n`, bare `\n`) occurrences in `s`. A `\n` is "bare" iff
/// it is not preceded by `\r`.
fn count_eols(s: &[u8]) -> (usize, usize) {
    let mut crlf = 0usize;
    let mut lf = 0usize;
    let mut prev = 0u8;
    for &b in s {
        if b == b'\n' {
            if prev == b'\r' {
                crlf += 1;
            } else {
                lf += 1;
            }
        }
        prev = b;
    }
    (crlf, lf)
}

/// Match the trailing-newline state of `output` to the majority of the
/// three input sides. `text_hunk_merge` preserves whatever its line
/// splitter sees on the last line; the semantic path used to force a
/// trailing `\n` unconditionally, which dirtied files that ended
/// without one on every side (Codex r3 P2 #2).
///
/// Rule: count how many of `base`, `ours`, `theirs` end with `\n`. If
/// the majority do, ensure output ends with `\n`; otherwise strip any
/// `\n` we may have inherited from a single side's content. Empty
/// inputs are not counted (they have no opinion on trailing-newline
/// state).
///
/// CRLF is treated as a single unit on BOTH the pop and push paths:
/// when popping a trailing `\n`, an immediately-preceding `\r` is
/// popped along with it (Codex r5 P1 #4); when pushing a trailing
/// newline back, the dominant EOL of the inputs is pushed so a
/// CRLF-canonical file doesn't gain a bare LF (heddle#114 r7 self-
/// audit prediction P1, same hazard class as the r6 P2 #1 markers
/// finding).
fn reconcile_trailing_newline(out: &mut Vec<u8>, sides: SideSources<'_>) {
    if out.is_empty() {
        return;
    }
    let want_newline = majority_ends_with_newline(sides.base, sides.ours, sides.theirs);
    let has_newline = *out.last().unwrap() == b'\n';
    match (want_newline, has_newline) {
        (true, false) => {
            out.extend_from_slice(sides.eol_policy.eol());
        }
        (false, true) => {
            out.pop();
            if out.last() == Some(&b'\r') {
                out.pop();
            }
        }
        _ => {}
    }
}

fn majority_ends_with_newline(base: &str, ours: &str, theirs: &str) -> bool {
    let mut with = 0u8;
    let mut total = 0u8;
    for s in [base, ours, theirs] {
        if s.is_empty() {
            continue;
        }
        total += 1;
        if s.as_bytes().last() == Some(&b'\n') {
            with += 1;
        }
    }
    // Default to "yes" when nothing has an opinion (all sides empty —
    // unreachable in practice since we'd have returned Clean(empty)
    // before reconstruction), and require strict majority otherwise.
    total == 0 || with * 2 > total
}

/// Emit a `<<<<<<< / ======= / >>>>>>>` conflict block wrapping two
/// insertion bodies. Mirrors the marker shape `heddle-merge::markers`
/// produces so external validators (heddle#78) and IDE conflict tools
/// parse it identically.
///
/// Line endings on the marker lines come from a per-item [`EolPolicy`]
/// computed over the two conflicting item bodies, NOT the whole-file
/// policy carried by `sides`. The markers and the body they bracket
/// are derived from the same sample, so the r8 invariant (markers +
/// body cannot disagree) holds — but they now reflect the item's own
/// EOL discipline rather than the surrounding file. In a mixed-EOL
/// file where the LF context outnumbers a CRLF item, the whole-file
/// policy would vote LF and wrap a CRLF body with bare-LF markers,
/// reintroducing the mixed-EOL hunk shape (Codex r2 P2, PR #193 cid
/// 3291860840).
///
/// When both items carry zero EOL observations — single-line bodies
/// in a CRLF file — the per-item policy ties to LF by default, which
/// reintroduces Codex r8 P2 (cid 3256283857). The whole-file
/// `sides.eol_policy` fills that case: it counts the surrounding
/// file context, so a CRLF file resolves to CRLF markers even when
/// the items contribute no observations of their own.
fn emit_addadd_conflict(
    ours: &[u8],
    theirs: &[u8],
    markers: ConflictMarkers<'_>,
    sides: SideSources<'_>,
) -> Vec<u8> {
    let items_policy = EolPolicy::detect(&[ours, theirs]);
    let eol = if items_policy.crlf + items_policy.lf > 0 {
        items_policy.eol()
    } else {
        sides.eol_policy.eol()
    };
    let mut out = Vec::with_capacity(ours.len() + theirs.len() + 64);
    out.extend_from_slice(b"<<<<<<< ");
    out.extend_from_slice(markers.ours.as_bytes());
    out.extend_from_slice(eol);
    out.extend_from_slice(ours);
    ensure_trailing_newline(&mut out, eol);
    out.extend_from_slice(b"=======");
    out.extend_from_slice(eol);
    out.extend_from_slice(theirs);
    ensure_trailing_newline(&mut out, eol);
    out.extend_from_slice(b">>>>>>> ");
    out.extend_from_slice(markers.theirs.as_bytes());
    out.extend_from_slice(eol);
    out
}

#[cfg(test)]
mod floor_tests {
    //! Unit tests for the heddle#484 output-boundary safety floor
    //! ([`output_conserves_structure`]). They exercise the floor directly with
    //! a crafted "emit plan" vs a chosen output byte string, so a deliberate
    //! reconstruction fault (collapse / drop / unparseable) can be injected
    //! without needing the model to actually regress.
    use super::*;

    /// Build the `(item_emit_order, resolved, struct_scope_inst_of)` triple the
    /// floor consumes, treating every item in `src` as if the merge placed it
    /// verbatim (the shape of a faithful reconstruction whose output IS `src`).
    #[allow(clippy::type_complexity)]
    fn plan(
        src: &str,
        language: Language,
    ) -> (
        Vec<MatchKey>,
        BTreeMap<MatchKey, (Option<Vec<u8>>, usize)>,
        BTreeMap<MatchKey, InstChain>,
    ) {
        let parsed = ParsedFile::parse(src, language).expect("plan source must parse");
        let seg = segment_file(&parsed);
        let mks = build_match_keys(&seg);
        let mut order = Vec::new();
        let mut resolved: BTreeMap<MatchKey, (Option<Vec<u8>>, usize)> = BTreeMap::new();
        let mut sinst: BTreeMap<MatchKey, InstChain> = BTreeMap::new();
        for (mk, item) in mks.iter().zip(seg.items.iter()) {
            order.push(mk.clone());
            let bytes = src.as_bytes()[item.start_byte..item.end_byte].to_vec();
            resolved.insert(mk.clone(), (Some(bytes), 0));
            sinst.insert(mk.clone(), item.struct_scope_inst.clone());
        }
        (order, resolved, sinst)
    }

    #[test]
    fn floor_is_noop_on_faithful_output() {
        // The output byte-for-byte matches the plan: conservation must hold so
        // a correct merge stays Clean (no false positive).
        let src = "impl Foo {\n    fn a() {}\n}\nimpl Foo {\n    fn b() {}\n}\n";
        let (order, resolved, sinst) = plan(src, Language::Rust);
        assert!(output_conserves_structure(
            src.as_bytes(),
            Language::Rust,
            &order,
            &resolved,
            &sinst
        ));
    }

    #[test]
    fn floor_is_noop_on_faithful_output_with_top_level_between_reopens() {
        // A top-level item separates two reopened `impl Foo` blocks — the
        // heddle#484 P1 shape. A faithful reconstruction conserves it.
        let src = "impl Foo {\n    fn a() {}\n}\nfn x() {}\nimpl Foo {\n    fn b() {}\n}\n";
        let (order, resolved, sinst) = plan(src, Language::Rust);
        assert!(output_conserves_structure(
            src.as_bytes(),
            Language::Rust,
            &order,
            &resolved,
            &sinst
        ));
    }

    #[test]
    fn floor_catches_container_collapse() {
        // The plan resolves TWO distinct `impl Foo` instances (a in #0, b in
        // #1). A regression collapses them into a single `impl Foo` holding
        // both methods. The canonical instance chains differ (b moves from
        // Foo#1 to Foo#0), so conservation fails → the caller falls back to a
        // textual conflict instead of returning the silently-collapsed merge.
        let src = "impl Foo {\n    fn a() {}\n}\nimpl Foo {\n    fn b() {}\n}\n";
        let collapsed = "impl Foo {\n    fn a() {}\n    fn b() {}\n}\n";
        let (order, resolved, sinst) = plan(src, Language::Rust);
        assert!(!output_conserves_structure(
            collapsed.as_bytes(),
            Language::Rust,
            &order,
            &resolved,
            &sinst
        ));
    }

    #[test]
    fn floor_catches_dropped_item() {
        let src = "fn a() {}\nfn b() {}\n";
        let dropped = "fn a() {}\n";
        let (order, resolved, sinst) = plan(src, Language::Rust);
        assert!(!output_conserves_structure(
            dropped.as_bytes(),
            Language::Rust,
            &order,
            &resolved,
            &sinst
        ));
    }

    #[test]
    fn floor_catches_duplicated_item() {
        let src = "fn a() {}\n";
        let duplicated = "fn a() {}\nfn a() {}\n";
        let (order, resolved, sinst) = plan(src, Language::Rust);
        assert!(!output_conserves_structure(
            duplicated.as_bytes(),
            Language::Rust,
            &order,
            &resolved,
            &sinst
        ));
    }

    #[test]
    fn floor_catches_item_moved_to_different_scope() {
        // Plan: `fn b` sits inside `impl Foo`. Output strands it at top level.
        let src = "impl Foo {\n    fn b() {}\n}\n";
        let moved = "impl Foo {\n}\nfn b() {}\n";
        let (order, resolved, sinst) = plan(src, Language::Rust);
        assert!(!output_conserves_structure(
            moved.as_bytes(),
            Language::Rust,
            &order,
            &resolved,
            &sinst
        ));
    }

    #[test]
    fn floor_catches_unparseable_collapse() {
        // An unparseable output that ALSO collapses structure: the closing `}`
        // of the first `impl Foo` is missing, so both methods land in one impl.
        // Error-tolerant re-parse recovers the items; the recovered chains
        // (b moved from Foo#1 to Foo#0) break conservation. This is the Bug 2/3
        // shape (unclosed delimiter) caught via conservation, not a has_error
        // gate.
        let src = "impl Foo {\n    fn a() {}\n}\nimpl Foo {\n    fn b() {}\n}\n";
        let unclosed = "impl Foo {\n    fn a() {}\n    fn b() {}\n";
        let (order, resolved, sinst) = plan(src, Language::Rust);
        assert!(!output_conserves_structure(
            unclosed.as_bytes(),
            Language::Rust,
            &order,
            &resolved,
            &sinst
        ));
    }
}
